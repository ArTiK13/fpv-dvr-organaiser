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

fn telegram_mock(capture: Arc<std::sync::Mutex<Vec<serde_json::Value>>>) -> Router {
    Router::new().route("/botTEST/:method", post(move |axum::extract::Path(method): axum::extract::Path<String>, Json(mut body): Json<serde_json::Value>| {
        let mut calls = capture.lock().unwrap();
        body["method"] = method.clone().into();
        let id = body["message_id"].as_i64().unwrap_or(1000 + calls.len() as i64);
        body["result_message_id"] = id.into();
        let text = body["text"].as_str().unwrap_or("ok").to_string();
        let chat = body["chat_id"].as_i64().unwrap_or(7777);
        calls.push(body);
        async move {
            let result = match method.as_str() {
                "SendMessage" | "EditMessageText" => serde_json::json!({"message_id":id,"date":0,"chat":{"id":chat,"type":"private"},"text":text}),
                "DeleteMessage" | "AnswerCallbackQuery" => serde_json::json!(true),
                _ => panic!("Unexpected Telegram call: {method}"),
            };
            Json(serde_json::json!({"ok":true,"result":result}))
        }
    }))
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
    let app = telegram_mock(capture);
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
    let first_calls = sent.lock().unwrap().clone();
    let created = first_calls
        .iter()
        .filter(|m| m["method"] == "SendMessage")
        .collect::<Vec<_>>();
    assert_eq!(
        created.len(),
        1,
        "one message for the entire user wizard including confirmation"
    );
    let completed_form = created[0]["result_message_id"].clone();
    assert!(first_calls
        .iter()
        .filter(|m| m["method"] == "EditMessageText")
        .all(|m| m["message_id"] == completed_form));
    assert_eq!(
        first_calls
            .iter()
            .filter(|m| m["method"] == "DeleteMessage")
            .count(),
        4,
        "only the four supplied text answers are removed"
    );
    let completed_at = first_calls.len();
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
    // Wait for background reclassification, then keep our synthetic fixture stable.
    let mut scan_guard = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(76120301)")
        .execute(&mut *scan_guard)
        .await?;
    let event: i64 =
        sqlx::query_scalar("INSERT INTO events(name) VALUES('test_event') RETURNING id")
            .fetch_one(&pool)
            .await?;
    let video: i64 = sqlx::query_scalar("INSERT INTO videos(relative_path,size_bytes,mtime_ns,event_id,pilot_id,state) VALUES('test_event/T1_N3/pilot/test.mp4',1234,1,$1,$2,'classified') RETURNING id").bind(event).bind(added.id).fetch_one(&pool).await?;
    handle(&bot, ChatId(7777), &state, &user, Command::Catalog).await?;
    handle(&bot, ChatId(7777), &state, &user, Command::Info(video)).await?;
    let card = sent.lock().unwrap().last().unwrap()["result_message_id"].clone();
    let card_at = sent.lock().unwrap().len();
    for (id, command) in [
        (80, Command::Filters),
        (81, Command::Menu),
        (82, Command::Admin),
    ] {
        let msg: Message = serde_json::from_value(
            serde_json::json!({"message_id":id,"date":0,"chat":{"id":7777,"type":"private"},"from":{"id":7777,"is_bot":false,"first_name":"Admin"},"text":"navigation"}),
        )?;
        message(bot.clone(), msg, command, state.clone()).await?;
        assert!(sent
            .lock()
            .unwrap()
            .iter()
            .any(|m| m["method"] == "DeleteMessage" && m["message_id"] == id));
    }

    for action in [
        "filters",
        "pick:device",
        "set:device:1",
        "next",
        "reset",
        "notify:10:00",
        "notify:off",
        "subscriptions",
    ] {
        handle_callback(&bot, ChatId(7777), &state, &user, action).await?;
    }
    let action = format!("subscription:{}", added.id);
    handle_callback(&bot, ChatId(7777), &state, &user, &action).await?;
    prepare(&bot, ChatId(7777), &state, &user, vec![video], "immich").await?;
    let nonce = state.sessions.lock().await[&user.id].nonce.unwrap();
    handle_callback(
        &bot,
        ChatId(7777),
        &state,
        &user,
        &format!("discard:{nonce}"),
    )
    .await?;
    start_user_form(&bot, ChatId(7777), &state, &user, None).await?;
    let form = state.sessions.lock().await[&user.id]
        .user_form
        .clone()
        .unwrap();
    let draft_calls = sent.lock().unwrap().len();
    // Invalid input stays on the same step and replaces the prompt with a helpful error.
    let invalid: Message = serde_json::from_value(
        serde_json::json!({"message_id":45,"date":0,"chat":{"id":7777,"type":"private"},"from":{"id":7777,"is_bot":false,"first_name":"Admin"},"text":"not-an-id"}),
    )?;
    free_message(bot.clone(), invalid, state.clone()).await?;
    assert_eq!(
        state.sessions.lock().await[&user.id]
            .user_form
            .as_ref()
            .unwrap()
            .token,
        form.token
    );
    assert!(sent.lock().unwrap()[draft_calls..]
        .iter()
        .all(|m| m["method"] != "SendMessage"));
    handle_callback(
        &bot,
        ChatId(7777),
        &state,
        &user,
        &format!("uf:{}:cancel", form.token),
    )
    .await?;
    let calls = sent.lock().unwrap().clone();
    assert!(
        calls[card_at..].iter().all(|m| m["message_id"] != card),
        "requested video card must never be edited or deleted"
    );
    assert!(
        calls[completed_at..]
            .iter()
            .all(|m| m["message_id"] != completed_form),
        "confirmed user card must remain in history"
    );
    // All search/filter/page navigation edits the original search message.
    let search_messages = calls
        .iter()
        .filter(|m| {
            m["method"] == "SendMessage"
                && m["text"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with("🔎 Найдено видео")
        })
        .count();
    assert_eq!(search_messages, 1);
    let notification_messages = calls
        .iter()
        .filter(|m| {
            m["method"] == "SendMessage"
                && m["text"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with("Ежедневный дайджест")
        })
        .count();
    assert_eq!(notification_messages, 1);
    scan_guard.rollback().await?;
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

#[tokio::test]
async fn panels_recover_deleted_controls_without_duplicating_on_transport_errors() -> Result<()> {
    let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let failure = Arc::new(std::sync::Mutex::new(None::<(u16, String)>));
    let log = calls.clone();
    let fail = failure.clone();
    let app = Router::new().route("/botTEST/:method",post(move |axum::extract::Path(method): axum::extract::Path<String>, Json(body): Json<serde_json::Value>| {
        let mut calls = log.lock().unwrap();
        calls.push(method.clone());
        let problem = if method == "EditMessageText" { fail.lock().unwrap().take() } else { None };
        let (status, result) = if let Some((status,description)) = problem {
            (axum::http::StatusCode::from_u16(status).unwrap(),serde_json::json!({"ok":false,"error_code":status,"description":description}))
        } else {
            let id = body["message_id"].as_i64().unwrap_or(1000+calls.len() as i64);
            (axum::http::StatusCode::OK,serde_json::json!({"ok":true,"result":{"message_id":id,"date":0,"chat":{"id":7777,"type":"private"},"text":body["text"]}}))
        };
        async move {(status,Json(result))}
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let bot = Bot::new("TEST").set_api_url(format!("http://{addr}").parse()?);
    let temp = tempfile::tempdir()?;
    let state = BotState::new(
        PgPool::connect_lazy("postgres://unused:unused@127.0.0.1/unused")?,
        Arc::new(config(temp.path().into())),
    );
    let chat = ChatId(7777);
    let a = show_panel(
        &bot,
        chat,
        &state,
        PanelKind::Search,
        "First",
        K::new(Vec::<Vec<B>>::new()),
    );
    let b = show_panel(
        &bot,
        chat,
        &state,
        PanelKind::Search,
        "Second",
        K::new(Vec::<Vec<B>>::new()),
    );
    let (a, b) = tokio::join!(a, b);
    a?;
    b?;
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.as_str() == "SendMessage")
            .count(),
        1
    );
    *failure.lock().unwrap() = Some((400, "Bad Request: message to edit not found".into()));
    show_panel(
        &bot,
        chat,
        &state,
        PanelKind::Search,
        "Recreated",
        K::new(Vec::<Vec<B>>::new()),
    )
    .await?;
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.as_str() == "SendMessage")
            .count(),
        2
    );
    *failure.lock().unwrap()=Some((400,"Bad Request: message is not modified: specified new message content and reply markup are exactly the same as a current content and reply markup of the message".into()));
    show_panel(
        &bot,
        chat,
        &state,
        PanelKind::Search,
        "Recreated",
        K::new(Vec::<Vec<B>>::new()),
    )
    .await?;
    *failure.lock().unwrap() = Some((503, "Service Unavailable".into()));
    assert!(show_panel(
        &bot,
        chat,
        &state,
        PanelKind::Search,
        "Temporary failure",
        K::new(Vec::<Vec<B>>::new())
    )
    .await
    .is_err());
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.as_str() == "SendMessage")
            .count(),
        2
    );
    server.abort();
    Ok(())
}
