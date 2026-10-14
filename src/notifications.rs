use crate::config::Config;
use anyhow::Result;
use chrono::{DateTime, NaiveDate, NaiveTime, Timelike, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use teloxide::{prelude::*, types::ChatId};
use tracing::warn;

pub async fn alert(pool: &PgPool, key: &str, msg: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    let previous: Option<bool> =
        sqlx::query_scalar("SELECT active FROM system_alerts WHERE key=$1 FOR UPDATE")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
    sqlx::query("INSERT INTO system_alerts(key,message) VALUES($1,$2) ON CONFLICT(key) DO UPDATE SET message=EXCLUDED.message,active=true")
        .bind(key).bind(msg).execute(&mut *tx).await?;
    if previous == Some(false) {
        sqlx::query("DELETE FROM system_alert_deliveries WHERE alert_key=$1")
            .bind(key)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn run(pool: PgPool, c: Arc<Config>) {
    let bot = Bot::new(c.telegram_bot_token.clone());
    loop {
        if let Err(e) = tick(&pool, &bot).await {
            warn!(error=%c.redact(&e.to_string()), "notification task");
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

#[derive(sqlx::FromRow)]
struct Recipient {
    id: i64,
    telegram_id: i64,
    notification_time: NaiveTime,
    last_digest_date: Option<NaiveDate>,
    notifications_enabled: bool,
    is_admin: bool,
    last_system_alert_at: Option<DateTime<Utc>>,
    system_alert_interval_seconds: i32,
}

pub async fn tick(pool: &PgPool, bot: &Bot) -> Result<()> {
    let mut lock = pool.begin().await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(76120302)")
        .fetch_one(&mut *lock)
        .await?;
    if !acquired {
        return Ok(());
    }
    sqlx::query("UPDATE system_alerts a SET active=false WHERE a.key LIKE 'classification:%' AND NOT EXISTS(SELECT 1 FROM classification_issues i WHERE a.key='classification:'||i.video_id||':'||i.issue_code AND i.resolved_at IS NULL)").execute(pool).await?;
    sqlx::query("UPDATE system_alerts a SET active=false WHERE a.key LIKE 'media:%' AND NOT EXISTS(SELECT 1 FROM media_errors e WHERE a.key='media:'||e.relative_path AND e.resolved_at IS NULL)").execute(pool).await?;
    let now = Utc::now().with_timezone(&chrono_tz::Europe::Moscow);
    let date = now.date_naive();
    let time = NaiveTime::from_hms_opt(now.hour(), now.minute(), 0).unwrap();
    let recipients: Vec<Recipient> = sqlx::query_as("SELECT id,telegram_id,notification_time,last_digest_date,notifications_enabled,is_admin,last_system_alert_at,system_alert_interval_seconds FROM users WHERE active")
        .fetch_all(pool).await?;
    for r in recipients {
        if r.notifications_enabled
            && time >= r.notification_time
            && r.last_digest_date != Some(date)
            && digest(pool, bot, &r, date).await.is_err()
        {
            // One blocked chat must not prevent notifications to other pilots.
            // Transport errors are not logged here: they can contain the bot token.
            warn!(user_id = r.id, "daily digest failed; will retry");
        }
        if r.is_admin
            && r.last_system_alert_at.is_none_or(|t| {
                Utc::now() - t >= chrono::Duration::seconds(r.system_alert_interval_seconds.into())
            })
            && system(pool, bot, &r).await.is_err()
        {
            warn!(user_id = r.id, "system notification failed; will retry");
        }
    }
    lock.commit().await?;
    Ok(())
}

async fn digest(pool: &PgPool, bot: &Bot, r: &Recipient, date: NaiveDate) -> Result<()> {
    let rows: Vec<(i64, String)> = sqlx::query_as("SELECT v.id,e.name FROM videos v JOIN events e ON e.id=v.event_id JOIN users p ON p.id=v.pilot_id JOIN notification_subscriptions s ON s.pilot_id=v.pilot_id WHERE s.user_id=$1 AND v.state IN ('classified','ready','waiting_for_immich') AND p.active AND (p.id=$1 OR p.is_public OR $2) AND v.first_classified_at >= s.created_at AND NOT EXISTS(SELECT 1 FROM notification_deliveries d WHERE d.user_id=$1 AND d.video_id=v.id)")
        .bind(r.id).bind(r.is_admin).fetch_all(pool).await?;
    if !rows.is_empty() {
        let mut groups = std::collections::BTreeMap::new();
        for (_, event) in &rows {
            *groups.entry(event).or_insert(0) += 1;
        }
        let details = groups
            .iter()
            .take(30)
            .map(|(e, n)| format!("{}: {n}", e.chars().take(70).collect::<String>()))
            .collect::<Vec<_>>()
            .join("\n");
        bot.send_message(
            ChatId(r.telegram_id),
            format!("Новые видео: {}\n{details}", rows.len()),
        )
        .await?;
    }
    let mut tx = pool.begin().await?;
    for (video, _) in rows {
        sqlx::query("INSERT INTO notification_deliveries(user_id,video_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(r.id).bind(video).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE users SET last_digest_date=$2 WHERE id=$1")
        .bind(r.id)
        .bind(date)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn system(pool: &PgPool, bot: &Bot, r: &Recipient) -> Result<()> {
    let alerts: Vec<(String, String)> = sqlx::query_as("SELECT key,message FROM system_alerts a WHERE active AND NOT EXISTS(SELECT 1 FROM system_alert_deliveries d WHERE d.user_id=$1 AND d.alert_key=a.key) ORDER BY created_at LIMIT 20")
        .bind(r.id).fetch_all(pool).await?;
    if alerts.is_empty() {
        return Ok(());
    }
    let details = alerts
        .iter()
        .map(|(_, m)| m.chars().take(160).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    bot.send_message(
        ChatId(r.telegram_id),
        format!("Системные уведомления:\n{details}"),
    )
    .await?;
    let mut tx = pool.begin().await?;
    for (key, _) in alerts {
        sqlx::query("INSERT INTO system_alert_deliveries VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(r.id)
            .bind(key)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE users SET last_system_alert_at=now() WHERE id=$1")
        .bind(r.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
