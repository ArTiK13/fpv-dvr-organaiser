use crate::scanner::safe_relative;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;
#[derive(Clone)]
pub struct ImmichClient {
    client: Client,
    base_url: String,
    public_url: String,
    api_key: String,
    container_root: String,
    library_id: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub id: Uuid,
    #[serde(rename = "originalPath")]
    pub original_path: String,
    #[serde(rename = "libraryId")]
    pub library_id: Option<Uuid>,
    #[serde(rename = "isOffline")]
    pub is_offline: bool,
    #[serde(rename = "isTrashed")]
    pub is_trashed: bool,
    #[serde(rename = "type")]
    pub kind: String,
}
#[derive(Deserialize)]
struct SearchResponse {
    assets: SearchAssets,
}
#[derive(Deserialize)]
struct SearchAssets {
    items: Vec<Asset>,
    #[serde(rename = "nextCursor")]
    next_cursor: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct ShareResponse {
    pub id: Uuid,
    pub key: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: Option<DateTime<Utc>>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareRequest<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub asset_ids: &'a [Uuid],
    pub expires_at: DateTime<Utc>,
    pub allow_download: bool,
    pub allow_upload: bool,
    pub show_metadata: bool,
    pub description: String,
}
impl ImmichClient {
    pub fn new(base: String, public: String, key: String, root: String, library: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("HTTP client"),
            base_url: base.trim_end_matches('/').into(),
            public_url: public.trim_end_matches('/').into(),
            api_key: key,
            container_root: root.trim_end_matches('/').into(),
            library_id: library,
        }
    }
    pub fn original_path(&self, relative: &str) -> Result<String> {
        if !safe_relative(relative) {
            bail!("unsafe relative path");
        }
        Ok(format!("{}/{}", self.container_root, relative))
    }
    pub async fn resolve_asset(&self, relative: &str) -> Result<Option<Asset>> {
        let path = self.original_path(relative)?;
        let mut cursor = None;
        let mut matches = vec![];
        loop {
            let mut body = serde_json::json!({"filter":{"originalPath":{"eq":path}},"orderBy":{"field":"fileCreatedAt","direction":"desc"}});
            if let Some(c) = cursor {
                body["cursor"] = serde_json::Value::String(c);
            }
            let result: SearchResponse = self
                .client
                .post(format!("{}/search/metadata", self.base_url))
                .header("x-api-key", &self.api_key)
                .json(&body)
                .send()
                .await
                .context("Immich search transport")?
                .error_for_status()
                .context("Immich search status")?
                .json()
                .await
                .context("Immich search response schema (check version compatibility)")?;
            for a in result.assets.items {
                if a.original_path == path
                    && !a.is_offline
                    && !a.is_trashed
                    && a.kind == "VIDEO"
                    && a.library_id
                        .is_some_and(|l| l.to_string() == self.library_id)
                {
                    matches.push(a);
                }
            }
            cursor = result.assets.next_cursor;
            if cursor.is_none() {
                break;
            }
            if matches.len() > 1 {
                break;
            }
        }
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            _ => bail!("ambiguous Immich assets for path"),
        }
    }
    pub async fn create_share(
        &self,
        ids: &[Uuid],
        expires: DateTime<Utc>,
        description: String,
    ) -> Result<(ShareResponse, String)> {
        if ids.is_empty() {
            bail!("empty Immich share");
        }
        // A timed-out POST may have succeeded remotely. Recover by the request's
        // unique description, but only when the complete immutable asset set agrees.
        let shares: Vec<serde_json::Value> = self
            .client
            .get(format!("{}/shared-links", self.base_url))
            .header("x-api-key", &self.api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut expected = ids.to_vec();
        expected.sort();
        for value in shares {
            if value["description"].as_str() != Some(description.as_str()) {
                continue;
            }
            let mut actual = value["assets"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v["id"].as_str()?.parse::<Uuid>().ok())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            actual.sort();
            if actual != expected
                || value["allowDownload"] != true
                || value["allowUpload"] != false
                || value["showMetadata"] != false
            {
                continue;
            }
            let share: ShareResponse = serde_json::from_value(value)?;
            if share.expires_at.is_some_and(|e| e > Utc::now()) {
                let url = self.share_url(&share.key);
                return Ok((share, url));
            }
        }
        let body = ShareRequest {
            kind: "INDIVIDUAL",
            asset_ids: ids,
            expires_at: expires,
            allow_download: true,
            allow_upload: false,
            show_metadata: false,
            description,
        };
        let s: ShareResponse = self
            .client
            .post(format!("{}/shared-links", self.base_url))
            .header("x-api-key", &self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let url = self.share_url(&s.key);
        Ok((s, url))
    }
    pub fn share_url(&self, key: &str) -> String {
        format!("{}/share/{}", self.public_url, key)
    }
    pub async fn revoke_share(&self, id: Uuid) -> Result<()> {
        let r = self
            .client
            .delete(format!("{}/shared-links/{id}", self.base_url))
            .header("x-api-key", &self.api_key)
            .send()
            .await?;
        if r.status() != StatusCode::NOT_FOUND {
            r.error_for_status()?;
        }
        Ok(())
    }
    pub async fn health(&self) -> Result<()> {
        self.client
            .get(format!("{}/server/ping", self.base_url))
            .header("x-api-key", &self.api_key)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}
#[async_trait::async_trait]
pub trait AssetPublisher: Send + Sync {
    async fn resolve(&self, relative: &str) -> Result<Option<Asset>>;
    async fn share(
        &self,
        ids: &[Uuid],
        expires: DateTime<Utc>,
        description: String,
    ) -> Result<(ShareResponse, String)>;
    async fn revoke(&self, id: Uuid) -> Result<()>;
}
#[async_trait::async_trait]
impl AssetPublisher for ImmichClient {
    async fn resolve(&self, p: &str) -> Result<Option<Asset>> {
        self.resolve_asset(p).await
    }
    async fn share(
        &self,
        i: &[Uuid],
        e: DateTime<Utc>,
        d: String,
    ) -> Result<(ShareResponse, String)> {
        self.create_share(i, e, d).await
    }
    async fn revoke(&self, i: Uuid) -> Result<()> {
        self.revoke_share(i).await
    }
}
