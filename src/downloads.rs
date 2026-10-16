use crate::{config::Config, db, scanner::safe_relative};
use anyhow::{bail, Result};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use chrono::Utc;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{path::PathBuf, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
#[derive(Clone)]
pub struct DownloadState {
    pub pool: PgPool,
    pub config: Arc<Config>,
}
pub fn router(s: DownloadState) -> Router {
    Router::new()
        .route("/download/:token", get(download))
        .with_state(s)
}
pub async fn checked_path(root: &std::path::Path, relative: &str) -> Result<PathBuf> {
    if !safe_relative(relative) {
        bail!("unsafe path");
    }
    let root = tokio::fs::canonicalize(root).await?;
    let mut path = root.clone();
    for part in std::path::Path::new(relative).components() {
        path.push(part);
        if tokio::fs::symlink_metadata(&path)
            .await?
            .file_type()
            .is_symlink()
        {
            bail!("symlink forbidden");
        }
    }
    let resolved = tokio::fs::canonicalize(path).await?;
    if !resolved.starts_with(root) {
        bail!("path escaped root");
    }
    Ok(resolved)
}
pub async fn create_token(s: &DownloadState, u: &db::User, id: i64) -> Result<String> {
    if !s.config.delivery_enabled {
        bail!("Выдача выключена в конфигурации");
    }
    if !u.active || !u.is_admin {
        bail!("administrator only");
    }
    if db::video(&s.pool, u, id).await?.is_none() {
        bail!("unavailable video");
    }
    let base = s
        .config
        .fallback_base_url
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("FALLBACK_BASE_URL not configured"))?;
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let hash = hex::encode(Sha256::digest(token.as_bytes()));
    sqlx::query("INSERT INTO fallback_download_tokens(token_hash,video_id,user_id,expires_at) VALUES($1,$2,$3,$4)").bind(hash).bind(id).bind(u.id).bind(Utc::now()+chrono::Duration::hours(1)).execute(&s.pool).await?;
    Ok(format!("{}/download/{token}", base.trim_end_matches('/')))
}
async fn download(
    State(s): State<DownloadState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    if !s.config.delivery_enabled || token.len() != 64 {
        return Err(StatusCode::NOT_FOUND);
    }
    let hash = hex::encode(Sha256::digest(token.as_bytes()));
    let row:Option<(i64,i64)>=sqlx::query_as("SELECT t.video_id,u.telegram_id FROM fallback_download_tokens t JOIN users u ON u.id=t.user_id WHERE token_hash=$1 AND expires_at>now() AND revoked_at IS NULL AND u.active AND u.is_admin").bind(hash).fetch_optional(&s.pool).await.map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?;
    let Some((id, tg)) = row else {
        return Err(StatusCode::NOT_FOUND);
    };
    let u = db::user(&s.pool, tg)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let v = db::video(&s.pool, &u, id)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let p = checked_path(&s.config.video_root, &v.relative_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let mut file = tokio::fs::File::open(&p)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let len = file
        .metadata()
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?
        .len();
    if len != v.size_bytes as u64 {
        return Err(StatusCode::CONFLICT);
    }
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let (start, end, partial) =
        range_bytes(range, len).map_err(|_| StatusCode::RANGE_NOT_SATISFIABLE)?;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let amount = end - start + 1;
    let mut builder = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"video-{id}.mp4\""),
        )
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, amount.to_string());
    if partial {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
    }
    builder
        .body(Body::from_stream(ReaderStream::new(file.take(amount))))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
pub fn range_bytes(range: Option<&str>, len: u64) -> Result<(u64, u64, bool)> {
    if len == 0 {
        bail!("empty file");
    }
    let Some(r) = range else {
        return Ok((0, len - 1, false));
    };
    let r = r
        .strip_prefix("bytes=")
        .ok_or_else(|| anyhow::anyhow!("bad range"))?;
    let (a, b) = r
        .split_once('-')
        .ok_or_else(|| anyhow::anyhow!("bad range"))?;
    let (start, end) = if a.is_empty() {
        let suffix = b.parse::<u64>()?;
        if suffix == 0 {
            bail!("zero suffix");
        }
        (len.saturating_sub(suffix), len - 1)
    } else {
        (
            a.parse::<u64>()?,
            if b.is_empty() {
                len - 1
            } else {
                b.parse::<u64>()?.min(len - 1)
            },
        )
    };
    if start > end || start >= len {
        bail!("bad range");
    }
    Ok((start, end, true))
}
