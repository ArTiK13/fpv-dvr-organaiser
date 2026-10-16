use anyhow::{bail, Context, Result};
use std::{env, path::PathBuf, time::Duration};

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub video_root: PathBuf,
    pub immich_base_url: String,
    pub immich_public_url: String,
    pub immich_api_key: String,
    pub immich_library_id: String,
    pub immich_container_root: String,
    pub immich_asset_wait_timeout: Duration,
    pub immich_link_ttl: Duration,
    pub immich_show_metadata: bool,
    pub telegram_bot_token: String,
    pub bootstrap_admin_chat_id: i64,
    pub default_user_id: Option<i64>,
    pub scan_interval: Duration,
    pub bind_addr: String,
    pub fallback_base_url: Option<String>,
    pub telegram_limit: u64,
    pub telegram_upload_timeout: Duration,
    pub share_chunk_size: usize,
    pub require_receipts: bool,
    pub stable_seconds: u64,
    pub delivery_enabled: bool,
    pub notifications_enabled: bool,
}
impl Config {
    /// Errors from HTTP clients can contain request URLs (including bot tokens).
    pub fn redact(&self, message: &str) -> String {
        let mut result = message.to_owned();
        for secret in [
            &self.telegram_bot_token,
            &self.immich_api_key,
            &self.database_url,
        ] {
            if !secret.is_empty() {
                result = result.replace(secret, "[REDACTED]");
            }
        }
        result
    }
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        let root = PathBuf::from(required("VIDEO_ROOT")?);
        if !root.is_absolute() {
            bail!("VIDEO_ROOT must be absolute");
        }
        let container = env::var("IMMICH_CONTAINER_ROOT").unwrap_or("/mnt/fpv".into());
        if !container.starts_with('/') {
            bail!("IMMICH_CONTAINER_ROOT must be absolute");
        }
        let c = Self {
            database_url: required("DATABASE_URL")?,
            video_root: root,
            immich_base_url: required("IMMICH_BASE_URL")?.trim_end_matches('/').into(),
            immich_public_url: required("IMMICH_PUBLIC_URL")?.trim_end_matches('/').into(),
            immich_api_key: secret("IMMICH_API_KEY")?,
            immich_library_id: required("IMMICH_LIBRARY_ID")?,
            immich_container_root: container,
            immich_asset_wait_timeout: duration_env("IMMICH_ASSET_WAIT_TIMEOUT", 1800)?,
            immich_link_ttl: duration_env("IMMICH_LINK_TTL", 604800)?,
            immich_show_metadata: env::var("IMMICH_SHOW_METADATA")
                .unwrap_or("false".into())
                .parse()?,
            telegram_bot_token: secret("TELEGRAM_BOT_TOKEN")?,
            bootstrap_admin_chat_id: required("BOOTSTRAP_ADMIN_CHAT_ID")?.parse()?,
            default_user_id: env::var("DEFAULT_USER_ID")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| v.parse())
                .transpose()?,
            scan_interval: duration_env("SCAN_INTERVAL", 60)?,
            bind_addr: env::var("BIND_ADDR").unwrap_or("0.0.0.0:8080".into()),
            fallback_base_url: env::var("FALLBACK_BASE_URL").ok().filter(|v| !v.is_empty()),
            telegram_limit: number("TELEGRAM_MAX_BYTES", 49_000_000)?,
            telegram_upload_timeout: duration_env("TELEGRAM_UPLOAD_TIMEOUT", 600)?,
            share_chunk_size: number("IMMICH_SHARE_CHUNK_SIZE", 500)?,
            require_receipts: env::var("REQUIRE_SYNC_RECEIPTS")
                .unwrap_or("true".into())
                .parse()?,
            stable_seconds: number("STABLE_SECONDS", 60)?,
            delivery_enabled: env::var("DELIVERY_ENABLED")
                .unwrap_or("true".into())
                .parse()?,
            notifications_enabled: env::var("NOTIFICATIONS_ENABLED")
                .unwrap_or("true".into())
                .parse()?,
        };
        if c.scan_interval.is_zero()
            || c.share_chunk_size == 0
            || c.immich_link_ttl.is_zero()
            || c.telegram_upload_timeout.is_zero()
        {
            bail!("interval, chunk size and TTL must be positive");
        }
        uuid::Uuid::parse_str(&c.immich_library_id).context("IMMICH_LIBRARY_ID must be UUID")?;
        for u in [&c.immich_base_url, &c.immich_public_url] {
            reqwest::Url::parse(u)?;
        }
        Ok(c)
    }
}
fn required(n: &str) -> Result<String> {
    env::var(n)
        .with_context(|| format!("missing {n}"))
        .and_then(|s| {
            if s.trim().is_empty() {
                bail!("empty {n}")
            } else {
                Ok(s)
            }
        })
}
fn secret(n: &str) -> Result<String> {
    if let Ok(p) = env::var(format!("{n}_FILE")) {
        Ok(std::fs::read_to_string(p)?.trim().into())
    } else {
        required(n)
    }
}
fn number<T: std::str::FromStr>(n: &str, d: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    env::var(n)
        .map(|s| s.parse::<T>().map_err(|e| anyhow::anyhow!("{n}: {e}")))
        .unwrap_or(Ok(d))
}
fn duration_env(n: &str, d: u64) -> Result<Duration> {
    let v = env::var(n).unwrap_or(d.to_string());
    let (v, m) = match v.chars().last() {
        Some('s') => (&v[..v.len() - 1], 1),
        Some('m') => (&v[..v.len() - 1], 60),
        Some('h') => (&v[..v.len() - 1], 3600),
        Some('d') => (&v[..v.len() - 1], 86400),
        _ => (&v[..], 1),
    };
    Ok(Duration::from_secs(
        v.parse::<u64>()?
            .checked_mul(m)
            .context("duration overflow")?,
    ))
}
