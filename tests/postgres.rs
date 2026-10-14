use anyhow::Result;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    routing::post,
    Json, Router,
};
use fpv_catalog_bot::{
    config::Config,
    db::{self, Filters, UserInput},
    downloads::{self, DownloadState},
    notifications, scanner,
};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tower::ServiceExt;
use uuid::Uuid;
async fn pool() -> Result<Option<(PgPool, String, PgPool)>> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("PostgreSQL test skipped: set TEST_DATABASE_URL");
        return Ok(None);
    };
    let admin = PgPool::connect(&url).await?;
    let schema = format!("fpvtest_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;
    let path = schema.clone();
    let p = PgPoolOptions::new()
        .max_connections(16)
        .after_connect(move |conn, _| {
            let path = path.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {path}"))
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await?;
    sqlx::migrate!().run(&p).await?;
    Ok(Some((p, schema, admin)))
}
fn config(root: std::path::PathBuf) -> Config {
    Config {
        database_url: String::new(),
        video_root: root,
        immich_base_url: "http://localhost/api".into(),
        immich_public_url: "https://photos.example".into(),
        immich_api_key: "test".into(),
        immich_library_id: Uuid::new_v4().to_string(),
        immich_container_root: "/mnt/fpv".into(),
        immich_asset_wait_timeout: Duration::from_secs(1800),
        immich_link_ttl: Duration::from_secs(604800),
        telegram_bot_token: "test".into(),
        bootstrap_admin_chat_id: 7777,
        default_user_id: None,
        scan_interval: Duration::from_secs(60),
        bind_addr: "127.0.0.1:0".into(),
        fallback_base_url: Some("https://fpv.example".into()),
        telegram_limit: 49_000_000,
        share_chunk_size: 500,
        require_receipts: false,
        stable_seconds: 0,
        delivery_enabled: true,
        notifications_enabled: true,
    }
}
async fn add(p: &PgPool, actor: i64, tg: i64, name: &str, public: bool) -> Result<i64> {
    let v: UserInput = serde_json::from_value(
        serde_json::json!({"telegram_id":tg,"nickname":name,"is_public":public}),
    )?;
    db::save_user(p, actor, v, 7777).await
}
#[tokio::test]
async fn catalog_lifecycle_permissions_links_and_notifications() -> Result<()> {
    let Some((p, schema, admin_pool)) = pool().await? else {
        return Ok(());
    };
    let temp = tempfile::tempdir()?;
    let mut c = config(temp.path().to_path_buf());
    db::bootstrap(&p, 7777).await?;
    let admin = db::user(&p, 7777).await?.unwrap();
    let own = add(&p, admin.id, 1111, "own", false).await?;
    let other = add(&p, admin.id, 2222, "private", false).await?;
    let public = add(&p, admin.id, 3333, "public", true).await?;
    let u = db::user(&p, 1111).await?.unwrap();
    assert_ne!(u.id, u.telegram_id);
    sqlx::query("INSERT INTO devices(name,type) VALUES('T1','quad')")
        .execute(&p)
        .await?;
    sqlx::query("INSERT INTO device_aliases SELECT id,'t1' FROM devices")
        .execute(&p)
        .await?;
    sqlx::query("INSERT INTO video_systems(name,with_osd) VALUES('O4',false)")
        .execute(&p)
        .await?;
    sqlx::query("INSERT INTO video_system_aliases SELECT id,'o4' FROM video_systems")
        .execute(&p)
        .await?;
    let folder = temp.path().join("karelia_08.26/T1_O4/own");
    std::fs::create_dir_all(&folder)?;
    let movie = folder.join("v.MP4");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=32x32:d=1",
            "-c:v",
            "mpeg4",
            "-y",
        ])
        .arg(&movie)
        .status()?;
    assert!(status.success());
    let other_folder = temp.path().join("event/t1_o4/private");
    let public_folder = temp.path().join("event/t1_o4/public");
    for f in [&other_folder, &public_folder] {
        std::fs::create_dir_all(f)?;
        std::fs::copy(&movie, f.join("v.mp4"))?;
    }
    assert_eq!(scanner::scan_once(&p, &c).await?, 3);
    let (_, _, visible) = db::search(&p, &u, &Filters::default()).await?;
    assert_eq!(visible.len(), 2);
    assert_eq!(
        db::selection(&p, &admin, &Filters::default()).await?.len(),
        3
    );
    let date_filter = Filters {
        from: Some("2026-08-17".parse()?),
        to: Some("2026-08-17".parse()?),
        ..Default::default()
    };
    assert_eq!(db::search(&p, &u, &date_filter).await?.0, 1);
    let own_video: i64 = sqlx::query_scalar("SELECT id FROM videos WHERE pilot_id=$1")
        .bind(own)
        .fetch_one(&p)
        .await?;
    let private_video: i64 = sqlx::query_scalar("SELECT id FROM videos WHERE pilot_id=$1")
        .bind(other)
        .fetch_one(&p)
        .await?;
    let public_video: i64 = sqlx::query_scalar("SELECT id FROM videos WHERE pilot_id=$1")
        .bind(public)
        .fetch_one(&p)
        .await?;
    assert!(db::video(&p, &u, private_video).await?.is_none());
    assert!(db::video(&p, &u, own_video).await?.is_some());
    // Repeated scans preserve first publication and do not enqueue duplicate notifications.
    let classified: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT first_classified_at FROM videos WHERE id=$1")
            .bind(own_video)
            .fetch_one(&p)
            .await?;
    scanner::scan_once(&p, &c).await?;
    assert_eq!(
        classified,
        sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
            "SELECT first_classified_at FROM videos WHERE id=$1"
        )
        .bind(own_video)
        .fetch_one(&p)
        .await?
    );
    // Library aliases can reclassify failed matches without probing accepted bytes again.
    sqlx::query("DELETE FROM device_aliases")
        .execute(&p)
        .await?;
    scanner::scan_once(&p, &c).await?;
    assert_eq!(db::search(&p, &u, &Filters::default()).await?.0, 0);
    sqlx::query("INSERT INTO device_aliases SELECT id,'t1' FROM devices")
        .execute(&p)
        .await?;
    scanner::scan_once(&p, &c).await?;
    assert_eq!(db::search(&p, &u, &Filters::default()).await?.0, 2);
    // A link contains an immutable selection; making its owner private requests revocation.
    let link:i64=sqlx::query_scalar("INSERT INTO immich_shared_links(immich_id,immich_key,user_id,expires_at) VALUES($1,'secret',$2,now()+interval '7 days') RETURNING id").bind(Uuid::new_v4()).bind(u.id).fetch_one(&p).await?;
    sqlx::query("INSERT INTO immich_shared_link_videos VALUES($1,$2)")
        .bind(link)
        .bind(public_video)
        .execute(&p)
        .await?;
    sqlx::query("UPDATE users SET is_public=false WHERE id=$1")
        .bind(public)
        .execute(&p)
        .await?;
    db::invalidate_links(&p).await?;
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT revoke_requested FROM immich_shared_links WHERE id=$1"
        )
        .bind(link)
        .fetch_one(&p)
        .await?
    );
    // Fallback is administrator-only, honours Range and returns no data on an invalid token.
    let state = DownloadState {
        pool: p.clone(),
        config: Arc::new(c.clone()),
    };
    assert!(downloads::create_token(&state, &u, own_video)
        .await
        .is_err());
    let url = downloads::create_token(&state, &admin, own_video).await?;
    let token = url.rsplit('/').next().unwrap();
    let mut indexing_config = c.clone();
    indexing_config.delivery_enabled = false;
    let indexing_state = DownloadState {
        pool: p.clone(),
        config: Arc::new(indexing_config),
    };
    assert!(downloads::create_token(&indexing_state, &admin, own_video)
        .await
        .is_err());
    let disabled = downloads::router(indexing_state)
        .oneshot(
            Request::builder()
                .uri(format!("/download/{token}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(disabled.status(), StatusCode::NOT_FOUND);
    let app = downloads::router(state);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/download/{token}"))
                .header("Range", "bytes=0-9")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(to_bytes(response.into_body(), 100).await?.len(), 10);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/download/invalid")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // Notification recording happens only after a successful Telegram response.
    let sent = Arc::new(AtomicUsize::new(0));
    let capture = sent.clone();
    let mock=Router::new().route("/botTEST/SendMessage",post(move||{let sent=capture.clone();async move{sent.fetch_add(1,Ordering::SeqCst);Json(serde_json::json!({"ok":true,"result":{"message_id":1,"date":0,"chat":{"id":1111,"type":"private"},"text":"ok"}}))}}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let bot = teloxide::Bot::new("TEST").set_api_url(format!("http://{addr}").parse()?);
    sqlx::query("UPDATE users SET notification_time='00:00',notifications_enabled=(id=$1)")
        .bind(u.id)
        .execute(&p)
        .await?;
    notifications::tick(&p, &bot).await?;
    assert_eq!(sent.load(Ordering::SeqCst), 1);
    notifications::tick(&p, &bot).await?;
    assert_eq!(sent.load(Ordering::SeqCst), 1);
    task.abort();
    // Changes under an accepted name are conflicts; absence is never a deletion from disk.
    std::fs::write(&movie, b"replaced")?;
    scanner::scan_once(&p, &c).await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM videos WHERE id=$1")
            .bind(own_video)
            .fetch_one(&p)
            .await?,
        "conflict"
    );
    std::fs::remove_file(other_folder.join("v.mp4"))?;
    scanner::scan_once(&p, &c).await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM videos WHERE id=$1")
            .bind(private_video)
            .fetch_one(&p)
            .await?,
        "missing"
    );
    // Imported files await SHA-256 receipts by default.
    c.require_receipts = true;
    let receipt_folder = temp.path().join("event/t1_o4/own");
    std::fs::create_dir_all(&receipt_folder)?;
    let rfile = receipt_folder.join("receipt.mp4");
    std::fs::copy(public_folder.join("v.mp4"), &rfile)?;
    scanner::scan_once(&p, &c).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM videos WHERE relative_path='event/t1_o4/own/receipt.mp4'"
        )
        .fetch_one(&p)
        .await?,
        0
    );
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(&rfile)?;
    let receipts = temp.path().join(".video-sync/receipts");
    std::fs::create_dir_all(&receipts)?;
    std::fs::write(
        receipts.join("r.json"),
        serde_json::to_vec(
            &serde_json::json!({"relative_path":"event/t1_o4/own/receipt.mp4","size_bytes":bytes.len(),"sha256":hex::encode(Sha256::digest(&bytes))}),
        )?,
    )?;
    scanner::scan_once(&p, &c).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM videos WHERE relative_path='event/t1_o4/own/receipt.mp4'"
        )
        .fetch_one(&p)
        .await?,
        1
    );
    p.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin_pool)
        .await?;
    admin_pool.close().await;
    Ok(())
}

