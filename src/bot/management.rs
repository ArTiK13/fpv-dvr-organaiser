use super::*;

pub(super) async fn show_form(
    bot: &Bot,
    chat: ChatId,
    s: &BotState,
    form: &UserForm,
) -> Result<()> {
    let (text, keys) = form.view();
    show_panel(bot, chat, s, PanelKind::Form, text, keys).await
}

pub(super) async fn start_user_form(
    bot: &Bot,
    chat: ChatId,
    s: &BotState,
    u: &User,
    tg: Option<i64>,
) -> Result<()> {
    admin(u)?;
    let mut form = UserForm::new();
    if let Some(tg) = tg {
        if let Some(existing) = db::user(&s.pool, tg).await? {
            return edit_user_form(bot, chat, s, u, existing.id).await;
        }
        let request: Option<(Option<String>,Option<String>)> = sqlx::query_as("SELECT display_name,username FROM access_requests WHERE telegram_id=$1 AND resolved_at IS NULL")
            .bind(tg).fetch_optional(&s.pool).await?;
        form.input.telegram_id = tg;
        if let Some((name, username)) = request {
            form.input.full_name = name;
            form.input.telegram_username = username;
            form.step = Step::Review;
        } else {
            form.step = Step::FullName;
        }
    }
    s.sessions.lock().await.entry(u.id).or_default().user_form = Some(form.clone());
    let (text, mut keys) = form.view();
    if tg.is_none() {
        let requests: Vec<(i64,Option<String>,Option<String>)> = sqlx::query_as("SELECT telegram_id,display_name,username FROM access_requests WHERE resolved_at IS NULL ORDER BY requested_at DESC LIMIT 10")
            .fetch_all(&s.pool).await?;
        for (id, name, username) in requests {
            let label = name.or(username).unwrap_or_else(|| id.to_string());
            keys.inline_keyboard.insert(
                0,
                vec![B::callback(
                    format!("Добавить: {}", label.chars().take(40).collect::<String>()),
                    format!("adduser:{id}"),
                )],
            );
        }
    }
    show_panel(bot, chat, s, PanelKind::Form, text, keys).await
}

pub(super) async fn edit_user_form(
    bot: &Bot,
    chat: ChatId,
    s: &BotState,
    u: &User,
    id: i64,
) -> Result<()> {
    admin(u)?;
    let row = sqlx::query("SELECT u.*,ARRAY(SELECT alias FROM user_file_aliases WHERE user_id=u.id ORDER BY alias) AS aliases FROM users u WHERE id=$1")
        .bind(id).fetch_optional(&s.pool).await?.context("Пользователь не найден")?;
    let form = UserForm::prefilled(input_from_row(&row));
    s.sessions.lock().await.entry(u.id).or_default().user_form = Some(form.clone());
    show_form(bot, chat, s, &form).await
}

fn input_from_row(row: &sqlx::postgres::PgRow) -> db::UserInput {
    db::UserInput {
        id: Some(row.get("id")),
        telegram_id: row.get("telegram_id"),
        full_name: row.get("full_name"),
        nickname: row.get("nickname"),
        telegram_username: row.get("telegram_username"),
        is_admin: row.get("is_admin"),
        is_public: row.get("is_public"),
        aliases: row.get("aliases"),
    }
}

