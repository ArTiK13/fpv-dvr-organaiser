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
    // Inline navigation reuses the panel; the reply-keyboard Filters command reopens it.
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
        let problem = if matches!(method.as_str(), "EditMessageText" | "SendMessage") { fail.lock().unwrap().take() } else { None };
        let (status, result) = if let Some((status,description)) = problem {
            (axum::http::StatusCode::from_u16(status).unwrap(),serde_json::json!({"ok":false,"error_code":status,"description":description}))
        } else {
            let id = body["message_id"].as_i64().unwrap_or(1000+calls.len() as i64);
            (axum::http::StatusCode::OK,serde_json::json!({"ok":true,"result": if method == "DeleteMessage" { serde_json::json!(true) } else { serde_json::json!({"message_id":id,"date":0,"chat":{"id":7777,"type":"private"},"text":body["text"]}) }}))
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
    // Reopening must not delete the previous control if sending the new one fails.
    panels::reopen_panel(chat, &state, PanelKind::Search).await;
    *failure.lock().unwrap() = Some((503, "Service Unavailable".into()));
    assert!(show_panel(
        &bot,
        chat,
        &state,
        PanelKind::Search,
        "Reopen",
        K::new(Vec::<Vec<B>>::new())
    )
    .await
    .is_err());
    assert!(!calls.lock().unwrap().iter().any(|m| m == "DeleteMessage"));
    show_panel(
        &bot,
        chat,
        &state,
        PanelKind::Search,
        "Reopen",
        K::new(Vec::<Vec<B>>::new()),
    )
    .await?;
    let methods = calls.lock().unwrap().clone();
    assert_eq!(
        &methods[methods.len() - 2..],
        &["SendMessage", "DeleteMessage"]
    );
    show_panel(
        &bot,
        chat,
        &state,
        PanelKind::Search,
        "Next step",
        K::new(Vec::<Vec<B>>::new()),
    )
    .await?;
    assert_eq!(calls.lock().unwrap().last().unwrap(), "EditMessageText");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn search_multiselect_reset_pagination_and_visibility() -> Result<()> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        return Ok(());
    };
    let admin_pool = PgPool::connect(&url).await?;
    let schema = format!("fpvsearch_{}", Uuid::new_v4().simple());
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
    for (tg, name, public) in [
        (7777_i64, "Viewer", false),
        (8888, "Public", true),
        (9999, "Private", false),
    ] {
        sqlx::query("INSERT INTO users(telegram_id,nickname,is_public) VALUES($1,$2,$3)")
            .bind(tg)
            .bind(name)
            .bind(public)
            .execute(&pool)
            .await?;
    }
    let user = db::user(&pool, 7777).await?.unwrap();
    let public = db::user(&pool, 8888).await?.unwrap();
    let private = db::user(&pool, 9999).await?.unwrap();
    let mut devices = vec![];
    for i in 0..14 {
        devices.push(
            sqlx::query_scalar::<_, i64>(
                "INSERT INTO devices(name,type) VALUES($1,'plane') RETURNING id",
            )
            .bind(format!("Device {i}"))
            .fetch_one(&pool)
            .await?,
        );
    }
    let mut systems = vec![];
    for (name, osd) in [("N3", true), ("O4", false)] {
        systems.push(
            sqlx::query_scalar::<_, i64>(
                "INSERT INTO video_systems(name,with_osd) VALUES($1,$2) RETURNING id",
            )
            .bind(name)
            .bind(osd)
            .fetch_one(&pool)
            .await?,
        );
    }
    let event: i64 = sqlx::query_scalar("INSERT INTO events(name) VALUES('test') RETURNING id")
        .fetch_one(&pool)
        .await?;
    let mut videos = vec![];
    for (i, (device, system, pilot, bytes)) in [
        (devices[0], systems[0], user.id, 100_i64),
        (devices[1], systems[1], public.id, 200),
        (devices[2], systems[0], user.id, 300),
        (devices[0], systems[0], private.id, 400),
        (devices[0], systems[1], user.id, 500),
        (devices[1], systems[0], public.id, 600),
    ]
    .into_iter()
    .enumerate()
    {
        videos.push(sqlx::query_scalar::<_, i64>("INSERT INTO videos(relative_path,size_bytes,mtime_ns,event_id,device_id,video_system_id,pilot_id,state) VALUES($1,$2,1,$3,$4,$5,$6,'classified') RETURNING id")
            .bind(format!("test/video{i}.mp4")).bind(bytes).bind(event).bind(device).bind(system).bind(pilot)
            .fetch_one(&pool).await?);
    }
    let temp = tempfile::tempdir()?;
    let state = BotState::new(pool.clone(), Arc::new(config(temp.path().into())));
    let sent = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let app = telegram_mock(sent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let bot = Bot::new("TEST").set_api_url(format!("http://{addr}").parse()?);
    let last = || sent.lock().unwrap().last().unwrap().clone();
    let has_button = |m: &serde_json::Value, text: &str| {
        m["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|row| row.as_array().unwrap())
            .any(|b| b["text"].as_str().unwrap().contains(text))
    };
    handle(&bot, ChatId(7777), &state, &user, Command::Catalog).await?;
    assert!(has_button(&last(), "Сбросить все фильтры"));
    let panel_id = last()["result_message_id"].clone();
    handle_callback(&bot, ChatId(7777), &state, &user, "pick:device").await?;
    assert!(has_button(&last(), "⬜️ Device 0"));
    assert!(has_button(&last(), "→"));
    handle_callback(&bot, ChatId(7777), &state, &user, "pickpage:device:1").await?;
    assert!(has_button(&last(), "Device 13"));
    handle_callback(
        &bot,
        ChatId(7777),
        &state,
        &user,
        &format!("set:device:{}:1", devices[13]),
    )
    .await?;
    assert!(has_button(&last(), "✅ Device 13"));
    assert!(last()["text"].as_str().unwrap().contains("страница 2 из 2"));
    handle_callback(&bot, ChatId(7777), &state, &user, "clear:device").await?;
    for id in &devices[..2] {
        handle_callback(
            &bot,
            ChatId(7777),
            &state,
            &user,
            &format!("set:device:{id}:0"),
        )
        .await?;
    }
    assert!(has_button(&last(), "✅ Device 0"));
    assert!(has_button(&last(), "✅ Device 1"));
    assert!(last()["text"].as_str().unwrap().contains("Выбрано: 2"));
    for (kind, ids) in [
        ("system", systems.clone()),
        ("pilot", vec![user.id, public.id]),
    ] {
        for id in ids {
            handle_callback(
                &bot,
                ChatId(7777),
                &state,
                &user,
                &format!("set:{kind}:{id}:0"),
            )
            .await?;
        }
    }
    assert!(!has_button(&last(), "Private"));
    assert!(handle_callback(
        &bot,
        ChatId(7777),
        &state,
        &user,
        &format!("set:pilot:{}:0", private.id)
    )
    .await
    .is_err());
    handle_callback(&bot, ChatId(7777), &state, &user, "filters").await?;
    assert!(last()["text"]
        .as_str()
        .unwrap()
        .contains("Аппараты: Device 0, Device 1"));
    assert!(last()["text"]
        .as_str()
        .unwrap()
        .contains("Видеосистемы: N3, O4"));
    assert!(has_button(&last(), "Аппараты: 2"));
    handle_callback(&bot, ChatId(7777), &state, &user, "search").await?;
    let f = state.sessions.lock().await[&user.id].filters.clone();
    let result = db::search(&pool, &user, &f).await?;
    assert_eq!((result.total, result.size_bytes), (4, 1400));
    let expected = vec![videos[0], videos[1], videos[4], videos[5]];
    assert_eq!(db::selection(&pool, &user, &f).await?, expected);
    let mut listed: Vec<_> = result.videos.into_iter().map(|v| v.id).collect();
    listed.sort_unstable();
    assert_eq!(listed, expected);
    // Even a direct JSON filter cannot expose a private pilot.
    let mut forged = f.clone();
    forged.pilot.push(private.id);
    assert_eq!(db::selection(&pool, &user, &forged).await?, expected);
    // Booleans still combine with the OR selections of other categories.
    handle_callback(&bot, ChatId(7777), &state, &user, "toggle:osd").await?;
    assert!(has_button(&last(), "OSD: есть"));
    assert_eq!(
        db::search(&pool, &user, &state.sessions.lock().await[&user.id].filters)
            .await?
            .total,
        2
    );
    for action in ["toggle:osd", "toggle:osd"] {
        handle_callback(&bot, ChatId(7777), &state, &user, action).await?;
    }
    handle_callback(
        &bot,
        ChatId(7777),
        &state,
        &user,
        &format!("set:device:{}:0", devices[1]),
    )
    .await?;
    assert!(has_button(&last(), "⬜️ Device 1"));
    assert!(has_button(&last(), "✅ Device 0"));
    handle_callback(&bot, ChatId(7777), &state, &user, "clear:device").await?;
    let f = state.sessions.lock().await[&user.id].filters.clone();
    assert!(f.device.is_empty());
    assert_eq!(f.system, systems);
    assert_eq!(f.pilot, vec![user.id, public.id]);
    assert_eq!(db::search(&pool, &user, &f).await?.total, 5);
    // Clearing the final checkbox means 'any', not an empty result set.
    for action in [
        format!("set:device:{}:0", devices[0]),
        format!("set:device:{}:0", devices[0]),
    ] {
        handle_callback(&bot, ChatId(7777), &state, &user, &action).await?;
    }
    assert!(state.sessions.lock().await[&user.id]
        .filters
        .device
        .is_empty());
    {
        let mut sessions = state.sessions.lock().await;
        let f = &mut sessions.get_mut(&user.id).unwrap().filters;
        f.text = Some("narrow search".into());
        f.from = Some("2026-01-01".parse()?);
        f.to = Some("2026-12-31".parse()?);
        f.colored = Some(true);
        f.page = 10;
    }
    for action in [
        "clear:system",
        "clear:pilot",
        "clear:date",
        "clear:text",
        "resetfilters",
    ] {
        handle_callback(&bot, ChatId(7777), &state, &user, action).await?;
    }
    assert!(has_button(&last(), "Аппараты: все"));
    assert!(has_button(&last(), "OSD: любые"));
    assert_eq!(
        serde_json::to_value(&state.sessions.lock().await[&user.id].filters)?,
        serde_json::to_value(Filters::default())?
    );
    // Reset directly from results must remove every category, dates, text and pagination.
    state
        .sessions
        .lock()
        .await
        .get_mut(&user.id)
        .unwrap()
        .filters = Filters {
        device: devices[..2].to_vec(),
        system: systems.clone(),
        pilot: vec![user.id],
        from: Some("2026-01-01".parse()?),
        to: Some("2026-12-31".parse()?),
        text: Some("narrow".into()),
        osd: Some(false),
        colored: Some(true),
        page: 10,
    };
    handle_callback(&bot, ChatId(7777), &state, &user, "reset").await?;
    assert_eq!(
        serde_json::to_value(&state.sessions.lock().await[&user.id].filters)?,
        serde_json::to_value(Filters::default())?
    );
    assert!(last()["text"]
        .as_str()
        .unwrap()
        .contains("Найдено видео: 5"));
    assert!(last()["text"].as_str().unwrap().contains("Страница 1"));
    // A disabled directory entry from an old picker cannot be selected.
    sqlx::query("UPDATE devices SET active=false WHERE id=$1")
        .bind(devices[0])
        .execute(&pool)
        .await?;
    assert!(handle_callback(
        &bot,
        ChatId(7777),
        &state,
        &user,
        &format!("set:device:{}:0", devices[0])
    )
    .await
    .is_err());
    let calls = sent.lock().unwrap().clone();
    assert_eq!(
        calls
            .iter()
            .filter(|m| m["method"] == "SendMessage")
            .count(),
        1
    );
    assert!(calls
        .iter()
        .filter(|m| m["method"] == "EditMessageText")
        .all(|m| m["message_id"] == panel_id));
    server.abort();
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin_pool)
        .await?;
    admin_pool.close().await;
    Ok(())
}

