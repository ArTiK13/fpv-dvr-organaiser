use crate::{
    config::Config,
    db::{self, Filters, User},
    downloads::{self, DownloadState},
    scanner,
};
use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use serde::Deserialize;
use sqlx::{PgPool, Row};
use std::{collections::HashMap, sync::Arc};
use teloxide::{
    prelude::*,
    types::{CallbackQuery, InlineKeyboardButton as B, InlineKeyboardMarkup as K},
    utils::command::BotCommands,
};
use tokio::sync::Mutex;
use uuid::Uuid;
#[derive(Default, Clone)]
struct Session {
    filters: Filters,
    selection: Vec<i64>,
    mode: String,
    nonce: Option<Uuid>,
}
#[derive(Clone)]
pub struct BotState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    sessions: Arc<Mutex<HashMap<i64, Session>>>,
}
impl BotState {
    pub fn new(pool: PgPool, config: Arc<Config>) -> Self {
        Self {
            pool,
            config,
            sessions: Arc::default(),
        }
    }
}
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "FPV-каталог:")]
enum Command {
    #[command(description = "главное меню")]
    Start,
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
    let result=async{
 let Some(u)=db::user(&s.pool,tg).await?.filter(|u|u.active)else{
  if matches!(cmd,Command::Start){sqlx::query("INSERT INTO access_requests(telegram_id,username,display_name) VALUES($1,$2,$3) ON CONFLICT(telegram_id) DO UPDATE SET username=EXCLUDED.username,display_name=EXCLUDED.display_name,resolved_at=NULL,requested_at=now()").bind(tg).bind(&from.username).bind(from.first_name.clone()).execute(&s.pool).await?;bot.send_message(msg.chat.id,"Заявка сохранена. Администратор должен выдать доступ. После добавления нажмите /start.").await?;}
  else{bot.send_message(msg.chat.id,"Доступ закрыт. /start — подать заявку.").await?;}return Ok(());
 };
 handle(&bot,msg.chat.id,&s,&u,cmd).await
}.await;
    if let Err(e) = result {
        tracing::warn!(error=%s.config.redact(&e.to_string()),"bot command failed");
        bot.send_message(
            msg.chat.id,
            format!(
                "Ошибка: {}",
                s.config
                    .redact(&e.to_string())
                    .chars()
                    .take(1500)
                    .collect::<String>()
            ),
        )
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
async fn handle(bot: &Bot, chat: ChatId, s: &BotState, u: &User, cmd: Command) -> Result<()> {
    let pool = &s.pool;
    match cmd {
        Command::Start => {
            bot.send_message(chat, "FPV-каталог. Выберите действие:")
                .reply_markup(K::new(vec![
                    vec![
                        B::callback("Поиск", "search"),
                        B::callback("Фильтры", "filters"),
                    ],
                    vec![
                        B::callback("Подписки", "subscriptions"),
                        B::callback("Выдача", "jobs"),
                    ],
                ]))
                .await?;
        }
        Command::Help => {
            let text=format!("{}\n\nПоиск: /search {{\"from\":\"2026-09-01\",\"to\":\"2026-09-30\",\"osd\":true}}\nАппарат: /device {{\"name\":\"T1\",\"type\":\"quad\",\"aliases\":[\"t1\"]}}\nСистема: /system {{\"name\":\"O4 Pro\",\"with_osd\":false,\"aliases\":[\"o4pro\"]}}\nПилот: /user {{\"telegram_id\":123456,\"nickname\":\"Kostyan\",\"aliases\":[\"kostyanbtw\"]}}\nДля редактирования добавьте поле id. Даты без дня учитывают весь месяц. Время — Москва.",Command::descriptions());
            bot.send_message(chat, text).await?;
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
            bot.send_message(
                chat,
                format!("Запрос {job} принят. /jobs — прогресс; /cancel {job} — остановить."),
            )
            .await?;
        }
        Command::Jobs => {
            let jobs=sqlx::query("SELECT id,state,total_count,completed_count FROM delivery_jobs WHERE user_id=$1 ORDER BY created_at DESC LIMIT 10").bind(u.id).fetch_all(pool).await?;
            let text = jobs
                .iter()
                .map(|r| {
                    format!(
                        "{}: {} {}/{}",
                        r.get::<Uuid, _>("id"),
                        r.get::<String, _>("state"),
                        r.get::<i32, _>("completed_count"),
                        r.get::<i32, _>("total_count")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            bot.send_message(
                chat,
                if text.is_empty() {
                    "Нет запросов"
                } else {
                    &text
                },
            )
            .await?;
        }
        Command::Cancel(id) => {
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
            bot.send_message(chat, "Настройки уведомлений сохранены (Москва).")
                .await?;
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
            bot.send_message(chat, "Подписки сохранены.").await?;
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
            let alerts: Vec<String> = sqlx::query_scalar(
                "SELECT message FROM system_alerts WHERE active ORDER BY created_at LIMIT 10",
            )
            .fetch_all(pool)
            .await?;
            let jobs: Vec<(String, i64)> = sqlx::query_as(
                "SELECT state,count(*) FROM delivery_jobs GROUP BY state ORDER BY state",
            )
            .fetch_all(pool)
            .await?;
            let text = format!("Видео: {}\nВыдача: {}\nПроблемы классификации: {count}\n{}\nОшибки файлов: {media_count}\n{}\nСистемные проблемы:\n{}",
                states.iter().map(|(s,n)|format!("{s}: {n}")).collect::<Vec<_>>().join(", "),
                jobs.iter().map(|(s,n)|format!("{s}: {n}")).collect::<Vec<_>>().join(", "),
                issues.iter().map(|(id,p,e)|format!("#{id} {p}: {e}")).collect::<Vec<_>>().join("\n"),
                media.iter().map(|(p,e)|format!("{p}: {e}")).collect::<Vec<_>>().join("\n"),
                alerts.join("\n"));
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
                    Err(e) => format!("Сканирование: {}", c.redact(&e.to_string())),
                };
                let _ = b.send_message(chat, text).await;
            });
        }
        Command::User(json) => {
            admin(u)?;
            let id = db::save_user(
                pool,
                u.id,
                serde_json::from_str(&json)?,
                s.config.bootstrap_admin_chat_id,
            )
            .await?;
            saved(bot, chat, s, id).await?;
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
            let text = match kind.trim() {
                "users" => {
                    let rows = sqlx::query(
                        "SELECT id,row_to_json(users)::text AS data FROM users ORDER BY id",
                    )
                    .fetch_all(pool)
                    .await?;
                    rows.iter()
                        .take(15)
                        .map(|r| r.get::<String, _>("data"))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                "devices" => {
                    let rows=sqlx::query("SELECT d.id,d.name,d.type,d.active,coalesce(array_agg(a.alias) FILTER (WHERE a.alias IS NOT NULL),'{}') AS aliases FROM devices d LEFT JOIN device_aliases a ON a.device_id=d.id GROUP BY d.id ORDER BY d.id LIMIT 20").fetch_all(pool).await?;
                    rows.iter()
                        .map(|r| {
                            format!(
                                "#{} {} {} {:?}",
                                r.get::<i64, _>("id"),
                                r.get::<String, _>("name"),
                                r.get::<String, _>("type"),
                                r.get::<Vec<String>, _>("aliases")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                "systems" => {
                    let rows = sqlx::query(
                        "SELECT id,name,with_osd FROM video_systems ORDER BY id LIMIT 20",
                    )
                    .fetch_all(pool)
                    .await?;
                    rows.iter()
                        .map(|r| {
                            format!(
                                "#{} {} OSD={}",
                                r.get::<i64, _>("id"),
                                r.get::<String, _>("name"),
                                r.get::<bool, _>("with_osd")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                _ => bail!("users/devices/systems"),
            };
            send_chunks(bot, chat, &text).await?;
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
            let rows:Vec<(i64,Option<String>,Option<String>)>=sqlx::query_as("SELECT telegram_id,username,display_name FROM access_requests WHERE resolved_at IS NULL LIMIT 20").fetch_all(pool).await?;
            bot.send_message(
                chat,
                format!(
                    "Заявки:\n{}\nДобавление: /user {{\"telegram_id\":ID,\"nickname\":\"имя\"}}",
                    rows.iter()
                        .map(|(id, name, display)| format!("{id} {name:?} {display:?}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            )
            .await?;
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
    bot.send_message(chat,format!("Выбрано {count} видео ({:.2} ГБ). Способ: {mode}. Большие файлы — Immich. Подтвердите выдачу.",bytes as f64/1e9)).reply_markup(K::new(vec![vec![B::callback("Подтвердить",format!("confirm:{nonce}")),B::callback("Отменить",format!("discard:{nonce}"))]])).await?;
    Ok(())
}
async fn show_search(bot: &Bot, chat: ChatId, s: &BotState, u: &User) -> Result<()> {
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
    bot.send_message(
        chat,
        format!(
            "Найдено: {n}, {:.2} ГБ. Страница {}\nФильтры: {}",
            bytes as f64 / 1e9,
            f.page + 1,
            serde_json::to_string(&f)?
        ),
    )
    .reply_markup(K::new(keyboard))
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
        keys.push(vec![B::callback("Fallback", format!("fallback:{id}"))]);
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
    bot.answer_callback_query(&q.id).await?;
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
        let text = s.config.redact(&e.to_string());
        bot.send_message(chat, format!("Ошибка: {text}")).await?;
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
    match data {
        "search" => return show_search(bot, chat, s, u).await,
        "next" => return handle(bot, chat, s, u, Command::Next).await,
        "jobs" => return handle(bot, chat, s, u, Command::Jobs).await,
        "filters" => {
            bot.send_message(chat, "Выберите фильтр. Даты задаются через /search JSON; это сбрасывает остальные фильтры.")
                .reply_markup(K::new(vec![
                    vec![B::callback("Аппарат", "pick:device"), B::callback("Система", "pick:system")],
                    vec![B::callback("Пилот", "pick:pilot"), B::callback("OSD", "toggle:osd"), B::callback("Colored", "toggle:colored")],
                    vec![B::callback("Сбросить", "reset")],
                ])).await?;
            return Ok(());
        }
        "reset" => {
            s.sessions.lock().await.entry(u.id).or_default().filters = Filters::default();
            return show_search(bot, chat, s, u).await;
        }
        "subscriptions" => {
            let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id,coalesce(nickname,full_name,telegram_username,'Пилот') FROM users WHERE active AND (id=$1 OR is_public OR $2) ORDER BY id")
                .bind(u.id).bind(u.is_admin).fetch_all(&s.pool).await?;
            let names = rows
                .iter()
                .map(|(id, n)| format!("#{id}: {n}"))
                .collect::<Vec<_>>()
                .join("\n");
            send_chunks(bot, chat, &format!("Доступные пилоты:\n{names}\n/subscribe ID ID — выбрать; /subscribe none — очистить\n/notify HH:MM — время; /notify off — выключить")).await?;
            return Ok(());
        }
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
        bot.send_message(
            chat,
            format!("Запрос {id} принят. /jobs — прогресс; /cancel {id} — остановить."),
        )
        .await?;
    } else if let Some(nonce) = data.strip_prefix("discard:") {
        let expected = Uuid::parse_str(nonce)?;
        {
            let mut sessions = s.sessions.lock().await;
            take_selection(sessions.entry(u.id).or_default(), Some(expected))?;
        }
        bot.send_message(chat, "Выборка отменена.").await?;
    } else if let Some(kind) = data.strip_prefix("pick:") {
        let rows: Vec<(i64, String)> = match kind {
            "device" => sqlx::query_as("SELECT id,name FROM devices WHERE active ORDER BY id LIMIT 50").fetch_all(&s.pool).await?,
            "system" => sqlx::query_as("SELECT id,name FROM video_systems WHERE active ORDER BY id LIMIT 50").fetch_all(&s.pool).await?,
            "pilot" => sqlx::query_as("SELECT id,coalesce(nickname,full_name,telegram_username,'Пилот') FROM users WHERE active AND (id=$1 OR is_public OR $2) ORDER BY id LIMIT 50")
                .bind(u.id).bind(u.is_admin).fetch_all(&s.pool).await?,
            _ => bail!("Неизвестный фильтр"),
        };
        bot.send_message(chat, "Выберите значение:")
            .reply_markup(K::new(rows.into_iter().map(|(id, name)| {
                vec![B::callback(name, format!("set:{kind}:{id}"))]
            })))
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
    let p = s.pool.clone();
    let c = s.config.clone();
    tokio::spawn(async move {
        if let Err(e) = scanner::scan_once(&p, &c).await {
            tracing::warn!(error=%c.redact(&e.to_string()),"reclassification");
        }
    });
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
    };
    Ok(selected)
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