pub(super) async fn free_message(bot: Bot, msg: Message, s: BotState) -> ResponseResult<()> {
    if !msg.chat.is_private() {
        return Ok(());
    }
    let Some(from) = msg.from.as_ref() else {
        return Ok(());
    };
    let result: Result<()> = async {
        let u = db::user(&s.pool, from.id.0 as i64)
            .await?
            .filter(|u| u.active)
            .ok_or_else(|| ui::invalid("Доступ закрыт. /start — подать заявку."))?;
        let Some(text) = msg.text() else {
            return Err(ui::invalid(
                "На этом шаге отправьте текст или выберите кнопку под сообщением.",
            ));
        };
        let form = {
            let mut sessions = s.sessions.lock().await;
            let Some(current) = sessions.entry(u.id).or_default().user_form.as_mut() else {
                return Err(ui::invalid(
                    "Выберите действие на клавиатуре. /menu — показать главное меню.",
                ));
            };
            admin(&u)?;
            // Validate a clone so failed input leaves both the step and values intact.
            let mut next = current.clone();
            next.text(text)?;
            *current = next.clone();
            next
        };
        show_form(&bot, msg.chat.id, &s, &form).await?;
        panels::remove_input(&bot, msg.chat.id, msg.id, &s).await;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        tracing::warn!(error=%s.config.redact(&e.to_string()), "user form input failed");
        let form = if let Some(u) = db::user(&s.pool, from.id.0 as i64)
            .await
            .ok()
            .flatten()
            .filter(|u| u.active && u.is_admin)
        {
            s.sessions
                .lock()
                .await
                .get(&u.id)
                .and_then(|session| session.user_form.clone())
        } else {
            None
        };
        if let Some(form) = form {
            let (prompt, keys) = form.view();
            if show_panel(
                &bot,
                msg.chat.id,
                &s,
                PanelKind::Form,
                format!("{}\n\n{prompt}", ui::error_message(&e)),
                keys,
            )
            .await
            .is_ok()
            {
                panels::remove_input(&bot, msg.chat.id, msg.id, &s).await;
            }
        } else {
            bot.send_message(msg.chat.id, ui::error_message(&e)).await?;
        }
    }
    Ok(())
}

pub(super) async fn user_form_callback(
    bot: &Bot,
    chat: ChatId,
    s: &BotState,
    u: &User,
    data: &str,
) -> Result<()> {
    admin(u)?;
    let value = data.strip_prefix("uf:").context("form callback")?;
    let (token, action) = value.split_once(':').context("form action")?;
    let token = Uuid::parse_str(token)?;
    let form = {
        let mut sessions = s.sessions.lock().await;
        let entry = sessions.entry(u.id).or_default();
        let current = entry.user_form.as_mut().filter(|f| f.token == token)
            .ok_or_else(|| ui::invalid("Эта форма уже закрыта или изменена. Используйте последнее сообщение или начните добавление заново."))?;
        if action == "save" {
            current.validate()?;
            entry.user_form.take().unwrap()
        } else if action == "cancel" {
            entry.user_form = None;
            drop(sessions);
            finish_panel(
                bot,
                chat,
                s,
                PanelKind::Form,
                "Заполнение отменено. Изменения не сохранены.",
                K::new(Vec::<Vec<B>>::new()),
            )
            .await?;
            return Ok(());
        } else {
            current.action(action)?;
            current.clone()
        }
    };
    if action != "save" {
        return show_form(bot, chat, s, &form).await;
    }
    let id = match db::save_user(
        &s.pool,
        u.id,
        form.input.clone(),
        s.config.bootstrap_admin_chat_id,
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            // Preserve the draft for corrections, but never overwrite a newer form.
            let mut sessions = s.sessions.lock().await;
            let entry = sessions.entry(u.id).or_default();
            if entry.user_form.is_none() {
                entry.user_form = Some(form);
            }
            return Err(error);
        }
    };
    finish_panel(
        bot,
        chat,
        s,
        PanelKind::Form,
        format!(
            "✅ Пользователь {}.\n\n{}\n\nКаталог будет проверен заново с учётом этих данных.",
            if form.input.id.is_some() {
                "обновлён"
            } else {
                "добавлен"
            },
            form.summary()
        ),
        K::new(vec![vec![B::callback("Добавить ещё", "newuser")]]),
    )
    .await?;
    reclassify(s);
    tracing::info!(user_id = id, "user saved from interactive form");
    Ok(())
}

pub(super) fn reclassify(s: &BotState) {
    let p = s.pool.clone();
    let c = s.config.clone();
    tokio::spawn(async move {
        if let Err(e) = scanner::scan_once(&p, &c).await {
            tracing::warn!(error=%c.redact(&e.to_string()), "reclassification");
        }
    });
}

