mod management;
mod panels;
mod user_form;
use crate::{
    config::Config,
    db::{self, Filters, User},
    downloads::{self, DownloadState},
    presentation as ui, scanner,
};
use anyhow::{bail, Context, Result};
use chrono::{NaiveDate, NaiveTime};
use management::*;
use panels::{finish_panel, show_panel, PanelKind};
use serde::Deserialize;
use sqlx::{PgPool, Row};
use std::{collections::HashMap, sync::Arc};
use teloxide::{
    prelude::*,
    types::{
        CallbackQuery, InlineKeyboardButton as B, InlineKeyboardMarkup as K, KeyboardButton,
        KeyboardMarkup,
    },
    utils::command::BotCommands,
};
use tokio::sync::Mutex;
use user_form::{Step, UserForm};
use uuid::Uuid;
#[derive(Default, Clone)]
struct Session {
    filters: Filters,
    selection: Vec<i64>,
    mode: String,
    nonce: Option<Uuid>,
    user_form: Option<UserForm>,
}
#[derive(Clone)]
pub struct BotState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    sessions: Arc<Mutex<HashMap<i64, Session>>>,
    panels: panels::Panels,
}
impl BotState {
    pub fn new(pool: PgPool, config: Arc<Config>) -> Self {
        Self {
            pool,
            config,
            sessions: Arc::default(),
            panels: Default::default(),
        }
    }
}
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "FPV-каталог:")]
enum Command {
    #[command(description = "главное меню")]
    Start,
    #[command(description = "показать кнопки главного меню")]
    Menu,
    #[command(description = "видео по текущим фильтрам")]
    Catalog,
    #[command(description = "выбрать фильтры кнопками")]
    Filters,
    #[command(description = "выбрать подписки кнопками")]
    Subscriptions,
    #[command(description = "настройки уведомлений")]
    Notifications,
    #[command(description = "меню администратора")]
    Admin,
    #[command(description = "команды и примеры")]
    Help,
    #[command(description = "поиск; текст или JSON фильтров")]
    Search(String),
    #[command(description = "следующая страница")]
    Next,
    #[command(description = "информация о видео: ID")]
    Info(i64),
    #[command(description = "Immich-ссылка: ID")]
    Link(i64),
    #[command(description = "отправить видео: ID")]
    Video(i64),
    #[command(description = "отправить документ: ID")]
    Document(i64),
    #[command(description = "выдать выборку: video/document/immich")]
    All(String),
    #[command(description = "подтвердить текущую выборку")]
    Confirm,
    #[command(description = "прогресс выдачи")]
    Jobs,
    #[command(description = "остановить выдачу: UUID")]
    Cancel(String),
    #[command(description = "подписки: ID пилотов через пробел или none")]
    Subscribe(String),
    #[command(description = "время дайджеста HH:MM, либо on/off")]
    Notify(String),
    #[command(description = "состояние системы, только админ")]
    Status,
    #[command(description = "запустить сканирование, только админ")]
    Scan,
    #[command(description = "сохранить пользователя: JSON, только админ")]
    User(String),
    #[command(description = "сохранить аппарат: JSON, только админ")]
    Device(String),
    #[command(description = "сохранить видеосистему: JSON, только админ")]
    System(String),
    #[command(description = "справочники: users/devices/systems, админ")]
    List(String),
    #[command(description = "удалить: user/device/system ID, админ")]
    Delete(String),
    #[command(description = "заявки, только админ")]
    Requests,
    #[command(description = "добавить пользователя пошагово")]
    Adduser,
    #[command(description = "отклонить заявку: Telegram ID, админ")]
    Reject(i64),
    #[command(description = "пилот по умолчанию: внутренний ID или none, админ")]
    Defaultuser(String),
    #[command(description = "дата события: event_ID from to, админ")]
    Date(String),
    #[command(description = "системные уведомления: 0/15/60/1440 минут, админ")]
    Alertinterval(i32),
    #[command(description = "резервная ссылка: ID видео, только админ")]
    Fallback(i64),
}
pub async fn run(state: BotState) {
    let bot = Bot::new(state.config.telegram_bot_token.clone());
    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(message),
        )
        .branch(
            Update::filter_message()
                .filter_map(|msg: Message| reply_command(msg.text()?))
                .endpoint(message),
        )
        .branch(Update::filter_message().endpoint(free_message))
        .branch(Update::filter_callback_query().endpoint(callback));
    let error_config = state.config.clone();
    let errors = Arc::new(move |e: teloxide::RequestError| {
        let text = error_config.redact(&e.to_string());
        async move {
            tracing::warn!(error=%text, "Telegram request failed");
        }
    });
    let listener = teloxide::update_listeners::polling_default(bot.clone()).await;
    Dispatcher::builder(bot, handler)
        .error_handler(errors.clone())
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch_with_listener(listener, errors)
        .await;
}
async fn message(bot: Bot, msg: Message, cmd: Command, s: BotState) -> ResponseResult<()> {
    if !msg.chat.is_private() {
        return Ok(());
    }
    let Some(from) = msg.from.as_ref() else {
        return Ok(());
    };
    let tg = from.id.0 as i64;
    let result: Result<()> = async {
        let Some(u) = db::user(&s.pool, tg).await?.filter(|u| u.active) else {
            if matches!(cmd, Command::Start) {
                sqlx::query("INSERT INTO access_requests(telegram_id,username,display_name) VALUES($1,$2,$3) ON CONFLICT(telegram_id) DO UPDATE SET username=EXCLUDED.username,display_name=EXCLUDED.display_name,resolved_at=NULL,requested_at=now()")
                    .bind(tg).bind(&from.username).bind(&from.first_name).execute(&s.pool).await?;
                bot.send_message(msg.chat.id, "Заявка сохранена. Администратор должен выдать доступ. После добавления нажмите /start.").await?;
            } else {
                bot.send_message(msg.chat.id, "Доступ закрыт. /start — подать заявку.").await?;
            }
            return Ok(());
        };
        let cleanup = matches!(cmd,
            Command::Start | Command::Menu | Command::Admin | Command::Catalog
            | Command::Search(_) | Command::Filters | Command::Subscriptions
            | Command::Notifications | Command::Notify(_) | Command::Subscribe(_)
            | Command::Adduser | Command::User(_) | Command::Next);
        handle(&bot, msg.chat.id, &s, &u, cmd).await?;
        if cleanup { panels::remove_input(&bot, msg.chat.id, msg.id, &s).await; }
        Ok(())
    }.await;
    if let Err(e) = result {
        tracing::warn!(error=%s.config.redact(&e.to_string()),"bot command failed");
        bot.send_message(msg.chat.id, format!("Ошибка: {}", ui::error_message(&e)))
            .await?;
    }
    Ok(())
}
fn admin(u: &User) -> Result<()> {
    if !u.is_admin {
        bail!("Требуются права администратора");
    }
    Ok(())
}
fn reply_command(text: &str) -> Option<Command> {
    Some(match text.trim() {
        "🔎 Поиск видео" => Command::Catalog,
        "🎛 Фильтры" => Command::Filters,
        "👥 Подписки" => Command::Subscriptions,
        "🔔 Уведомления" => Command::Notifications,
        "📤 Мои запросы" => Command::Jobs,
        "❓ Помощь" => Command::Help,
        "⚙️ Управление" => Command::Admin,
        "📊 Состояние" => Command::Status,
        "🔄 Сканировать" => Command::Scan,
        "✈️ Аппараты" => Command::List("devices".into()),
        "📹 Видеосистемы" => Command::List("systems".into()),
        "👤 Пользователи" => Command::List("users".into()),
        "📩 Заявки" | "➕ Добавить пользователя" => Command::Adduser,
        "🏠 Главное меню" => Command::Menu,
        _ => return None,
    })
}
fn menu_keyboard(is_admin: bool, management: bool) -> KeyboardMarkup {
    let mut rows = if management && is_admin {
        vec![
            vec!["📊 Состояние", "🔄 Сканировать"],
            vec!["✈️ Аппараты", "📹 Видеосистемы"],
            vec!["👤 Пользователи", "➕ Добавить пользователя"],
            vec!["🏠 Главное меню"],
        ]
    } else {
        vec![
            vec!["🔎 Поиск видео", "🎛 Фильтры"],
            vec!["👥 Подписки", "🔔 Уведомления"],
            vec!["📤 Мои запросы", "❓ Помощь"],
        ]
    };
    if is_admin && !management {
        rows.push(vec!["⚙️ Управление"]);
    }
    KeyboardMarkup::new(
        rows.into_iter()
            .map(|row| row.into_iter().map(KeyboardButton::new)),
    )
    .resize_keyboard()
    .persistent()
    .input_field_placeholder("Выберите действие")
}
async fn handle(bot: &Bot, chat: ChatId, s: &BotState, u: &User, cmd: Command) -> Result<()> {
    let pool = &s.pool;
    if !matches!(
        cmd,
        Command::Adduser | Command::User(_) | Command::Cancel(_)
    ) {
        panels::leave_form(bot, chat, s, u).await;
    }
    match cmd {
        Command::Start | Command::Menu => {
            panels::reply_menu(bot,chat,s,"FPV-каталог. Действия доступны на клавиатуре внизу чата. /menu — вернуть главное меню.",menu_keyboard(u.is_admin,false)).await?;
        }
        Command::Catalog => show_search(bot, chat, s, u).await?,
        Command::Filters => show_filters(bot, chat, s, u).await?,
        Command::Subscriptions => show_subscriptions(bot, chat, s, u).await?,
        Command::Notifications => show_notifications(bot, chat, s, u).await?,
        Command::Adduser => {
            admin(u)?;
            start_user_form(bot, chat, s, u, None).await?;
        }
        Command::Admin => {
            admin(u)?;
            panels::reply_menu(bot,chat,s,"Управление каталогом. Выберите раздел на клавиатуре. Пользователей можно добавлять и редактировать пошагово.",menu_keyboard(true,true)).await?;
        }
        Command::Help => {
            bot.send_message(chat, "Действия доступны на клавиатуре внизу чата.

🔎 Поиск видео — список по текущим фильтрам.
🎛 Фильтры — аппарат, видеосистема, пилот, OSD и цветокоррекция.
👥 Подписки — о чьих видео получать новости.
🔔 Уведомления — включение и время ежедневного дайджеста.
📤 Мои запросы — прогресс отправки видео.

В карточке видео выберите отправку видео, файла или ссылки Immich. Большие файлы доступны по ссылке.

Администратор добавляет пилотов через «Управление» → «Добавить пользователя», а редактирует через «Пользователи».
/menu — вернуть кнопки. /cancel — отменить заполнение формы.").await?;
        }
        Command::Search(value) => {
            let f = if value.trim().starts_with('{') {
                serde_json::from_str(&value)?
            } else {
                Filters {
                    text: (!value.trim().is_empty()).then_some(value),
                    ..Default::default()
                }
            };
            s.sessions.lock().await.entry(u.id).or_default().filters = f;
            show_search(bot, chat, s, u).await?;
        }
        Command::Next => {
            let mut sessions = s.sessions.lock().await;
            let page = &mut sessions.entry(u.id).or_default().filters.page;
            *page = page.saturating_add(1);
            drop(sessions);
            show_search(bot, chat, s, u).await?;
        }
        Command::Info(id) => show_info(bot, chat, s, u, id).await?,
        Command::Link(id) | Command::Video(id) | Command::Document(id) => {
            let mode = match cmd {
                Command::Link(_) => "immich",
                Command::Document(_) => "document",
                _ => "video",
            };
            prepare(bot, chat, s, u, vec![id], mode).await?;
        }
        Command::All(mode) => {
            let mode = if mode.trim().is_empty() {
                "video"
            } else {
                mode.trim()
            };
            if !["video", "document", "immich"].contains(&mode) {
                bail!("Выберите video/document/immich");
            }
            let f = s
                .sessions
                .lock()
                .await
                .entry(u.id)
                .or_default()
                .filters
                .clone();
            let ids = db::selection(pool, u, &f).await?;
            prepare(bot, chat, s, u, ids, mode).await?;
        }
        Command::Confirm => {
            if !s.config.delivery_enabled {
                bail!("Выдача выключена в конфигурации");
            }
            let session = {
                let mut sessions = s.sessions.lock().await;
                take_selection(sessions.entry(u.id).or_default(), None)?
            };
            for id in &session.selection {
                if db::video(pool, u, *id).await?.is_none() {
                    bail!("Доступ к выборке изменился");
                }
            }
            let job = db::enqueue(pool, u, chat.0, &session.selection, &session.mode).await?;
            finish_panel(
                bot,
                chat,
                s,
                PanelKind::Selection,
                "✅ Запрос принят. Прогресс доступен через «Мои запросы».",
                job_buttons(job),
            )
            .await?;
        }
        Command::Jobs => {
            let jobs = sqlx::query("SELECT id,state,mode,total_count,completed_count,created_at FROM delivery_jobs WHERE user_id=$1 ORDER BY created_at DESC LIMIT 10").bind(u.id).fetch_all(pool).await?;
            if jobs.is_empty() {
                bot.send_message(
                    chat,
                    "📤 Запросов на выдачу пока нет. Найдите видео и выберите способ отправки.",
                )
                .await?;
            }
            for job in jobs {
                let id: Uuid = job.get("id");
                let state: String = job.get("state");
                let created: chrono::DateTime<chrono::Utc> = job.get("created_at");
                let text = format!(
                    "📤 {}\nСпособ: {}\nСостояние: {}\nГотово: {} из {} видео",
                    created
                        .with_timezone(&chrono_tz::Europe::Moscow)
                        .format("%d.%m.%Y %H:%M"),
                    ui::delivery_mode(&job.get::<String, _>("mode")),
                    ui::state(&state),
                    job.get::<i32, _>("completed_count"),
                    job.get::<i32, _>("total_count")
                );
                let keys = if ["pending", "running", "waiting_for_immich"].contains(&state.as_str())
                {
                    vec![vec![B::callback(
                        "Остановить выдачу",
                        format!("canceljob:{id}"),
                    )]]
                } else {
                    vec![]
                };
                bot.send_message(chat, text)
                    .reply_markup(K::new(keys))
                    .await?;
            }
        }
        Command::Cancel(id) => {
            if id.trim().is_empty() {
                s.sessions.lock().await.entry(u.id).or_default().user_form = None;
                finish_panel(
                    bot,
                    chat,
                    s,
                    PanelKind::Form,
                    "Заполнение отменено.",
                    K::new(Vec::<Vec<B>>::new()),
                )
                .await?;
                return Ok(());
            }
            let id = Uuid::parse_str(id.trim())?;
            sqlx::query("UPDATE delivery_jobs SET state='cancelled' WHERE id=$1 AND user_id=$2 AND state NOT IN ('done','failed')").bind(id).bind(u.id).execute(pool).await?;
            sqlx::query("UPDATE immich_shared_links SET revoke_requested=true WHERE job_id=$1 AND user_id=$2 AND NOT sent").bind(id).bind(u.id).execute(pool).await?;
            bot.send_message(
                chat,
                "Остановка запрошена. Уже доставленные файлы сохраняются.",
            )
            .await?;
        }
        Command::Notify(v) => {
            match v.trim() {
                "on" | "off" => {
                    sqlx::query("UPDATE users SET notifications_enabled=$2 WHERE id=$1")
                        .bind(u.id)
                        .bind(v.trim() == "on")
                        .execute(pool)
                        .await?;
                }
                t => {
                    sqlx::query("UPDATE users SET notification_time=$2 WHERE id=$1")
                        .bind(u.id)
                        .bind(db::parse_time(t)?)
                        .execute(pool)
                        .await?;
                }
            }
            show_notifications(bot, chat, s, u).await?;
        }
        Command::Subscribe(v) => {
            let mut ids = vec![];
            if v.trim() != "none" {
                for id in v.split_whitespace() {
                    let id: i64 = id.parse()?;
                    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1 AND active AND (id=$2 OR is_public OR $3))").bind(id).bind(u.id).bind(u.is_admin).fetch_one(pool).await?;
                    if !valid {
                        bail!("Недоступный пилот: {id}");
                    }
                    ids.push(id);
                }
            }
            let mut tx = pool.begin().await?;
            sqlx::query("DELETE FROM notification_subscriptions WHERE user_id=$1")
                .bind(u.id)
                .execute(&mut *tx)
                .await?;
            for id in ids {
                sqlx::query(
                    "INSERT INTO notification_subscriptions(user_id,pilot_id) VALUES($1,$2) ON CONFLICT DO NOTHING",
                )
                .bind(u.id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            show_subscriptions(bot, chat, s, u).await?;
        }
        Command::Status => {
            admin(u)?;
            let states: Vec<(String, i64)> =
                sqlx::query_as("SELECT state,count(*) FROM videos GROUP BY state ORDER BY state")
                    .fetch_all(pool)
                    .await?;
            let issues: Vec<(i64,String,String)> = sqlx::query_as("SELECT v.id,v.relative_path,i.issue_code FROM classification_issues i JOIN videos v ON v.id=i.video_id WHERE i.resolved_at IS NULL ORDER BY v.id LIMIT 15").fetch_all(pool).await?;
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM classification_issues WHERE resolved_at IS NULL",
            )
            .fetch_one(pool)
            .await?;
            let media_count: i64 =
                sqlx::query_scalar("SELECT count(*) FROM media_errors WHERE resolved_at IS NULL")
                    .fetch_one(pool)
                    .await?;
            let media: Vec<(String,String)> = sqlx::query_as("SELECT relative_path,error FROM media_errors WHERE resolved_at IS NULL ORDER BY first_seen_at LIMIT 5").fetch_all(pool).await?;
            let alerts: Vec<(String, String)> = sqlx::query_as(
                "SELECT key,message FROM system_alerts WHERE active ORDER BY created_at LIMIT 10",
            )
            .fetch_all(pool)
            .await?;
            let jobs: Vec<(String, i64)> = sqlx::query_as(
                "SELECT state,count(*) FROM delivery_jobs GROUP BY state ORDER BY state",
            )
            .fetch_all(pool)
            .await?;
            let text = format!("📊 Состояние каталога\n\nВидео: {}\nВыдача: {}\nПроблемы классификации: {count}\n{}\nОшибки файлов: {media_count}\n{}\nСистемные проблемы:\n{}",
                states.iter().map(|(s,n)|format!("{}: {n}",ui::state(s))).collect::<Vec<_>>().join(", "),
                jobs.iter().map(|(s,n)|format!("{}: {n}",ui::state(s))).collect::<Vec<_>>().join(", "),
                issues.iter().map(|(id,p,e)|format!("Видео №{id}: {}\n{p}",ui::issue(e))).collect::<Vec<_>>().join("\n"),
                media.iter().map(|(p,_e)|format!("Не удалось прочитать параметры: {p}")).collect::<Vec<_>>().join("\n"),
                alerts.iter().map(|(k,m)|ui::alert(k,m)).collect::<Vec<_>>().join("\n"));
            send_chunks(bot, chat, &s.config.redact(&text)).await?;
        }
        Command::Scan => {
            admin(u)?;
            let p = pool.clone();
            let c = s.config.clone();
            let b = bot.clone();
            bot.send_message(chat, "Сканирование поставлено на выполнение.")
                .await?;
            tokio::spawn(async move {
                let text = match scanner::scan_once(&p, &c).await {
                    Ok(n) => format!("Сканирование: {n} видео"),
                    Err(e) => {
                        tracing::warn!(error=%c.redact(&e.to_string()), "manual scan failed");
                        "Не удалось обновить каталог. Проверьте доступ к папке видео или попробуйте позже.".into()
                    }
                };
                let _ = b.send_message(chat, text).await;
            });
        }
        Command::User(value) => {
            admin(u)?;
            if !value.trim().starts_with('{') {
                let tg = if value.trim().is_empty() {
                    None
                } else {
                    Some(user_form::parse_id(&value)?)
                };
                start_user_form(bot, chat, s, u, tg).await?;
            } else {
                let id = db::save_user(
                    pool,
                    u.id,
                    serde_json::from_str(&value)?,
                    s.config.bootstrap_admin_chat_id,
                )
                .await?;
                saved(bot, chat, s, id).await?;
            }
        }
        Command::Device(json) => {
            admin(u)?;
            let d: DeviceInput = serde_json::from_str(&json)?;
            let id = save_device(pool, d).await?;
            audit(pool, u.id, "save_device", id).await?;
            saved(bot, chat, s, id).await?;
        }
        Command::System(json) => {
            admin(u)?;
            let v: SystemInput = serde_json::from_str(&json)?;
            let id = save_system(pool, v).await?;
            audit(pool, u.id, "save_system", id).await?;
            saved(bot, chat, s, id).await?;
        }
        Command::List(kind) => {
            admin(u)?;
            show_directory(bot, chat, s, &kind).await?;
        }
        Command::Delete(v) => {
            admin(u)?;
            let (kind, id) = v.split_once(' ').context("user/device/system ID")?;
            let id: i64 = id.trim().parse()?;
            let query = match kind {
                "user" => {
                    let tg: Option<i64> =
                        sqlx::query_scalar("SELECT telegram_id FROM users WHERE id=$1")
                            .bind(id)
                            .fetch_optional(pool)
                            .await?;
                    if tg == Some(s.config.bootstrap_admin_chat_id) {
                        bail!("Нельзя удалить главного администратора");
                    }
                    "UPDATE users SET active=false WHERE id=$1"
                }
                "device" => "UPDATE devices SET active=false WHERE id=$1",
                "system" => "UPDATE video_systems SET active=false WHERE id=$1",
                _ => bail!("user/device/system"),
            };
            sqlx::query(query).bind(id).execute(pool).await?;
            db::invalidate_links(pool).await?;
            audit(pool, u.id, "delete", id).await?;
            saved(bot, chat, s, id).await?;
        }
        Command::Requests => {
            admin(u)?;
            start_user_form(bot, chat, s, u, None).await?;
        }
        Command::Reject(id) => {
            admin(u)?;
            sqlx::query("UPDATE access_requests SET resolved_at=now() WHERE telegram_id=$1")
                .bind(id)
                .execute(pool)
                .await?;
            bot.send_message(chat, "Заявка отклонена.").await?;
        }
        Command::Defaultuser(v) => {
            admin(u)?;
            let value = if v.trim() == "none" {
                serde_json::Value::Null
            } else {
                let id: i64 = v.trim().parse()?;
                let exists: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1 AND active)")
                        .bind(id)
                        .fetch_one(pool)
                        .await?;
                if !exists {
                    bail!("Неизвестный пилот");
                }
                serde_json::json!(id)
            };
            sqlx::query("INSERT INTO settings VALUES('default_user_id',$1) ON CONFLICT(key) DO UPDATE SET value=EXCLUDED.value").bind(value).execute(pool).await?;
            saved(bot, chat, s, 0).await?;
        }
        Command::Date(v) => {
            admin(u)?;
            let a = v.split_whitespace().collect::<Vec<_>>();
            if a.len() != 3 {
                bail!("/date event_ID YYYY-MM-DD YYYY-MM-DD");
            }
            let id: i64 = a[0].parse()?;
            let from: NaiveDate = a[1].parse()?;
            let to: NaiveDate = a[2].parse()?;
            if from > to {
                bail!("Неверный диапазон");
            }
            sqlx::query("UPDATE events SET date_start=$2,date_end=$3,date_precision='manual',manual_date=true WHERE id=$1").bind(id).bind(from).bind(to).execute(pool).await?;
            audit(pool, u.id, "event_date", id).await?;
            bot.send_message(chat, "Дата сохранена.").await?;
        }
        Command::Alertinterval(minutes) => {
            admin(u)?;
            if ![0, 15, 60, 1440].contains(&minutes) {
                bail!("0/15/60/1440");
            }
            sqlx::query("UPDATE users SET system_alert_interval_seconds=$2 WHERE id=$1")
                .bind(u.id)
                .bind(minutes * 60)
                .execute(pool)
                .await?;
            bot.send_message(chat, "Интервал сохранён.").await?;
        }
        Command::Fallback(id) => {
            admin(u)?;
            let url = downloads::create_token(
                &DownloadState {
                    pool: pool.clone(),
                    config: s.config.clone(),
                },
                u,
                id,
            )
            .await?;
            bot.send_message(chat, format!("Резервная ссылка, 1 час:\n{url}"))
                .await?;
        }
    }
    Ok(())
}
async fn prepare(
    bot: &Bot,
    chat: ChatId,
    s: &BotState,
    u: &User,
    ids: Vec<i64>,
    mode: &str,
) -> Result<()> {
    if !s.config.delivery_enabled {
        bail!("Выдача выключена в конфигурации");
    }
    let mut bytes = 0_i64;
    for id in &ids {
        bytes += db::video(&s.pool, u, *id)
            .await?
            .context("Видео недоступно")?
            .size_bytes;
    }
    if ids.is_empty() {
        bail!("Нет выбранных видео");
    }
    let count = ids.len();
    let mut sessions = s.sessions.lock().await;
    let session = sessions.entry(u.id).or_default();
    session.selection = ids;
    session.mode = mode.into();
    let nonce = Uuid::new_v4();
    session.nonce = Some(nonce);
    drop(sessions);
    show_panel(bot,chat,s,PanelKind::Selection,format!("Выбрано видео: {count}\nОбщий размер: {}\nСпособ: {}\n\nБольшие файлы будут доступны по ссылке Immich. Подтвердите отправку.",ui::size(bytes),ui::delivery_mode(mode)),K::new(vec![vec![B::callback("Подтвердить",format!("confirm:{nonce}")),B::callback("Отменить",format!("discard:{nonce}"))]])).await?;
    Ok(())
}
async fn show_search(bot: &Bot, chat: ChatId, s: &BotState, u: &User) -> Result<()> {
    panels::leave_form(bot, chat, s, u).await;
    let f = s
        .sessions
        .lock()
        .await
        .entry(u.id)
        .or_default()
        .filters
        .clone();
    let (n, bytes, rows) = db::search(&s.pool, u, &f).await?;
    let mut keyboard = vec![];
    for v in &rows {
        keyboard.push(vec![B::callback(
            format!(
                "#{} {}",
                v.id,
                v.relative_path.chars().take(50).collect::<String>()
            ),
            format!("info:{}", v.id),
        )]);
    }
    keyboard.push(vec![
        B::callback("Следующая", "next"),
        B::callback("Фильтры", "filters"),
    ]);
    keyboard.push(vec![
        B::callback("Все как видео", "all:video"),
        B::callback("Все как файлы", "all:document"),
        B::callback("Immich", "all:immich"),
    ]);
    show_panel(
        bot,
        chat,
        s,
        PanelKind::Search,
        format!(
            "🔎 Найдено видео: {n}\nОбщий размер: {}\nСтраница {}\n\n{}",
            ui::size(bytes),
            f.page + 1,
            filter_summary(&s.pool, u, &f).await?
        ),
        K::new(keyboard),
    )
    .await?;
    Ok(())
}
async fn show_info(bot: &Bot, chat: ChatId, s: &BotState, u: &User, id: i64) -> Result<()> {
    let v = db::video(&s.pool, u, id)
        .await?
        .context("Видео не найдено или недоступно")?;
    let mut keys = vec![vec![B::callback("Immich", format!("link:{id}"))]];
    if v.size_bytes as u64 <= s.config.telegram_limit {
        keys.insert(
            0,
            vec![
                B::callback("Видео", format!("video:{id}")),
                B::callback("Документ", format!("document:{id}")),
            ],
        );
    }
    if u.is_admin {
        keys.push(vec![B::callback(
            "Резервная ссылка",
            format!("fallback:{id}"),
        )]);
    }
    let duration = v
        .duration_ms
        .map(|d| format!("{:.1} с", d as f64 / 1000.0))
        .unwrap_or("нет данных".into());
    let resolution = match (v.width, v.height) {
        (Some(w), Some(h)) => format!("{w}×{h}"),
        _ => "нет данных".into(),
    };
    let date = match (v.date_start, v.date_end) {
        (Some(a), Some(b)) if a == b => a.to_string(),
        (Some(a), Some(b)) => format!("{a} — {b}"),
        _ => "не указана".into(),
    };
    let fps = v
        .fps
        .as_ref()
        .and_then(|f| {
            let (a, b) = f.split_once('/')?;
            let denominator = b.parse::<f64>().ok()?;
            if denominator == 0.0 {
                return None;
            }
            Some(format!("{:.2}", a.parse::<f64>().ok()? / denominator))
        })
        .unwrap_or("нет данных".into());
    let text=format!("#{}\n{}\nСобытие #{}: {} ({})\nПилот: {}\nАппарат: {}\nСистема: {}, OSD: {}\nПокрашено: {}\nДлительность: {}\nРазрешение: {}, FPS: {}\nРазмер: {:.1} МБ",v.id,v.relative_path,v.event_id,v.event_name,date,v.pilot_name.as_deref().unwrap_or("не определён"),v.device_name.as_deref().unwrap_or("не определён"),v.system_name.as_deref().unwrap_or("не определена"),match v.with_osd{Some(true)=>"есть",Some(false)=>"нет",None=>"не определено"},if v.colored{"да"}else{"нет"},duration,resolution,fps,v.size_bytes as f64/1e6);
    bot.send_message(chat, text)
        .reply_markup(K::new(keys))
        .await?;
    Ok(())
}
async fn callback(bot: Bot, q: CallbackQuery, s: BotState) -> ResponseResult<()> {
    let Some(msg) = q.message.as_ref() else {
        return Ok(());
    };
    let chat = msg.chat().id;
    if chat.0 != q.from.id.0 as i64 {
        return Ok(());
    }
    let result = async {
        let u = db::user(&s.pool, q.from.id.0 as i64)
            .await?
            .filter(|u| u.active)
            .context("Доступ закрыт")?;
        handle_callback(&bot, chat, &s, &u, q.data.as_deref().unwrap_or_default()).await
    }
    .await;
    if let Err(e) = result {
        tracing::warn!(error=%s.config.redact(&e.to_string()), "bot action failed");
        bot.answer_callback_query(&q.id)
            .text(ui::error_message(&e).chars().take(190).collect::<String>())
            .show_alert(true)
            .await?;
    } else {
        bot.answer_callback_query(&q.id).await?;
    }
    Ok(())
}

