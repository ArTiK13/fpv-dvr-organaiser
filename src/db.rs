use anyhow::{bail, Result};
use chrono::{NaiveDate, NaiveTime};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

#[derive(Clone, Debug, FromRow)]
pub struct User {
    pub id: i64,
    pub telegram_id: i64,
    pub is_admin: bool,
    pub is_public: bool,
    pub active: bool,
}
pub async fn user(pool: &PgPool, tg: i64) -> Result<Option<User>> {
    Ok(sqlx::query_as(
        "SELECT id,telegram_id,is_admin,is_public,active FROM users WHERE telegram_id=$1",
    )
    .bind(tg)
    .fetch_optional(pool)
    .await?)
}
/// Change one subscription without resetting other pilots' notification cursors.
pub async fn toggle_subscription(pool: &PgPool, user_id: i64, pilot_id: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    let active: Option<i64> =
        sqlx::query_scalar("SELECT id FROM users WHERE id=$1 AND active FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    if active.is_none() {
        bail!("Доступ закрыт");
    }
    let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users u JOIN users p ON p.id=$2 WHERE u.id=$1 AND p.active AND (p.id=u.id OR p.is_public OR u.is_admin))")
        .bind(user_id).bind(pilot_id).fetch_one(&mut *tx).await?;
    if !allowed {
        bail!("Недоступный пилот");
    }
    let removed =
        sqlx::query("DELETE FROM notification_subscriptions WHERE user_id=$1 AND pilot_id=$2")
            .bind(user_id)
            .bind(pilot_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    if removed == 0 {
        sqlx::query("INSERT INTO notification_subscriptions(user_id,pilot_id) VALUES($1,$2)")
            .bind(user_id)
            .bind(pilot_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn bootstrap(pool: &PgPool, tg: i64) -> Result<()> {
    let existing = user(pool, tg).await?;
    sqlx::query("INSERT INTO users(telegram_id,full_name,is_admin) VALUES($1,'Главный администратор',true) ON CONFLICT(telegram_id) DO UPDATE SET is_admin=true,active=true").bind(tg).execute(pool).await?;
    if existing.is_none() {
        let u = user(pool, tg).await?.unwrap();
        sqlx::query("INSERT INTO notification_subscriptions(user_id,pilot_id) SELECT $1,id FROM users WHERE active ON CONFLICT DO NOTHING").bind(u.id).execute(pool).await?;
    }
    Ok(())
}
#[derive(Clone, Default, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Filters {
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub device: Option<i64>,
    pub system: Option<i64>,
    pub osd: Option<bool>,
    pub colored: Option<bool>,
    pub pilot: Option<i64>,
    pub text: Option<String>,
    #[serde(default)]
    pub page: u32,
}
#[derive(Clone, Debug, FromRow)]
pub struct Video {
    pub id: i64,
    pub relative_path: String,
    pub size_bytes: i64,
    pub duration_ms: Option<i64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub fps: Option<String>,
    pub event_name: String,
    pub event_id: i64,
    pub device_name: Option<String>,
    pub system_name: Option<String>,
    pub pilot_name: Option<String>,
    pub with_osd: Option<bool>,
    pub colored: bool,
    pub state: String,
    pub date_start: Option<NaiveDate>,
    pub date_end: Option<NaiveDate>,
}
const SELECT:&str="SELECT v.id,v.relative_path,v.size_bytes,v.duration_ms,v.width,v.height,v.fps,e.name AS event_name,e.id AS event_id,d.name AS device_name,s.name AS system_name,coalesce(p.nickname,p.full_name,p.telegram_username) AS pilot_name,s.with_osd,v.colored,v.state,e.date_start,e.date_end";
const JOINS:&str=" FROM videos v JOIN events e ON e.id=v.event_id LEFT JOIN devices d ON d.id=v.device_id LEFT JOIN video_systems s ON s.id=v.video_system_id LEFT JOIN users p ON p.id=v.pilot_id ";
fn restrictions(q: &mut QueryBuilder<'_, Postgres>, u: &User, f: &Filters) {
    q.push(" WHERE v.state IN ('classified','ready','waiting_for_immich') AND p.active=true ");
    if !u.is_admin {
        q.push(" AND (v.pilot_id=")
            .push_bind(u.id)
            .push(" OR p.is_public=true)");
    }
    if let Some(v) = f.from {
        q.push(" AND e.date_end>=").push_bind(v);
    }
    if let Some(v) = f.to {
        q.push(" AND e.date_start<=").push_bind(v);
    }
    if let Some(v) = f.device {
        q.push(" AND v.device_id=").push_bind(v);
    }
    if let Some(v) = f.system {
        q.push(" AND v.video_system_id=").push_bind(v);
    }
    if let Some(v) = f.pilot {
        q.push(" AND v.pilot_id=").push_bind(v);
    }
    if let Some(v) = f.osd {
        q.push(" AND s.with_osd=").push_bind(v);
    }
    if let Some(v) = f.colored {
        q.push(" AND v.colored=").push_bind(v);
    }
    if let Some(v) = &f.text {
        q.push(" AND v.relative_path ILIKE ")
            .push_bind(format!("%{v}%"));
    }
}
pub async fn search(pool: &PgPool, u: &User, f: &Filters) -> Result<(i64, i64, Vec<Video>)> {
    if matches!((f.from,f.to),(Some(a),Some(b)) if a>b) {
        bail!("Начало диапазона позже конца");
    }
    let mut count = QueryBuilder::new(format!(
        "SELECT count(*),coalesce(sum(v.size_bytes),0)::bigint{JOINS}"
    ));
    restrictions(&mut count, u, f);
    let (n, bytes) = count.build_query_as::<(i64, i64)>().fetch_one(pool).await?;
    let mut q = QueryBuilder::new(format!("{SELECT}{JOINS}"));
    restrictions(&mut q, u, f);
    q.push(" ORDER BY v.id DESC LIMIT 10 OFFSET ")
        .push_bind(i64::from(f.page) * 10);
    let rows = q.build_query_as().fetch_all(pool).await?;
    Ok((n, bytes, rows))
}
pub async fn selection(pool: &PgPool, u: &User, f: &Filters) -> Result<Vec<i64>> {
    let mut q = QueryBuilder::new(format!("SELECT v.id{JOINS}"));
    restrictions(&mut q, u, f);
    q.push(" ORDER BY v.id");
    Ok(q.build_query_scalar().fetch_all(pool).await?)
}
pub async fn video(pool: &PgPool, u: &User, id: i64) -> Result<Option<Video>> {
    let mut q = QueryBuilder::new(format!("{SELECT}{JOINS}"));
    restrictions(&mut q, u, &Filters::default());
    q.push(" AND v.id=").push_bind(id);
    Ok(q.build_query_as().fetch_optional(pool).await?)
}
pub async fn enqueue(pool: &PgPool, u: &User, chat: i64, ids: &[i64], mode: &str) -> Result<Uuid> {
    if ids.is_empty() {
        bail!("Пустая выборка");
    }
    let id = Uuid::new_v4();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO delivery_jobs(id,user_id,chat_id,mode,total_count) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(id)
    .bind(u.id)
    .bind(chat)
    .bind(mode)
    .bind(ids.len() as i32)
    .execute(&mut *tx)
    .await?;
    for v in ids {
        sqlx::query("INSERT INTO delivery_job_videos(job_id,video_id) VALUES($1,$2)")
            .bind(id)
            .bind(v)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(id)
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserInput {
    pub id: Option<i64>,
    pub telegram_id: i64,
    pub full_name: Option<String>,
    pub nickname: Option<String>,
    pub telegram_username: Option<String>,
    #[serde(default)]
    pub is_admin: bool,
    #[serde(default = "yes")]
    pub is_public: bool,
    #[serde(default)]
    pub aliases: Vec<String>,
}
fn yes() -> bool {
    true
}
pub async fn save_user(pool: &PgPool, actor: i64, v: UserInput, bootstrap: i64) -> Result<i64> {
    if v.telegram_id <= 0
        || (v.aliases.is_empty()
            && [&v.full_name, &v.nickname, &v.telegram_username]
                .iter()
                .all(|x| x.as_ref().is_none_or(|s| s.trim().is_empty())))
    {
        bail!("Нужен Telegram ID и хотя бы одно имя или alias");
    }
    if v.telegram_id == bootstrap && !v.is_admin {
        bail!("Главного администратора нельзя понизить");
    }
    if let Some(id) = v.id {
        let old_tg: Option<i64> = sqlx::query_scalar("SELECT telegram_id FROM users WHERE id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
        if old_tg == Some(bootstrap) && v.telegram_id != bootstrap {
            bail!("Нельзя изменить Telegram ID главного администратора");
        }
    }
    let previous = if let Some(id) = v.id {
        sqlx::query_as::<_, (bool, bool, i64)>(
            "SELECT is_admin,active,telegram_id FROM users WHERE id=$1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await?
    } else {
        None
    };
    let was_admin = previous.is_some_and(|(a, active, _)| a && active);
    let is_admin = v.is_admin;
    let new_user = v.id.is_none();
    let mut tx = pool.begin().await?;
    // Recheck the actor's current role at the write boundary, including wizard submissions.
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id=$1 AND active AND is_admin)",
    )
    .bind(actor)
    .fetch_one(&mut *tx)
    .await?;
    if !allowed {
        bail!("Требуются права администратора");
    }
    let id: i64 = if let Some(id) = v.id {
        sqlx::query_scalar("UPDATE users SET telegram_id=$2,full_name=$3,nickname=$4,telegram_username=$5,is_admin=$6,is_public=$7,active=true WHERE id=$1 RETURNING id").bind(id).bind(v.telegram_id).bind(v.full_name).bind(v.nickname).bind(v.telegram_username).bind(v.is_admin).bind(v.is_public).fetch_one(&mut *tx).await?
    } else {
        sqlx::query_scalar("INSERT INTO users(telegram_id,full_name,nickname,telegram_username,is_admin,is_public) VALUES($1,$2,$3,$4,$5,$6) RETURNING id").bind(v.telegram_id).bind(v.full_name).bind(v.nickname).bind(v.telegram_username).bind(v.is_admin).bind(v.is_public).fetch_one(&mut *tx).await?
    };
    sqlx::query("DELETE FROM user_file_aliases WHERE user_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for a in v.aliases {
        if a.trim().is_empty() {
            bail!("Пустой alias");
        }
        sqlx::query("INSERT INTO user_file_aliases VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(id)
            .bind(a)
            .execute(&mut *tx)
            .await?;
    }
    if new_user {
        sqlx::query("INSERT INTO notification_subscriptions(user_id,pilot_id) VALUES($1,$1) ON CONFLICT DO NOTHING")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE access_requests SET resolved_at=now() WHERE telegram_id=$1")
        .bind(v.telegram_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO audit_log(actor_id,action,entity) VALUES($1,'save_user',$2)")
        .bind(actor)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
    if new_user {
        sqlx::query("INSERT INTO notification_subscriptions(user_id,pilot_id) SELECT id,$1 FROM users WHERE is_admin AND active ON CONFLICT DO NOTHING").bind(id).execute(&mut *tx).await?;
    }
    if is_admin && !was_admin {
        sqlx::query("INSERT INTO notification_subscriptions(user_id,pilot_id) SELECT $1,id FROM users WHERE active ON CONFLICT DO NOTHING").bind(id).execute(&mut *tx).await?;
    }
    if previous.is_some_and(|(_, _, tg)| tg != v.telegram_id) {
        sqlx::query("UPDATE immich_shared_links SET revoke_requested=true WHERE user_id=$1 AND revoked_at IS NULL").bind(id).execute(&mut *tx).await?;
        sqlx::query("UPDATE delivery_jobs SET state='cancelled' WHERE user_id=$1 AND state NOT IN ('done','failed','cancelled')").bind(id).execute(&mut *tx).await?;
        sqlx::query("UPDATE fallback_download_tokens SET revoked_at=now() WHERE user_id=$1 AND revoked_at IS NULL").bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    invalidate_links(pool).await?;
    Ok(id)
}
pub async fn invalidate_links(pool: &PgPool) -> Result<()> {
    sqlx::query("UPDATE immich_shared_links l SET revoke_requested=true WHERE revoked_at IS NULL AND (NOT EXISTS(SELECT 1 FROM users u WHERE u.id=l.user_id AND u.active) OR EXISTS(SELECT 1 FROM immich_shared_link_videos lv JOIN videos v ON v.id=lv.video_id LEFT JOIN users p ON p.id=v.pilot_id JOIN users u ON u.id=l.user_id WHERE lv.link_id=l.id AND (NOT coalesce(p.active,false) OR v.state IN ('missing','conflict','unclassified') OR (NOT u.is_admin AND v.pilot_id<>u.id AND NOT p.is_public))))").execute(pool).await?;
    Ok(())
}
pub fn parse_time(v: &str) -> Result<NaiveTime> {
    Ok(NaiveTime::parse_from_str(v, "%H:%M")?)
}