pub(super) async fn show_directory(
    bot: &Bot,
    chat: ChatId,
    s: &BotState,
    kind: &str,
) -> Result<()> {
    match kind.trim() {
        "users" => return show_users(bot, chat, s, 0).await,
        "devices" | "systems" => {
            let devices = kind.trim() == "devices";
            let rows = if devices {
                sqlx::query("SELECT d.*,ARRAY(SELECT alias FROM device_aliases WHERE device_id=d.id ORDER BY alias) AS aliases FROM devices d ORDER BY active DESC,id").fetch_all(&s.pool).await?
            } else {
                sqlx::query("SELECT s.*,ARRAY(SELECT alias FROM video_system_aliases WHERE video_system_id=s.id ORDER BY alias) AS aliases FROM video_systems s ORDER BY active DESC,id").fetch_all(&s.pool).await?
            };
            let title = if devices {
                "✈️ Аппараты"
            } else {
                "📹 Видеосистемы"
            };
            let mut cards = vec![];
            for row in rows {
                let description = if devices {
                    format!("Тип: {}", ui::device_type(&row.get::<String, _>("type")))
                } else {
                    format!(
                        "OSD (данные полёта на видео): {}",
                        if row.get::<bool, _>("with_osd") {
                            "есть"
                        } else {
                            "нет"
                        }
                    )
                };
                cards.push(format!(
                    "{}{}\n{}\nИмена в папках: {}",
                    row.get::<String, _>("name"),
                    if row.get::<bool, _>("active") {
                        ""
                    } else {
                        " (удалён)"
                    },
                    description,
                    ui::names(&row.get::<Vec<String>, _>("aliases"))
                ));
            }
            let text = if cards.is_empty() {
                format!("{title}\n\nПока ничего не добавлено.")
            } else {
                format!("{title}\n\n{}", cards.join("\n\n"))
            };
            send_chunks(bot, chat, &text).await?;
        }
        _ => {
            return Err(ui::invalid(
                "Выберите «Пользователи», «Аппараты» или «Видеосистемы» в меню управления.",
            ))
        }
    }
    Ok(())
}

pub(super) async fn show_users(bot: &Bot, chat: ChatId, s: &BotState, page: u32) -> Result<()> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&s.pool)
        .await?;
    let rows = sqlx::query("SELECT u.*,ARRAY(SELECT alias FROM user_file_aliases WHERE user_id=u.id ORDER BY alias) AS aliases FROM users u ORDER BY active DESC,id LIMIT 8 OFFSET $1").bind(i64::from(page)*8)
                .fetch_all(&s.pool).await?;
    bot.send_message(
        chat,
        format!(
            "👤 Пользователи: {count}\nСтраница {}\nДля изменения данных нажмите кнопку под карточкой.",
            page + 1
        ),
    )
    .await?;
    for row in rows {
        let input = input_from_row(&row);
        let id = input.id.unwrap();
        let form = UserForm::prefilled(input);
        let active: bool = row.get("active");
        let enabled: bool = row.get("notifications_enabled");
        let time: NaiveTime = row.get("notification_time");
        bot.send_message(
            chat,
            format!(
                "👤 {}\n\n{}\n\nДоступ к боту: {}\nДайджест: {}",
                form.input
                    .nickname
                    .as_deref()
                    .or(form.input.full_name.as_deref())
                    .or(form.input.telegram_username.as_deref())
                    .unwrap_or("Пилот"),
                form.summary(),
                if active {
                    "разрешён"
                } else {
                    "отключён"
                },
                if enabled {
                    format!("ежедневно в {} по Москве", time.format("%H:%M"))
                } else {
                    "выключен".into()
                }
            ),
        )
        .reply_markup(K::new(vec![vec![B::callback(
            if active {
                "Изменить пользователя"
            } else {
                "Восстановить / изменить"
            },
            format!("edituser:{id}"),
        )]]))
        .await?;
    }

    let mut navigation = vec![];
    if page > 0 {
        navigation.push(B::callback("← Назад", format!("userspage:{}", page - 1)));
    }
    if i64::from(page + 1) * 8 < count {
        navigation.push(B::callback("Далее →", format!("userspage:{}", page + 1)));
    }
    let mut rows = vec![vec![B::callback("➕ Добавить пользователя", "newuser")]];
    if !navigation.is_empty() {
        rows.insert(0, navigation);
    }
    bot.send_message(chat, "Управление пользователями")
        .reply_markup(K::new(rows))
        .await?;
    Ok(())
}