async fn handle_callback(
    bot: &Bot,
    chat: ChatId,
    s: &BotState,
    u: &User,
    data: &str,
) -> Result<()> {
    if data == "newuser" {
        admin(u)?;
        return start_user_form(bot, chat, s, u, None).await;
    }
    if let Some(page) = data.strip_prefix("userspage:") {
        admin(u)?;
        return show_users(bot, chat, s, page.parse::<u32>()?.min(1_000_000)).await;
    }
    if data.starts_with("uf:") {
        return user_form_callback(bot, chat, s, u, data).await;
    }
    if let Some(value) = data.strip_prefix("adduser:") {
        admin(u)?;
        return start_user_form(bot, chat, s, u, Some(user_form::parse_id(value)?)).await;
    }
    if let Some(value) = data.strip_prefix("edituser:") {
        admin(u)?;
        return edit_user_form(bot, chat, s, u, value.parse()?).await;
    }
    if let Some(value) = data.strip_prefix("canceljob:") {
        return handle(bot, chat, s, u, Command::Cancel(value.into())).await;
    }
    if let Some(value) = data.strip_prefix("notify:") {
        handle(bot, chat, s, u, Command::Notify(value.into())).await?;
        return Ok(());
    }
    if let Some(value) = data.strip_prefix("subscription:") {
        let id: i64 = value.parse()?;
        db::toggle_subscription(&s.pool, u.id, id).await?;
        return show_subscriptions(bot, chat, s, u).await;
    }
    match data {
        "search" => return show_search(bot, chat, s, u).await,
        "next" => return handle(bot, chat, s, u, Command::Next).await,
        "jobs" => return handle(bot, chat, s, u, Command::Jobs).await,
        "filters" => return show_filters(bot, chat, s, u).await,
        "reset" => {
            s.sessions.lock().await.entry(u.id).or_default().filters = Filters::default();
            return show_search(bot, chat, s, u).await;
        }
        "subscriptions" => return show_subscriptions(bot, chat, s, u).await,
        _ => {}
    }
    if let Some(nonce) = data.strip_prefix("confirm:") {
        if !s.config.delivery_enabled {
            bail!("Выдача выключена в конфигурации");
        }
        let expected = Uuid::parse_str(nonce)?;
        // Consume under one mutex: simultaneous clicks cannot enqueue two jobs.
        let selection = {
            let mut sessions = s.sessions.lock().await;
            take_selection(sessions.entry(u.id).or_default(), Some(expected))?
        };
        for id in &selection.selection {
            if db::video(&s.pool, u, *id).await?.is_none() {
                bail!("Доступ изменился");
            }
        }
        let id = db::enqueue(&s.pool, u, chat.0, &selection.selection, &selection.mode).await?;
        finish_panel(
            bot,
            chat,
            s,
            PanelKind::Selection,
            "✅ Запрос принят. Прогресс доступен через «Мои запросы».",
            job_buttons(id),
        )
        .await?;
    } else if let Some(nonce) = data.strip_prefix("discard:") {
        let expected = Uuid::parse_str(nonce)?;
        {
            let mut sessions = s.sessions.lock().await;
            take_selection(sessions.entry(u.id).or_default(), Some(expected))?;
        }
        finish_panel(
            bot,
            chat,
            s,
            PanelKind::Selection,
            "Выборка отменена.",
            K::new(Vec::<Vec<B>>::new()),
        )
        .await?;
    } else if let Some(kind) = data.strip_prefix("pick:") {
        let rows: Vec<(i64, String)> = match kind {
            "device" => sqlx::query_as("SELECT id,name FROM devices WHERE active ORDER BY id LIMIT 50").fetch_all(&s.pool).await?,
            "system" => sqlx::query_as("SELECT id,name FROM video_systems WHERE active ORDER BY id LIMIT 50").fetch_all(&s.pool).await?,
            "pilot" => sqlx::query_as("SELECT id,coalesce(nickname,full_name,telegram_username,'Пилот') FROM users WHERE active AND (id=$1 OR is_public OR $2) ORDER BY id LIMIT 50")
                .bind(u.id).bind(u.is_admin).fetch_all(&s.pool).await?,
            _ => bail!("Неизвестный фильтр"),
        };
        panels::leave_form(bot, chat, s, u).await;
        let mut keys = rows
            .into_iter()
            .map(|(id, name)| vec![B::callback(name, format!("set:{kind}:{id}"))])
            .collect::<Vec<_>>();
        keys.push(vec![B::callback("← Назад к фильтрам", "filters")]);
        show_panel(
            bot,
            chat,
            s,
            PanelKind::Search,
            "Выберите значение:",
            K::new(keys),
        )
        .await?;
    } else if let Some(value) = data.strip_prefix("set:") {
        let (kind, id) = value.split_once(':').context("callback")?;
        let id: i64 = id.parse()?;
        {
            let mut sessions = s.sessions.lock().await;
            let f = &mut sessions.entry(u.id).or_default().filters;
            match kind {
                "device" => f.device = Some(id),
                "system" => f.system = Some(id),
                "pilot" => f.pilot = Some(id),
                _ => bail!("Неизвестный фильтр"),
            }
            f.page = 0;
        }
        show_search(bot, chat, s, u).await?;
    } else if let Some(kind) = data.strip_prefix("toggle:") {
        {
            let mut sessions = s.sessions.lock().await;
            let f = &mut sessions.entry(u.id).or_default().filters;
            let value = match kind {
                "osd" => &mut f.osd,
                "colored" => &mut f.colored,
                _ => bail!("Неизвестный фильтр"),
            };
            *value = match *value {
                None => Some(true),
                Some(true) => Some(false),
                Some(false) => None,
            };
            f.page = 0;
        }
        show_search(bot, chat, s, u).await?;
    } else if let Some(mode) = data.strip_prefix("all:") {
        handle(bot, chat, s, u, Command::All(mode.into())).await?;
    } else if let Some((action, id)) = data.split_once(':') {
        let id = id.parse()?;
        let cmd = match action {
            "info" => Command::Info(id),
            "link" => Command::Link(id),
            "video" => Command::Video(id),
            "document" => Command::Document(id),
            "fallback" => Command::Fallback(id),
            _ => bail!("Неизвестное действие"),
        };
        handle(bot, chat, s, u, cmd).await?;
    } else {
        bail!("Неизвестное действие");
    }
    Ok(())
}
async fn show_filters(bot: &Bot, chat: ChatId, s: &BotState, u: &User) -> Result<()> {
    panels::leave_form(bot, chat, s, u).await;
    show_panel(bot,chat,s,PanelKind::Search, "Выберите фильтр. Для OSD и цветокоррекции повторное нажатие переключает: любые → да → нет. Даты можно задать командой /search из помощи.",K::new(vec![
            vec![B::callback("Аппарат", "pick:device"), B::callback("Видеосистема", "pick:system")],
            vec![B::callback("Пилот", "pick:pilot"), B::callback("OSD", "toggle:osd")],
            vec![B::callback("Цветокоррекция", "toggle:colored"), B::callback("Сбросить", "reset")],
            vec![B::callback("Показать видео", "search")],
        ])).await?;
    Ok(())
}
async fn show_subscriptions(bot: &Bot, chat: ChatId, s: &BotState, u: &User) -> Result<()> {
    panels::leave_form(bot, chat, s, u).await;
    let rows: Vec<(i64,String,bool)> = sqlx::query_as("SELECT p.id,coalesce(p.nickname,p.full_name,p.telegram_username,'Пилот'),EXISTS(SELECT 1 FROM notification_subscriptions n WHERE n.user_id=$1 AND n.pilot_id=p.id) FROM users p WHERE p.active AND (p.id=$1 OR p.is_public OR $2) ORDER BY p.id LIMIT 50")
        .bind(u.id).bind(u.is_admin).fetch_all(&s.pool).await?;
    let keys = rows.into_iter().map(|(id, name, selected)| {
        vec![B::callback(
            format!("{} {name}", if selected { "✅" } else { "⬜️" }),
            format!("subscription:{id}"),
        )]
    });
    show_panel(bot,chat,s,PanelKind::Subscriptions, "Уведомления о новых видео: нажмите на пилота, чтобы включить или выключить подписку. ✅ — подписка включена. Время дайджеста настраивается через кнопку «Уведомления».",K::new(keys)).await?;
    Ok(())
}
async fn show_notifications(bot: &Bot, chat: ChatId, s: &BotState, u: &User) -> Result<()> {
    panels::leave_form(bot, chat, s, u).await;
    let (enabled, time): (bool, NaiveTime) =
        sqlx::query_as("SELECT notifications_enabled,notification_time FROM users WHERE id=$1")
            .bind(u.id)
            .fetch_one(&s.pool)
            .await?;
    show_panel(bot,chat,s,PanelKind::Notifications, format!("Ежедневный дайджест: {}. Время: {} по Москве. Выберите настройку ниже. Для другого времени: /notify ЧЧ:ММ.", if enabled {"включён"} else {"выключен"}, time.format("%H:%M")),K::new(vec![
            vec![B::callback("Включить", "notify:on"),B::callback("Выключить", "notify:off")],
            vec![B::callback("06:00", "notify:06:00"),B::callback("08:00", "notify:08:00"),B::callback("10:00", "notify:10:00")],
            vec![B::callback("12:00", "notify:12:00"),B::callback("18:00", "notify:18:00"),B::callback("21:00", "notify:21:00")],
        ])).await?;
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceInput {
    id: Option<i64>,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    aliases: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemInput {
    id: Option<i64>,
    name: String,
    with_osd: bool,
    aliases: Vec<String>,
}
fn aliases_valid(name: &str, a: &[String]) -> Result<()> {
    if name.trim().is_empty() || a.is_empty() || a.iter().any(|s| s.trim().is_empty()) {
        bail!("Нужны название и непустые aliases");
    }
    Ok(())
}
async fn save_device(p: &PgPool, d: DeviceInput) -> Result<i64> {
    aliases_valid(&d.name, &d.aliases)?;
    if !["plane", "quad", "other"].contains(&d.kind.as_str()) {
        bail!("type: plane/quad/other");
    }
    let mut tx = p.begin().await?;
    let id: i64 = if let Some(id) = d.id {
        sqlx::query_scalar(
            "UPDATE devices SET name=$2,type=$3,active=true WHERE id=$1 RETURNING id",
        )
        .bind(id)
        .bind(d.name)
        .bind(d.kind)
        .fetch_one(&mut *tx)
        .await?
    } else {
        sqlx::query_scalar("INSERT INTO devices(name,type) VALUES($1,$2) RETURNING id")
            .bind(d.name)
            .bind(d.kind)
            .fetch_one(&mut *tx)
            .await?
    };
    sqlx::query("DELETE FROM device_aliases WHERE device_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for a in d.aliases {
        sqlx::query("INSERT INTO device_aliases VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(id)
            .bind(a)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(id)
}
async fn save_system(p: &PgPool, d: SystemInput) -> Result<i64> {
    aliases_valid(&d.name, &d.aliases)?;
    let mut tx = p.begin().await?;
    let id: i64 = if let Some(id) = d.id {
        sqlx::query_scalar(
            "UPDATE video_systems SET name=$2,with_osd=$3,active=true WHERE id=$1 RETURNING id",
        )
        .bind(id)
        .bind(d.name)
        .bind(d.with_osd)
        .fetch_one(&mut *tx)
        .await?
    } else {
        sqlx::query_scalar("INSERT INTO video_systems(name,with_osd) VALUES($1,$2) RETURNING id")
            .bind(d.name)
            .bind(d.with_osd)
            .fetch_one(&mut *tx)
            .await?
    };
    sqlx::query("DELETE FROM video_system_aliases WHERE video_system_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for a in d.aliases {
        sqlx::query("INSERT INTO video_system_aliases VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(id)
            .bind(a)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(id)
}
async fn audit(p: &PgPool, u: i64, a: &str, id: i64) -> Result<()> {
    sqlx::query("INSERT INTO audit_log(actor_id,action,entity) VALUES($1,$2,$3)")
        .bind(u)
        .bind(a)
        .bind(id.to_string())
        .execute(p)
        .await?;
    Ok(())
}
async fn saved(bot: &Bot, chat: ChatId, s: &BotState, id: i64) -> Result<()> {
    bot.send_message(
        chat,
        format!("Сохранено, ID {id}. Переклассификация запущена."),
    )
    .await?;
    reclassify(s);
    Ok(())
}
async fn send_chunks(bot: &Bot, chat: ChatId, text: &str) -> Result<()> {
    let chars = text.chars().collect::<Vec<_>>();
    for c in chars.chunks(3500) {
        bot.send_message(chat, c.iter().collect::<String>()).await?;
    }
    if chars.is_empty() {
        bot.send_message(chat, "Список пуст.").await?;
    }
    Ok(())
}

fn take_selection(session: &mut Session, expected: Option<Uuid>) -> Result<Session> {
    if expected.is_some() && session.nonce != expected {
        bail!("Эта выборка уже заменена или подтверждена");
    }
    if session.selection.is_empty() {
        bail!("Сначала выберите видео");
    }
    let selected = Session {
        filters: session.filters.clone(),
        selection: std::mem::take(&mut session.selection),
        mode: session.mode.clone(),
        nonce: session.nonce.take(),
        user_form: None,
    };
    Ok(selected)
}

fn job_buttons(id: Uuid) -> K {
    K::new(vec![vec![
        B::callback("Прогресс", "jobs"),
        B::callback("Остановить", format!("canceljob:{id}")),
    ]])
}
async fn filter_summary(pool: &PgPool, u: &User, f: &Filters) -> Result<String> {
    let mut parts = vec![];
    if f.from.is_some() || f.to.is_some() {
        parts.push(format!(
            "Дата: {} — {}",
            f.from
                .map(|d| d.format("%d.%m.%Y").to_string())
                .unwrap_or("с начала архива".into()),
            f.to.map(|d| d.format("%d.%m.%Y").to_string())
                .unwrap_or("до конца архива".into())
        ));
    }
    if let Some(id) = f.device {
        let name: Option<String> = sqlx::query_scalar("SELECT name FROM devices WHERE id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
        parts.push(format!("Аппарат: {}", name.as_deref().unwrap_or("удалён")));
    }
    if let Some(id) = f.system {
        let name: Option<String> = sqlx::query_scalar("SELECT name FROM video_systems WHERE id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
        parts.push(format!(
            "Видеосистема: {}",
            name.as_deref().unwrap_or("удалена")
        ));
    }
    if let Some(id) = f.pilot {
        let name: Option<String> = sqlx::query_scalar("SELECT coalesce(nickname,full_name,telegram_username,'Пилот') FROM users WHERE id=$1 AND active AND (id=$2 OR is_public OR $3)").bind(id).bind(u.id).bind(u.is_admin).fetch_optional(pool).await?;
        parts.push(format!(
            "Пилот: {}",
            name.as_deref().unwrap_or("недоступен")
        ));
    }
    if let Some(value) = f.osd {
        parts.push(format!("OSD: {}", if value { "есть" } else { "нет" }));
    }
    if let Some(value) = f.colored {
        parts.push(format!(
            "Цветокоррекция: {}",
            if value { "есть" } else { "нет" }
        ));
    }
    if let Some(value) = &f.text {
        parts.push(format!("Поиск по названию: {value}"));
    }
    Ok(if parts.is_empty() {
        "Показаны все доступные вам видео.".into()
    } else {
        format!("Выбраны фильтры:\n{}", parts.join("\n"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn confirmations_are_bound_to_snapshot_and_consumed_once() {
        let nonce = Uuid::new_v4();
        let mut s = Session {
            selection: vec![1, 2],
            mode: "immich".into(),
            nonce: Some(nonce),
            ..Default::default()
        };
        assert!(take_selection(&mut s, Some(Uuid::new_v4())).is_err());
        assert_eq!(
            take_selection(&mut s, Some(nonce)).unwrap().selection,
            vec![1, 2]
        );
        assert!(take_selection(&mut s, Some(nonce)).is_err());
    }
}

#[cfg(test)]
mod flow_tests;
