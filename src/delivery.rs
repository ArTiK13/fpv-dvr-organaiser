use crate::{config::Config, db, downloads::checked_path, immich::ImmichClient, notifications};
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use teloxide::{
    prelude::*,
    types::{ChatId, InputFile},
};
use tracing::warn;
use uuid::Uuid;
#[derive(sqlx::FromRow)]
struct Job {
    id: Uuid,
    user_id: i64,
    chat_id: i64,
    mode: String,
    wait_started_at: Option<DateTime<Utc>>,
    attempts: i32,
}
pub async fn run(pool: PgPool, c: Arc<Config>, immich: Arc<ImmichClient>) {
    // teloxide's default 17-second timeout also covers the entire multipart upload.
    let client = teloxide::net::default_reqwest_settings()
        .timeout(c.telegram_upload_timeout)
        .build()
        .expect("Telegram upload HTTP client");
    let bot = Bot::with_client(c.telegram_bot_token.clone(), client);
    loop {
        if let Err(e) = tick(&pool, &c, &immich, &bot).await {
            warn!(error=%c.redact(&e.to_string()),"delivery worker");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
pub async fn tick(pool: &PgPool, c: &Config, immich: &ImmichClient, bot: &Bot) -> Result<()> {
    let mut lock = pool.begin().await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(76120303)")
        .fetch_one(&mut *lock)
        .await?;
    if !acquired {
        return Ok(());
    }
    maintain_links(pool, immich).await?;
    let job:Option<Job>=sqlx::query_as("SELECT id,user_id,chat_id,mode,wait_started_at,attempts FROM delivery_jobs WHERE state IN ('pending','running','waiting_for_immich') AND next_attempt_at<=now() ORDER BY created_at LIMIT 1").fetch_optional(pool).await?;
    if let Some(j) = job {
        sqlx::query("UPDATE delivery_jobs SET state='running',attempts=attempts+1 WHERE id=$1 AND state<>'cancelled'").bind(j.id).execute(pool).await?;
        if let Err(e) = process(pool, c, immich, bot, &j).await {
            let error = c.redact(&e.to_string());
            let timed_out = (Utc::now() - j.wait_started_at.unwrap_or_else(Utc::now))
                .to_std()
                .unwrap_or_default()
                >= c.immich_asset_wait_timeout;
            if timed_out {
                sqlx::query("UPDATE delivery_jobs SET state='failed',error=$2 WHERE id=$1 AND state<>'cancelled'").bind(j.id).bind(&error).execute(pool).await?;
                notifications::alert(
                    pool,
                    &format!("delivery:{}", j.id),
                    &format!("Не завершена выдача {}: {}", j.id, error),
                )
                .await?;
                let _=bot.send_message(ChatId(j.chat_id),"Не удалось завершить выдачу. Immich мог ещё не обработать файл. Попробуйте позже. Прогресс доступен через «Мои запросы».").await;
            } else {
                let seconds = (15_i64 * 2_i64.pow((j.attempts as u32).min(5))).min(300);
                sqlx::query("UPDATE delivery_jobs SET state='waiting_for_immich',wait_started_at=coalesce(wait_started_at,now()),error=$2,next_attempt_at=now()+($3 * interval '1 second') WHERE id=$1 AND state<>'cancelled'").bind(j.id).bind(&error).bind(seconds as f64).execute(pool).await?;
            }
        }
    }
    lock.commit().await?;
    Ok(())
}
/// Revocations run separately so a long Telegram upload cannot postpone them.
pub async fn run_maintenance(pool: PgPool, c: Arc<Config>, immich: Arc<ImmichClient>) {
    loop {
        if let Err(e) = maintain_links(&pool, &immich).await {
            warn!(error=%c.redact(&e.to_string()), "link maintenance");
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

pub async fn maintain_links(pool: &PgPool, immich: &ImmichClient) -> Result<()> {
    let mut lock = pool.begin().await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(76120304)")
        .fetch_one(&mut *lock)
        .await?;
    if !acquired {
        return Ok(());
    }
    db::invalidate_links(pool).await?;
    let rows = sqlx::query("SELECT id,immich_id FROM immich_shared_links WHERE revoked_at IS NULL AND (revoke_requested OR expires_at<now())")
        .fetch_all(pool).await?;
    let mut failed = false;
    for row in rows {
        let local: i64 = row.get("id");
        let remote: Uuid = row.get("immich_id");
        match immich.revoke_share(remote).await {
            Ok(()) => {
                sqlx::query("UPDATE immich_shared_links SET revoked_at=now() WHERE id=$1")
                    .bind(local)
                    .execute(pool)
                    .await?;
            }
            Err(_) => {
                failed = true;
                notifications::alert(
                    pool,
                    "immich_revoke",
                    "Не удалось отозвать ссылку Immich; повторяем попытки.",
                )
                .await?;
            }
        }
    }
    if !failed {
        sqlx::query("UPDATE system_alerts SET active=false WHERE key='immich_revoke'")
            .execute(pool)
            .await?;
    }
    lock.commit().await?;
    Ok(())
}

async fn requester(pool: &PgPool, j: &Job) -> Result<db::User> {
    let tg: i64 = sqlx::query_scalar("SELECT telegram_id FROM users WHERE id=$1")
        .bind(j.user_id)
        .fetch_one(pool)
        .await?;
    let u = db::user(pool, tg)
        .await?
        .ok_or_else(|| anyhow::anyhow!("unknown user"))?;
    let state: String = sqlx::query_scalar("SELECT state FROM delivery_jobs WHERE id=$1")
        .bind(j.id)
        .fetch_one(pool)
        .await?;
    if !u.active || u.telegram_id != j.chat_id || state == "cancelled" {
        bail!("cancelled or inactive user");
    }
    Ok(u)
}
async fn process(
    pool: &PgPool,
    c: &Config,
    immich: &ImmichClient,
    bot: &Bot,
    j: &Job,
) -> Result<()> {
    let ids:Vec<i64>=sqlx::query_scalar("SELECT video_id FROM delivery_job_videos WHERE job_id=$1 AND NOT delivered ORDER BY video_id").bind(j.id).fetch_all(pool).await?;
    let mut big = vec![];
    for id in ids {
        let u = requester(pool, j).await?;
        let v = db::video(pool, &u, id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("permission changed for video {id}"))?;
        if j.mode == "immich" || v.size_bytes as u64 > c.telegram_limit {
            big.push(v);
            continue;
        }
        let p = checked_path(&c.video_root, &v.relative_path).await?;
        if tokio::fs::metadata(&p).await?.len() != v.size_bytes as u64 {
            bail!("video changed");
        }
        let result = if j.mode == "document" {
            bot.send_document(ChatId(j.chat_id), InputFile::file(p))
                .await
        } else {
            bot.send_video(ChatId(j.chat_id), InputFile::file(p)).await
        };
        match result {
            Ok(_) => {
                mark(pool, j.id, id).await?;
            }
            Err(teloxide::RequestError::RetryAfter(s)) => {
                tokio::time::sleep(s.duration().min(std::time::Duration::from_secs(30))).await;
                bail!("Telegram rate limit");
            }
            Err(e) => return Err(e.into()),
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    for chunk in big.chunks(c.share_chunk_size) {
        let u = requester(pool, j).await?;
        let mut assets = vec![];
        let mut videos = vec![];
        for v in chunk {
            if db::video(pool, &u, v.id).await?.is_none() {
                bail!("permission changed");
            }
            let a = immich
                .resolve_asset(&v.relative_path)
                .await?
                .ok_or_else(|| anyhow::anyhow!("asset not yet indexed: {}", v.relative_path))?;
            sqlx::query("UPDATE videos SET immich_asset_id=$2,immich_original_path=$3,immich_status='ready',immich_last_seen_at=now() WHERE id=$1").bind(v.id).bind(a.id).bind(a.original_path).execute(pool).await?;
            assets.push(a.id);
            videos.push(v.id);
        }
        // Reuse a saved share after process restart; never expand a published selection.
        let existing:Option<(i64,String,DateTime<Utc>)>=sqlx::query_as("SELECT l.id,l.immich_key,l.expires_at FROM immich_shared_links l WHERE l.job_id=$1 AND NOT l.revoke_requested AND l.revoked_at IS NULL AND l.expires_at>now() AND (SELECT array_agg(lv.video_id ORDER BY lv.video_id) FROM immich_shared_link_videos lv WHERE lv.link_id=l.id)=$2::bigint[] LIMIT 1").bind(j.id).bind(&videos).fetch_optional(pool).await?;
        let (local, key, expires_at) = if let Some(x) = existing {
            x
        } else {
            let expires = Utc::now() + chrono::Duration::from_std(c.immich_link_ttl)?;
            let (share, _) = immich
                .create_share(
                    &assets,
                    expires,
                    format!("FPV bot request {}:{}", j.id, videos[0]),
                )
                .await?;
            let saved=async{let mut tx=pool.begin().await?;let local:i64=sqlx::query_scalar("INSERT INTO immich_shared_links(immich_id,immich_key,user_id,job_id,expires_at) VALUES($1,$2,$3,$4,$5) RETURNING id").bind(share.id).bind(&share.key).bind(j.user_id).bind(j.id).bind(share.expires_at.unwrap_or(expires)).fetch_one(&mut *tx).await?;for v in &videos{sqlx::query("INSERT INTO immich_shared_link_videos VALUES($1,$2)").bind(local).bind(v).execute(&mut *tx).await?;}tx.commit().await?;Ok::<_,anyhow::Error>((local,share.key.clone(),share.expires_at.unwrap_or(expires)))}.await;
            match saved {
                Ok(v) => v,
                Err(e) => {
                    let _ = immich.revoke_share(share.id).await;
                    return Err(e);
                }
            }
        };
        // Check the fresh identity and each ACL again immediately before releasing the URL.
        let u = requester(pool, j).await?;
        for id in &videos {
            if db::video(pool, &u, *id).await?.is_none() {
                db::invalidate_links(pool).await?;
                bail!("permission changed before share publication");
            }
        }
        bot.send_message(
            ChatId(j.chat_id),
            format!(
                "Immich: {} видео, ссылка действует до {} (Москва):\n{}",
                videos.len(),
                expires_at
                    .with_timezone(&chrono_tz::Europe::Moscow)
                    .format("%d.%m.%Y %H:%M"),
                immich.share_url(&key)
            ),
        )
        .await?;
        sqlx::query("UPDATE immich_shared_links SET sent=true WHERE id=$1")
            .bind(local)
            .execute(pool)
            .await?;
        mark_batch(pool, j.id, &videos).await?;
    }
    sqlx::query(
        "UPDATE delivery_jobs SET state='done',error=NULL WHERE id=$1 AND state<>'cancelled'",
    )
    .bind(j.id)
    .execute(pool)
    .await?;
    bot.send_message(ChatId(j.chat_id), "✅ Выдача видео завершена.")
        .await?;
    Ok(())
}
async fn mark(pool: &PgPool, job: Uuid, video: i64) -> Result<()> {
    mark_batch(pool, job, &[video]).await
}
async fn mark_batch(pool: &PgPool, job: Uuid, videos: &[i64]) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE delivery_job_videos SET delivered=true WHERE job_id=$1 AND video_id=ANY($2)",
    )
    .bind(job)
    .bind(videos)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE delivery_jobs SET completed_count=(SELECT count(*) FROM delivery_job_videos WHERE job_id=$1 AND delivered) WHERE id=$1").bind(job).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