#[tokio::test]
async fn search_pages_stay_in_bounds_and_sections_reopen_visibly() -> Result<()> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        return Ok(());
    };
    let admin_pool = PgPool::connect(&url).await?;
    let schema = format!("fpvpages_{}", Uuid::new_v4().simple());
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
    let event: i64 = sqlx::query_scalar("INSERT INTO events(name) VALUES('test') RETURNING id")
        .fetch_one(&pool)
        .await?;
    let ids: Vec<i64> = sqlx::query_scalar("INSERT INTO videos(relative_path,size_bytes,mtime_ns,event_id,pilot_id,state) SELECT 'test/video' || n || '.mp4',100,1,$1,$2,'classified' FROM generate_series(1,14) n RETURNING id")
        .bind(event).bind(user.id).fetch_all(&pool).await?;
    let temp = tempfile::tempdir()?;
    let state = BotState::new(pool.clone(), Arc::new(config(temp.path().into())));
    let sent = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = telegram_mock(sent.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let bot = Bot::new("TEST").set_api_url(format!("http://{addr}").parse()?);
    let last = || {
        sent.lock()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["method"] == "SendMessage" || m["method"] == "EditMessageText")
            .unwrap()
            .clone()
    };
    let buttons = |m: &serde_json::Value| {
        m["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|r| r.as_array().unwrap().clone())
            .collect::<Vec<_>>()
    };
    let assert_page = |page: u32, pages: u32, count: usize| {
        let m = last();
        assert!(m["text"]
            .as_str()
            .unwrap()
            .contains(&format!("Страница {page} из {pages}")));
        let keys = buttons(&m);
        assert_eq!(
            keys.iter()
                .filter(|b| b["text"].as_str().unwrap().contains("Предыдущая"))
                .count(),
            usize::from(page > 1)
        );
        assert_eq!(
            keys.iter()
                .filter(|b| b["text"].as_str().unwrap().contains("Следующая"))
                .count(),
            usize::from(page < pages)
        );
        assert_eq!(
            keys.iter()
                .filter(|b| b["callback_data"].as_str().unwrap().starts_with("info:"))
                .count(),
            count
        );
    };
    handle(&bot, ChatId(7777), &state, &user, Command::Catalog).await?;
    assert_page(1, 2, 10);
    let first_panel = last()["result_message_id"].clone();
    for id in &ids[..2] {
        handle(&bot, ChatId(7777), &state, &user, Command::Info(*id)).await?;
        assert!(buttons(&last())
            .iter()
            .any(|b| b["callback_data"] == "closecard"));
    }
    let cards: Vec<_> = sent
        .lock()
        .unwrap()
        .iter()
        .filter(|m| {
            m["method"] == "SendMessage" && m["text"].as_str().unwrap_or("").starts_with('#')
        })
        .map(|m| m["result_message_id"].clone())
        .collect();
    let cards_at = sent.lock().unwrap().len();
    // Reply keyboard sections must be sent at the bottom before deleting the old control.
    let mut old_panel = first_panel;
    for (id, command, text) in [
        (900, Command::Catalog, "🔎 Поиск видео"),
        (901, Command::Filters, "🎛 Фильтры"),
    ] {
        let at = sent.lock().unwrap().len();
        let msg: Message = serde_json::from_value(
            serde_json::json!({"message_id":id,"date":0,"chat":{"id":7777,"type":"private"},"from":{"id":7777,"is_bot":false,"first_name":"Admin"},"text":text}),
        )?;
        message(bot.clone(), msg, command, state.clone()).await?;
        let calls = sent.lock().unwrap().clone();
        let new = last();
        assert_eq!(new["method"], "SendMessage");
        assert_ne!(new["result_message_id"], old_panel);
        let send = calls[at..]
            .iter()
            .position(|m| m["method"] == "SendMessage")
            .unwrap();
        let delete = calls[at..]
            .iter()
            .position(|m| m["method"] == "DeleteMessage" && m["message_id"] == old_panel)
            .unwrap();
        assert!(send < delete);
        assert!(calls[at..]
            .iter()
            .any(|m| m["method"] == "DeleteMessage" && m["message_id"] == id));
        old_panel = new["result_message_id"].clone();
    }
    let navigation_at = sent.lock().unwrap().len();
    handle_callback(&bot, ChatId(7777), &state, &user, "search").await?;
    handle_callback(&bot, ChatId(7777), &state, &user, "searchpage:1").await?;
    assert_page(2, 2, 4);
    // A double click targets the same page; old next callbacks and /next also clamp.
    for action in ["searchpage:1", "next", "next", "searchpage:4294967295"] {
        handle_callback(&bot, ChatId(7777), &state, &user, action).await?;
        assert_page(2, 2, 4);
        assert_eq!(state.sessions.lock().await[&user.id].filters.page, 1);
    }
    for action in ["prev", "prev"] {
        handle_callback(&bot, ChatId(7777), &state, &user, action).await?;
        assert_page(1, 2, 10);
    }
    sqlx::query("INSERT INTO videos(relative_path,size_bytes,mtime_ns,event_id,pilot_id,state) SELECT 'test/video' || n || '.mp4',100,1,$1,$2,'classified' FROM generate_series(15,21) n")
        .bind(event).bind(user.id).execute(&pool).await?;
    handle_callback(&bot, ChatId(7777), &state, &user, "searchpage:1").await?;
    assert_page(2, 3, 10);
    handle_callback(&bot, ChatId(7777), &state, &user, "searchpage:2").await?;
    assert_page(3, 3, 1);
    // User-entered page numbers must be clamped too.
    handle(
        &bot,
        ChatId(7777),
        &state,
        &user,
        Command::Search(r#"{"page":4294967295}"#.into()),
    )
    .await?;
    assert_page(3, 3, 1);
    // Removing results while on the final page adjusts the page and the buttons.
    sqlx::query("UPDATE videos SET state='missing' WHERE id > $1")
        .bind(ids[9])
        .execute(&pool)
        .await?;
    handle_callback(&bot, ChatId(7777), &state, &user, "search").await?;
    assert_page(1, 1, 10);
    assert_eq!(state.sessions.lock().await[&user.id].filters.page, 0);
    sqlx::query("UPDATE videos SET state='missing' WHERE id <> $1")
        .bind(ids[0])
        .execute(&pool)
        .await?;
    handle_callback(&bot, ChatId(7777), &state, &user, "next").await?;
    assert_page(1, 1, 1);
    sqlx::query("UPDATE videos SET state='missing'")
        .execute(&pool)
        .await?;
    for action in ["next", "prev", "searchpage:4294967295"] {
        handle_callback(&bot, ChatId(7777), &state, &user, action).await?;
        assert!(last()["text"]
            .as_str()
            .unwrap()
            .contains("Найдено видео: 0"));
        assert!(buttons(&last()).iter().all(|b| !b["callback_data"]
            .as_str()
            .unwrap()
            .starts_with("searchpage:")));
        assert_eq!(state.sessions.lock().await[&user.id].filters.page, 0);
    }
    let calls = sent.lock().unwrap().clone();
    assert!(calls[navigation_at..]
        .iter()
        .filter(|m| m["method"] == "EditMessageText")
        .all(|m| m["message_id"] == old_panel));
    assert!(calls[navigation_at..]
        .iter()
        .all(|m| m["method"] != "SendMessage"));
    assert!(calls[cards_at..]
        .iter()
        .all(|m| !cards.contains(&m["message_id"])));
    // Closing a card removes exactly that message, without changing other cards or selection.
    let selection = state.sessions.lock().await[&user.id].selection.clone();
    let q: CallbackQuery = serde_json::from_value(
        serde_json::json!({"id":"close-one-card","from":{"id":7777,"is_bot":false,"first_name":"Admin"},"chat_instance":"test","message":{"message_id":cards[0],"date":1,"chat":{"id":7777,"type":"private"},"from":{"id":123,"is_bot":true,"first_name":"Bot"},"text":"video card"},"data":"closecard"}),
    )?;
    let at = sent.lock().unwrap().len();
    callback(bot.clone(), q, state.clone()).await?;
    let calls = sent.lock().unwrap().clone();
    assert_eq!(
        calls[at..]
            .iter()
            .filter(|m| m["method"] == "DeleteMessage" && m["message_id"] == cards[0])
            .count(),
        1
    );
    assert!(calls[at..].iter().all(|m| m["message_id"] != cards[1]));
    assert!(calls[at..]
        .iter()
        .any(|m| m["method"] == "AnswerCallbackQuery"));
    assert_eq!(state.sessions.lock().await[&user.id].selection, selection);
    server.abort();
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin_pool)
        .await?;
    admin_pool.close().await;
    Ok(())
}
