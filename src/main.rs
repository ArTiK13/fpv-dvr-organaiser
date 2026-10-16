use anyhow::Result;
use axum::{routing::get, Router};
use fpv_catalog_bot::{
    bot::{self, BotState},
    config::Config,
    db, delivery,
    downloads::{self, DownloadState},
    immich::ImmichClient,
    notifications, scanner,
};
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use tokio::net::TcpListener;
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let c = Arc::new(Config::from_env()?);
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect(&c.database_url)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    db::bootstrap(&pool, c.bootstrap_admin_chat_id).await?;
    let immich = Arc::new(
        ImmichClient::new(
            c.immich_base_url.clone(),
            c.immich_public_url.clone(),
            c.immich_api_key.clone(),
            c.immich_container_root.clone(),
            c.immich_library_id.clone(),
        )
        .with_share_metadata(c.immich_show_metadata),
    );
    if let Err(e) = immich.health().await {
        tracing::warn!(error=%c.redact(&e.to_string()),"Immich health check");
    }
    if std::env::args().any(|a| a == "--scan-once") {
        scanner::scan_once(&pool, &c).await?;
        return Ok(());
    }
    if std::env::args().any(|a| a == "--immich-check") {
        immich.health().await?;
        return Ok(());
    }
    let args: Vec<String> = std::env::args().collect();
    if let Some(index) = args.iter().position(|a| a == "--immich-smoke") {
        let path = args
            .get(index + 1)
            .ok_or_else(|| anyhow::anyhow!("--immich-smoke requires relative video path"))?;
        let asset = immich
            .resolve_asset(path)
            .await?
            .ok_or_else(|| anyhow::anyhow!("test video not indexed by Immich"))?;
        let expires = chrono::Utc::now() + chrono::Duration::minutes(5);
        let (share, _) = immich
            .create_share(
                &[asset.id],
                expires,
                format!("FPV smoke {}", uuid::Uuid::new_v4()),
            )
            .await?;
        immich.revoke_share(share.id).await?;
        tracing::info!("Immich smoke test: asset lookup, share creation and revocation passed");
        return Ok(());
    }
    let scan = tokio::spawn(scanner::run_loop(pool.clone(), c.clone()));
    let maintenance = tokio::spawn(delivery::run_maintenance(
        pool.clone(),
        c.clone(),
        immich.clone(),
    ));
    let mut workers = vec![scan, maintenance];
    if c.delivery_enabled {
        workers.push(tokio::spawn(delivery::run(pool.clone(), c.clone(), immich)));
    }
    if c.notifications_enabled {
        workers.push(tokio::spawn(notifications::run(pool.clone(), c.clone())));
    }
    let state = DownloadState {
        pool: pool.clone(),
        config: c.clone(),
    };
    let health_pool = pool.clone();
    let app = Router::new()
        .route(
            "/healthz",
            get(move || {
                let p = health_pool.clone();
                async move {
                    match sqlx::query("SELECT 1").execute(&p).await {
                        Ok(_) => axum::http::StatusCode::OK,
                        Err(_) => axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    }
                }
            }),
        )
        .merge(downloads::router(state));
    let listener = TcpListener::bind(&c.bind_addr).await?;
    let server = axum::serve(listener, app);
    let telegram = bot::run(BotState::new(pool.clone(), c.clone()));
    tokio::select! {r=server=>{r?;},_=telegram=>{},_=shutdown()=>{}}
    for w in workers {
        w.abort();
    }
    pool.close().await;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
