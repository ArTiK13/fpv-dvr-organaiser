use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{path::Path, time::Duration};
use tokio::process::Command;
#[derive(Clone, Debug, Deserialize)]
pub struct ProbeFormat {
    pub duration: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ProbeStream {
    pub codec_type: Option<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub avg_frame_rate: Option<String>,
    pub r_frame_rate: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ProbeOutput {
    pub streams: Vec<ProbeStream>,
    pub format: ProbeFormat,
}
pub async fn probe(path: &Path) -> Result<ProbeOutput> {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new("ffprobe")
            .kill_on_drop(true)
            .args([
                "-v",
                "error",
                "-show_streams",
                "-show_format",
                "-of",
                "json",
            ])
            .arg(path)
            .output(),
    )
    .await
    .context("ffprobe timeout")??;
    if !out.status.success() {
        bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(1000)
                .collect::<String>()
        );
    }
    let p: ProbeOutput = serde_json::from_slice(&out.stdout)?;
    if !p
        .streams
        .iter()
        .any(|s| s.codec_type.as_deref() == Some("video"))
    {
        bail!("no video stream");
    }
    Ok(p)
}
