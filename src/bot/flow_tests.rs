use super::*;
use axum::{routing::post, Json, Router};
use std::time::Duration;

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
        immich_show_metadata: false,
        telegram_bot_token: "test".into(),
        bootstrap_admin_chat_id: 7777,
        default_user_id: None,
        scan_interval: Duration::from_secs(60),
        bind_addr: "127.0.0.1:0".into(),
        fallback_base_url: Some("https://fpv.example".into()),
        telegram_limit: 49_000_000,
        telegram_upload_timeout: Duration::from_secs(600),
        share_chunk_size: 500,
        require_receipts: false,
        stable_seconds: 0,
        delivery_enabled: true,
        notifications_enabled: true,
    }
}

#[tokio::test]
async fn interactive_user_flow_saves_only_on_confirmation_and_rejects_stale_buttons() -> Result<()>
{
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        return Ok(());
    };
    let admin_pool = PgPool::connect(&url).await?;
    let schema = format!("fpvform_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin_pool)
        .await?;
    let path = schema.clone();
    let pool = sqlx::postgres::PgPoolOptions::new()
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
    sqlx::migrate!().run(&pool).await?;
    db::bootstrap(&pool, 7777).await?;
    let user = db::user(&pool, 7777).await?.unwrap();
    let temp = tempfile::tempdir()?;
    let state = BotState::new(pool.clone(), Arc::new(config(temp.path().into())));
    let sent = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let capture = sent.clone();
    let app = Router::new().route("/botTEST/SendMessage", post(move |Json(body): Json<serde_json::Value>| {
        capture.lock().unwrap().push(body);
        async { Json(serde_json::json!({"ok":true,"result":{"message_id":1,"date":0,"chat":{"id":7777,"type":"private"},"text":"ok"}})) }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let bot = Bot::new("TEST").set_api_url(format!("http://{addr}").parse()?);
    handle(
        &bot,
        ChatId(7777),
        &state,
        &user,
        Command::User("1004106925".into()),
    )
    .await?;
    let old_token = state.sessions.lock().await[&user.id]
        .user_form
        .as_ref()
        .unwrap()
        .token;
    for text in [
        "Константин",
        "kostyanbtw",
        "@kostyanbtw",
        "kostyanbtw, kostyan",
    ] {
        let msg: Message = serde_json::from_value(
            serde_json::json!({"message_id":2,"date":0,"chat":{"id":7777,"type":"private"},"from":{"id":7777,"is_bot":false,"first_name":"Admin"},"text":text}),
        )?;
        free_message(bot.clone(), msg, state.clone()).await?;
    }
    assert!(handle_callback(
        &bot,
        ChatId(7777),
        &state,
        &user,
        &format!("uf:{old_token}:save")
    )
    .await
    .is_err());
    for action in ["pilot", "private"] {
        let token = state.sessions.lock().await[&user.id]
            .user_form
            .as_ref()
            .unwrap()
            .token;
        handle_callback(
            &bot,
            ChatId(7777),
            &state,
            &user,
            &format!("uf:{token}:{action}"),
        )
        .await?;
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
            .fetch_one(&pool)
            .await?,
        1
    );
    let token = state.sessions.lock().await[&user.id]
        .user_form
        .as_ref()
        .unwrap()
        .token;
    let save = format!("uf:{token}:save");
    handle_callback(&bot, ChatId(7777), &state, &user, &save).await?;
    assert!(handle_callback(&bot, ChatId(7777), &state, &user, &save)
        .await
        .is_err());
    let added = db::user(&pool, 1004106925).await?.unwrap();
    assert!(!added.is_admin && !added.is_public);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM user_file_aliases WHERE user_id=$1")
            .bind(added.id)
            .fetch_one(&pool)
            .await?,
        2
    );
    assert!(sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM notification_subscriptions WHERE user_id=$1 AND pilot_id=$2)").bind(user.id).bind(added.id).fetch_one(&pool).await?);
    handle(
        &bot,
        ChatId(7777),
        &state,
        &user,
        Command::List("users".into()),
    )
    .await?;
    handle(&bot, ChatId(7777), &state, &user, Command::Catalog).await?;
    sqlx::query("INSERT INTO devices(name,type) VALUES('T1','plane')")
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO video_systems(name,with_osd) VALUES('N3',true)")
        .execute(&pool)
        .await?;
    handle(
        &bot,
        ChatId(7777),
        &state,
        &user,
        Command::List("devices".into()),
    )
    .await?;
    handle(
        &bot,
        ChatId(7777),
        &state,
        &user,
        Command::List("systems".into()),
    )
    .await?;
    edit_user_form(&bot, ChatId(7777), &state, &user, added.id).await?;
    let token = state.sessions.lock().await[&user.id]
        .user_form
        .as_ref()
        .unwrap()
        .token;
    handle_callback(
        &bot,
        ChatId(7777),
        &state,
        &user,
        &format!("uf:{token}:edit_nick"),
    )
    .await?;
    let msg: Message = serde_json::from_value(
        serde_json::json!({"message_id":3,"date":0,"chat":{"id":7777,"type":"private"},"from":{"id":7777,"is_bot":false,"first_name":"Admin"},"text":"Kostyan"}),
    )?;
    free_message(bot.clone(), msg, state.clone()).await?;
    let token = state.sessions.lock().await[&user.id]
        .user_form
        .as_ref()
        .unwrap()
        .token;
    handle_callback(
        &bot,
        ChatId(7777),
        &state,
        &user,
        &format!("uf:{token}:save"),
    )
    .await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT nickname FROM users WHERE id=$1")
            .bind(added.id)
            .fetch_one(&pool)
            .await?,
        "Kostyan"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
            .fetch_one(&pool)
            .await?,
        2
    );
    let messages = sent.lock().unwrap().clone();
    assert!(messages.iter().any(|m| m["text"]
        .as_str()
        .unwrap_or("")
        .contains("Пользователь добавлен")));
    assert!(messages
        .iter()
        .all(|m| !m["text"].as_str().unwrap_or("").contains("telegram_id")));
    assert!(messages
        .iter()
        .any(|m| m["text"].as_str().unwrap_or("").contains("Тип: самолёт")));
    assert!(messages.iter().any(|m| m["text"]
        .as_str()
        .unwrap_or("")
        .contains("OSD (данные полёта на видео): есть")));
    // Current write permissions must win over a previously issued admin form.
    let input: db::UserInput =
        serde_json::from_value(serde_json::json!({"telegram_id":9999,"nickname":"forbidden"}))?;
    sqlx::query("UPDATE users SET is_admin=false WHERE id=$1")
        .bind(user.id)
        .execute(&pool)
        .await?;
    assert!(db::save_user(&pool, user.id, input, 7777).await.is_err());
    assert!(db::user(&pool, 9999).await?.is_none());
    server.abort();
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin_pool)
        .await?;
    admin_pool.close().await;
    Ok(())
}