#[derive(Default)]
struct PublishMock {
    indexed: bool,
    unavailable: bool,
    shares: Vec<serde_json::Value>,
    creates: usize,
    revokes: usize,
    library: String,
}
async fn find_asset(
    axum::extract::State(s): axum::extract::State<Arc<std::sync::Mutex<PublishMock>>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let s = s.lock().unwrap();
    if s.unavailable {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let path = body["filter"]["originalPath"]["eq"].as_str().unwrap();
    let id = if path.ends_with("a.mp4") {
        Uuid::from_u128(1)
    } else {
        Uuid::from_u128(2)
    };
    Ok(Json(
        serde_json::json!({"assets":{"items":if s.indexed{vec![serde_json::json!({"id":id,"originalPath":path,"libraryId":s.library,"isOffline":false,"isTrashed":false,"type":"VIDEO"})]}else{vec![]},"nextCursor":null}}),
    ))
}
async fn list_shares(
    axum::extract::State(s): axum::extract::State<Arc<std::sync::Mutex<PublishMock>>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!(s.lock().unwrap().shares))
}
async fn publish(
    axum::extract::State(s): axum::extract::State<Arc<std::sync::Mutex<PublishMock>>>,
    Json(b): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let mut s = s.lock().unwrap();
    s.creates += 1;
    let r = serde_json::json!({"id":Uuid::new_v4(),"key":format!("secret{}",s.creates),"expiresAt":b["expiresAt"],"description":b["description"],"assets":b["assetIds"].as_array().unwrap().iter().map(|id|serde_json::json!({"id":id})).collect::<Vec<_>>(),"allowDownload":b["allowDownload"],"allowUpload":b["allowUpload"],"showMetadata":b["showMetadata"]});
    s.shares.push(r.clone());
    Json(r)
}
async fn unpublish(
    axum::extract::State(s): axum::extract::State<Arc<std::sync::Mutex<PublishMock>>>,
) -> StatusCode {
    s.lock().unwrap().revokes += 1;
    StatusCode::NO_CONTENT
}
#[tokio::test]
async fn delivery_waits_recovers_chunks_and_revokes() -> Result<()> {
    use axum::routing::delete;
    use fpv_catalog_bot::{delivery, immich::ImmichClient};
    let Some((p, schema, admin_pool)) = pool().await? else {
        return Ok(());
    };
    let temp = tempfile::tempdir()?;
    let mut c = config(temp.path().into());
    c.share_chunk_size = 1;
    db::bootstrap(&p, 7777).await?;
    let admin = db::user(&p, 7777).await?.unwrap();
    let viewer = add(&p, admin.id, 1111, "viewer", false).await?;
    let pilot = add(&p, admin.id, 2222, "pilot", true).await?;
    let state = Arc::new(std::sync::Mutex::new(PublishMock {
        library: c.immich_library_id.clone(),
        ..Default::default()
    }));
    let app=Router::new().route("/api/search/metadata",post(find_asset)).route("/api/shared-links",post(publish).get(list_shares)).route("/api/shared-links/:id",delete(unpublish)).route("/botTEST/SendMessage",post(||async{Json(serde_json::json!({"ok":true,"result":{"message_id":1,"date":0,"chat":{"id":1111,"type":"private"},"text":"ok"}}))})).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let immich = ImmichClient::new(
        format!("http://{addr}/api"),
        "https://photos.example".into(),
        "test".into(),
        c.immich_container_root.clone(),
        c.immich_library_id.clone(),
    );
    let bot = teloxide::Bot::new("TEST").set_api_url(format!("http://{addr}").parse()?);
    let event: i64 = sqlx::query_scalar("INSERT INTO events(name) VALUES('event') RETURNING id")
        .fetch_one(&p)
        .await?;
    let mut ids = vec![];
    for name in ["a.mp4", "b.mp4"] {
        let id:i64=sqlx::query_scalar("INSERT INTO videos(relative_path,size_bytes,mtime_ns,event_id,pilot_id,state,first_classified_at) VALUES($1,100,1,$2,$3,'classified',now()) RETURNING id").bind(format!("event/{name}")).bind(event).bind(pilot).fetch_one(&p).await?;
        ids.push(id);
    }
    let u = db::user(&p, 1111).await?.unwrap();
    assert_eq!(u.id, viewer);
    let job = db::enqueue(&p, &u, 1111, &ids, "immich").await?;
    delivery::tick(&p, &c, &immich, &bot).await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM delivery_jobs WHERE id=$1")
            .bind(job)
            .fetch_one(&p)
            .await?,
        "waiting_for_immich"
    );
    assert_eq!(state.lock().unwrap().creates, 0);
    state.lock().unwrap().unavailable = true;
    sqlx::query("UPDATE delivery_jobs SET next_attempt_at=now() WHERE id=$1")
        .bind(job)
        .execute(&p)
        .await?;
    delivery::tick(&p, &c, &immich, &bot).await?;
    assert_eq!(state.lock().unwrap().creates, 0);
    {
        let mut s = state.lock().unwrap();
        s.indexed = true;
        s.unavailable = false;
    }
    sqlx::query("UPDATE delivery_jobs SET next_attempt_at=now() WHERE id=$1")
        .bind(job)
        .execute(&p)
        .await?;
    delivery::tick(&p, &c, &immich, &bot).await?;
    assert_eq!(state.lock().unwrap().creates, 2);
    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT completed_count FROM delivery_jobs WHERE id=$1")
            .bind(job)
            .fetch_one(&p)
            .await?,
        2
    );
    // Simulate a crash after Telegram accepts the URL but before the local progress commit.
    sqlx::query("UPDATE delivery_job_videos SET delivered=false WHERE job_id=$1")
        .bind(job)
        .execute(&p)
        .await?;
    sqlx::query("UPDATE delivery_jobs SET state='running',next_attempt_at=now() WHERE id=$1")
        .bind(job)
        .execute(&p)
        .await?;
    delivery::tick(&p, &c, &immich, &bot).await?;
    assert_eq!(state.lock().unwrap().creates, 2);
    // Recover a remotely created link even when its first POST response was lost.
    let remote = state.lock().unwrap().shares[0].clone();
    let asset_id = remote["assets"][0]["id"].as_str().unwrap().parse()?;
    immich
        .create_share(
            &[asset_id],
            chrono::Utc::now() + chrono::Duration::days(7),
            remote["description"].as_str().unwrap().into(),
        )
        .await?;
    assert_eq!(state.lock().unwrap().creates, 2);
    sqlx::query("UPDATE users SET is_public=false WHERE id=$1")
        .bind(pilot)
        .execute(&p)
        .await?;
    // Revocation remains available in indexing-only mode and while the delivery lock is held.
    let mut delivery_lock = p.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(76120303)")
        .execute(&mut *delivery_lock)
        .await?;
    delivery::maintain_links(&p, &immich).await?;
    delivery_lock.commit().await?;
    assert_eq!(state.lock().unwrap().revokes, 2);
    // A cancelled request produces no published links.
    sqlx::query("UPDATE users SET is_public=true WHERE id=$1")
        .bind(pilot)
        .execute(&p)
        .await?;
    let cancelled = db::enqueue(&p, &u, 1111, &ids, "immich").await?;
    sqlx::query("UPDATE delivery_jobs SET state='cancelled' WHERE id=$1")
        .bind(cancelled)
        .execute(&p)
        .await?;
    delivery::tick(&p, &c, &immich, &bot).await?;
    assert_eq!(state.lock().unwrap().creates, 2);
    // An old job starts its wait timeout only on the first failed attempt.
    state.lock().unwrap().indexed = false;
    let timeout = db::enqueue(&p, &u, 1111, &ids, "immich").await?;
    sqlx::query("UPDATE delivery_jobs SET created_at=now()-interval '31 minutes' WHERE id=$1")
        .bind(timeout)
        .execute(&p)
        .await?;
    delivery::tick(&p, &c, &immich, &bot).await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM delivery_jobs WHERE id=$1")
            .bind(timeout)
            .fetch_one(&p)
            .await?,
        "waiting_for_immich"
    );
    // The wait start is persisted, so restarts do not extend the timeout.
    sqlx::query("UPDATE delivery_jobs SET wait_started_at=now()-interval '31 minutes',next_attempt_at=now() WHERE id=$1").bind(timeout).execute(&p).await?;
    delivery::tick(&p, &c, &immich, &bot).await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM delivery_jobs WHERE id=$1")
            .bind(timeout)
            .fetch_one(&p)
            .await?,
        "failed"
    );
    task.abort();
    p.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin_pool)
        .await?;
    Ok(())
}

