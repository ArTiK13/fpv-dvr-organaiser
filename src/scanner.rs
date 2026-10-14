use crate::{
    classifier::{classify, Device, DeviceType, Pilot, VideoSystem},
    config::Config,
    db, media,
};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::{
    collections::HashMap,
    path::{Component, Path},
    sync::Arc,
};
use tokio::{io::AsyncReadExt, time::sleep};
use tracing::warn;
use walkdir::WalkDir;
#[derive(Deserialize)]
pub struct Receipt {
    pub relative_path: String,
    pub size_bytes: u64,
    pub sha256: String,
}
pub fn safe_relative(s: &str) -> bool {
    !s.is_empty()
        && Path::new(s)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !s.split('/').any(|p| p.is_empty() || p == "." || p == "..")
}
pub async fn run_loop(pool: PgPool, c: Arc<Config>) {
    loop {
        if let Err(e) = scan_once(&pool, &c).await {
            warn!(error=%c.redact(&e.to_string()),"scan failed");
            let _ = crate::notifications::alert(
                &pool,
                "scanner",
                &format!("Ошибка сканирования: {}", c.redact(&e.to_string())),
            )
            .await;
        }
        sleep(c.scan_interval).await;
    }
}
pub async fn scan_once(pool: &PgPool, c: &Config) -> Result<u64> {
    let mut lock = pool.begin().await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(76120301)")
        .fetch_one(&mut *lock)
        .await?;
    if !acquired {
        bail!("Сканирование уже идёт");
    }
    let root = c
        .video_root
        .canonicalize()
        .context("video root unavailable")?;
    if !root.is_dir() {
        bail!("VIDEO_ROOT is not a directory");
    }
    let started = Utc::now();
    let receipts = receipts(&root)?;
    let (devices, systems, pilots) = references(pool).await?;
    let default = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT value FROM settings WHERE key='default_user_id'",
    )
    .fetch_optional(pool)
    .await?
    .map(|v| v.as_i64())
    .unwrap_or(c.default_user_id);
    let revision = hex::encode(Sha256::digest(
        format!("{devices:?}{systems:?}{pilots:?}{default:?}").as_bytes(),
    ));
    let rows:Vec<Existing>=sqlx::query_as("SELECT id,relative_path,size_bytes,mtime_ns,state,classification_revision,event_id FROM videos").fetch_all(pool).await?;
    let catalog: HashMap<String, Existing> = rows
        .into_iter()
        .map(|v| (v.relative_path.clone(), v))
        .collect();
    let mut seen = vec![];
    let entries = WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !ignored(e.path()));
    let mut count = 0;
    for entry in entries {
        let e = entry.context("walk video root")?;
        if !e.file_type().is_file() {
            continue;
        }
        let Some(path) = e.path().strip_prefix(&root)?.to_str().map(str::to_owned) else {
            continue;
        };
        if !safe_relative(&path) {
            continue;
        }
        let m = tokio::fs::metadata(e.path()).await?;
        let size = m.len() as i64;
        let mtime = mtime(&m)?;
        let existing = catalog.get(&path);
        if let Some(v) = existing {
            seen.push(v.id);
            if seen.len() >= 1000 {
                mark_seen(pool, &seen).await?;
                seen.clear();
            }
            if v.size_bytes != size || v.mtime_ns != mtime {
                sqlx::query("UPDATE videos SET state='conflict' WHERE id=$1")
                    .bind(v.id)
                    .execute(pool)
                    .await?;
                issue(pool, v.id, "file_changed").await?;
                continue;
            }
            if v.state == "conflict" {
                continue;
            }
            if v.classification_revision.as_deref() == Some(&revision) && v.state != "missing" {
                count += 1;
                continue;
            }
        }
        if existing.is_none() {
            if c.require_receipts && !receipts.contains_key(&path) {
                continue;
            }
            if !c.require_receipts {
                let stable:bool=sqlx::query_scalar("INSERT INTO file_observations(relative_path,size_bytes,mtime_ns) VALUES($1,$2,$3) ON CONFLICT(relative_path) DO UPDATE SET stable_since=CASE WHEN file_observations.size_bytes=EXCLUDED.size_bytes AND file_observations.mtime_ns=EXCLUDED.mtime_ns THEN file_observations.stable_since ELSE now() END,size_bytes=EXCLUDED.size_bytes,mtime_ns=EXCLUDED.mtime_ns RETURNING stable_since<=now()-($4 * interval '1 second')").bind(&path).bind(size).bind(mtime).bind(c.stable_seconds as f64).fetch_one(pool).await?;
                if !stable {
                    continue;
                }
            }
        }
        let class = classify(&path, &devices, &systems, &pilots, default);
        let (id, event) = if let Some(v) = existing {
            (v.id, v.event_id)
        } else {
            let sha = if let Some(r) = receipts.get(&path) {
                let sha = hash(e.path()).await?;
                if r.size_bytes != m.len() || r.sha256.to_lowercase() != sha {
                    crate::notifications::alert(
                        pool,
                        &format!("receipt:{path}"),
                        &format!("SHA-256 не совпал: {path}"),
                    )
                    .await?;
                    continue;
                }
                Some(sha)
            } else {
                None
            };
            let p = match media::probe(e.path()).await {
                Ok(p) => p,
                Err(err) => {
                    if is_video_extension(e.path()) {
                        sqlx::query("INSERT INTO media_errors(relative_path,error) VALUES($1,$2) ON CONFLICT(relative_path) DO UPDATE SET error=EXCLUDED.error,resolved_at=NULL").bind(&path).bind(err.to_string()).execute(pool).await?;
                        crate::notifications::alert(
                            pool,
                            &format!("media:{path}"),
                            &format!("Ошибка ffprobe: {path}"),
                        )
                        .await?;
                    }
                    continue;
                }
            };
            let after = tokio::fs::metadata(e.path()).await?;
            if after.len() != m.len() || crate::scanner::mtime(&after)? != mtime {
                continue;
            }
            let date = class.event_date.as_ref();
            let event:i64=sqlx::query_scalar("INSERT INTO events(name,date_start,date_end,date_precision) VALUES($1,$2,$3,$4) ON CONFLICT(name) DO UPDATE SET name=EXCLUDED.name RETURNING id").bind(&class.event_name).bind(date.map(|d|d.start)).bind(date.map(|d|d.end)).bind(date.map(|d|&d.precision)).fetch_one(pool).await?;
            let stream = p
                .streams
                .iter()
                .find(|s| s.codec_type.as_deref() == Some("video"))
                .unwrap();
            let duration = p
                .format
                .duration
                .as_ref()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|d| d.is_finite() && *d >= 0.0)
                .map(|d| (d * 1000.0) as i64);
            let id:i64=sqlx::query_scalar("INSERT INTO videos(relative_path,size_bytes,mtime_ns,sha256,event_id,duration_ms,width,height,fps) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id").bind(&path).bind(size).bind(mtime).bind(sha).bind(event).bind(duration).bind(stream.width).bind(stream.height).bind(stream.avg_frame_rate.as_ref().filter(|s|*s!="0/0").or(stream.r_frame_rate.as_ref())).fetch_one(pool).await?;
            sqlx::query("UPDATE media_errors SET resolved_at=now() WHERE relative_path=$1")
                .bind(&path)
                .execute(pool)
                .await?;
            (id, event)
        };
        let mut tx = pool.begin().await?;
        let _ = event;
        sqlx::query("UPDATE videos SET device_id=$2,video_system_id=$3,pilot_id=$4,colored=$5,state=$6,first_classified_at=CASE WHEN $6='classified' THEN coalesce(first_classified_at,now()) ELSE first_classified_at END,classification_revision=$7 WHERE id=$1").bind(id).bind(class.device_id).bind(class.video_system_id).bind(class.pilot_id).bind(class.colored).bind(if class.issues.is_empty(){"classified"}else{"unclassified"}).bind(&revision).execute(&mut *tx).await?;
        sqlx::query("UPDATE classification_issues SET resolved_at=now() WHERE video_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        for i in &class.issues {
            issue(pool, id, i).await?;
        }
        count += 1;
    }
    mark_seen(pool, &seen).await?;
    sqlx::query("UPDATE videos SET state='missing' WHERE last_seen_at<$1 AND state<>'missing'")
        .bind(started)
        .execute(pool)
        .await?;
    db::invalidate_links(pool).await?;
    sqlx::query("UPDATE system_alerts SET active=false WHERE key='scanner'")
        .execute(pool)
        .await?;
    lock.commit().await?;
    Ok(count)
}
#[derive(sqlx::FromRow)]
struct Existing {
    id: i64,
    relative_path: String,
    size_bytes: i64,
    mtime_ns: i64,
    state: String,
    classification_revision: Option<String>,
    event_id: i64,
}
async fn mark_seen(pool: &PgPool, ids: &[i64]) -> Result<()> {
    sqlx::query("UPDATE videos SET last_seen_at=now() WHERE id=ANY($1)")
        .bind(ids)
        .execute(pool)
        .await?;
    Ok(())
}
async fn issue(pool: &PgPool, id: i64, code: &str) -> Result<()> {
    sqlx::query("INSERT INTO classification_issues(video_id,issue_code) VALUES($1,$2) ON CONFLICT(video_id,issue_code) DO UPDATE SET resolved_at=NULL").bind(id).bind(code).execute(pool).await?;
    crate::notifications::alert(
        pool,
        &format!("classification:{id}:{code}"),
        &format!("Видео #{id}: {code}"),
    )
    .await
}
fn ignored(p: &Path) -> bool {
    p.file_name().is_some_and(|v| {
        let s = v.to_string_lossy();
        [".video-sync", ".rsync-partial", ".DS_Store", ".immich"].contains(&s.as_ref())
            || s.starts_with("._")
    })
}
fn is_video_extension(p: &Path) -> bool {
    p.extension().is_some_and(|v| {
        ["mp4", "mov", "mkv", "avi", "webm", "m4v", "mts", "m2ts"]
            .contains(&v.to_string_lossy().to_lowercase().as_str())
    })
}
fn mtime(m: &std::fs::Metadata) -> Result<i64> {
    Ok(m.modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos()
        .try_into()?)
}
async fn hash(p: &Path) -> Result<String> {
    let mut f = tokio::fs::File::open(p).await?;
    let mut h = Sha256::new();
    let mut b = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut b).await?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(hex::encode(h.finalize()))
}
fn receipts(root: &Path) -> Result<HashMap<String, Receipt>> {
    let dir = root.join(".video-sync/receipts");
    let mut out = HashMap::new();
    if !dir.exists() {
        return Ok(out);
    }
    for e in WalkDir::new(dir).follow_links(false) {
        let e = e?;
        if e.file_type().is_file() && e.path().extension().is_some_and(|v| v == "json") {
            let r: Receipt = serde_json::from_slice(&std::fs::read(e.path())?)
                .context("invalid sync receipt")?;
            if !safe_relative(&r.relative_path) {
                bail!("unsafe receipt path");
            }
            out.insert(r.relative_path.clone(), r);
        }
    }
    Ok(out)
}
async fn references(pool: &PgPool) -> Result<(Vec<Device>, Vec<VideoSystem>, Vec<Pilot>)> {
    let mut ds = vec![];
    for r in sqlx::query("SELECT id,name,type FROM devices WHERE active ORDER BY id")
        .fetch_all(pool)
        .await?
    {
        let id = r.get("id");
        ds.push(Device {
            id,
            name: r.get("name"),
            kind: match r.get::<String, _>("type").as_str() {
                "plane" => DeviceType::Plane,
                "quad" => DeviceType::Quad,
                _ => DeviceType::Other,
            },
            aliases: sqlx::query_scalar(
                "SELECT alias FROM device_aliases WHERE device_id=$1 ORDER BY alias",
            )
            .bind(id)
            .fetch_all(pool)
            .await?,
        });
    }
    let mut ss = vec![];
    for r in sqlx::query("SELECT id,name,with_osd FROM video_systems WHERE active ORDER BY id")
        .fetch_all(pool)
        .await?
    {
        let id = r.get("id");
        ss.push(VideoSystem {
            id,
            name: r.get("name"),
            with_osd: r.get("with_osd"),
            aliases: sqlx::query_scalar(
                "SELECT alias FROM video_system_aliases WHERE video_system_id=$1 ORDER BY alias",
            )
            .bind(id)
            .fetch_all(pool)
            .await?,
        });
    }
    let mut ps = vec![];
    for r in sqlx::query(
        "SELECT id,is_public,full_name,nickname,telegram_username FROM users WHERE active ORDER BY id",
    )
    .fetch_all(pool)
    .await?
    {
        let id = r.get("id");
        let mut aliases: Vec<String> =
            sqlx::query_scalar("SELECT alias FROM user_file_aliases WHERE user_id=$1 ORDER BY alias")
                .bind(id)
                .fetch_all(pool)
                .await?;
        if aliases.is_empty() {
            for field in ["full_name", "nickname", "telegram_username"] {
                if let Some(v) = r.get::<Option<String>, _>(field) {
                    aliases.push(v.trim_start_matches('@').into());
                }
            }
        }
        ps.push(Pilot {
            id,
            aliases,
            public: r.get("is_public"),
        });
    }
    Ok((ds, ss, ps))
}