#[test]
fn transport_errors_redact_secrets() {
    let c = config(std::path::PathBuf::from("/tmp"));
    let message = format!(
        "url https://api.telegram.org/bot{}/sendMessage; key={}",
        c.telegram_bot_token, c.immich_api_key
    );
    let text = c.redact(&message);
    assert!(!text.contains(&c.telegram_bot_token));
    assert!(!text.contains(&c.immich_api_key));
    assert!(text.contains("[REDACTED]"));
}

#[tokio::test]
async fn identity_changes_revoke_links_cancel_jobs_and_preserve_subscriptions() -> Result<()> {
    let Some((p, schema, admin_pool)) = pool().await? else {
        return Ok(());
    };
    db::bootstrap(&p, 7777).await?;
    let admin = db::user(&p, 7777).await?.unwrap();
    let pilot = add(&p, admin.id, 1111, "pilot", true).await?;
    // Custom admin subscriptions survive subsequent edits and bootstrap.
    sqlx::query("DELETE FROM notification_subscriptions WHERE user_id=$1")
        .bind(admin.id)
        .execute(&p)
        .await?;
    let u = db::user(&p, 1111).await?.unwrap();
    let event: i64 = sqlx::query_scalar("INSERT INTO events(name) VALUES('identity') RETURNING id")
        .fetch_one(&p)
        .await?;
    let video: i64 = sqlx::query_scalar("INSERT INTO videos(relative_path,size_bytes,mtime_ns,event_id,pilot_id,state) VALUES('identity/v.mp4',100,1,$1,$2,'classified') RETURNING id")
        .bind(event).bind(pilot).fetch_one(&p).await?;
    let job = db::enqueue(&p, &u, 1111, &[video], "immich").await?;
    let link: i64 = sqlx::query_scalar("INSERT INTO immich_shared_links(immich_id,immich_key,user_id,job_id,expires_at) VALUES($1,'test-key',$2,$3,now()+interval '7 days') RETURNING id")
        .bind(Uuid::new_v4()).bind(pilot).bind(job).fetch_one(&p).await?;
    sqlx::query("INSERT INTO immich_shared_link_videos VALUES($1,$2)")
        .bind(link)
        .bind(video)
        .execute(&p)
        .await?;
    let update: UserInput = serde_json::from_value(
        serde_json::json!({"id":pilot,"telegram_id":2222,"nickname":"pilot"}),
    )?;
    db::save_user(&p, admin.id, update, 7777).await?;
    assert!(db::user(&p, 1111).await?.is_none());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM delivery_jobs WHERE id=$1")
            .bind(job)
            .fetch_one(&p)
            .await?,
        "cancelled"
    );
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT revoke_requested FROM immich_shared_links WHERE id=$1"
        )
        .bind(link)
        .fetch_one(&p)
        .await?
    );
    db::bootstrap(&p, 7777).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM notification_subscriptions WHERE user_id=$1"
        )
        .bind(admin.id)
        .fetch_one(&p)
        .await?,
        0
    );
    let new_pilot = add(&p, admin.id, 3333, "new pilot", true).await?;
    assert!(sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM notification_subscriptions WHERE user_id=$1 AND pilot_id=$2)").bind(admin.id).bind(new_pilot).fetch_one(&p).await?);
    assert!(!sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM notification_subscriptions WHERE user_id=$1 AND pilot_id=$2)").bind(pilot).bind(new_pilot).fetch_one(&p).await?);
    // A problem recurring after recovery must produce a fresh system notification.
    notifications::alert(&p, "immich_revoke", "unavailable").await?;
    sqlx::query("INSERT INTO system_alert_deliveries VALUES($1,'immich_revoke')")
        .bind(admin.id)
        .execute(&p)
        .await?;
    notifications::alert(&p, "immich_revoke", "still unavailable").await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_alert_deliveries")
            .fetch_one(&p)
            .await?,
        1
    );
    sqlx::query("UPDATE system_alerts SET active=false WHERE key='immich_revoke'")
        .execute(&p)
        .await?;
    notifications::alert(&p, "immich_revoke", "unavailable again").await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_alert_deliveries")
            .fetch_one(&p)
            .await?,
        0
    );
    p.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin_pool)
        .await?;
    Ok(())
}
