use super::*;

const PAGE_SIZE: usize = 12;

pub(super) fn choices_mut<'a>(f: &'a mut Filters, kind: &str) -> Result<&'a mut Vec<i64>> {
    match kind {
        "device" => Ok(&mut f.device),
        "system" => Ok(&mut f.system),
        "pilot" => Ok(&mut f.pilot),
        _ => bail!("Неизвестный фильтр"),
    }
}

fn directory(kind: &str) -> Result<(&'static str, &'static str, &'static str)> {
    match kind {
        "device" => Ok(("Аппараты", "devices", "name")),
        "system" => Ok(("Видеосистемы", "video_systems", "name")),
        "pilot" => Ok((
            "Пилоты",
            "users",
            "coalesce(nickname,full_name,telegram_username,'Пилот')",
        )),
        _ => bail!("Неизвестный фильтр"),
    }
}

// Identifiers and visibility clauses come only from these fixed helpers.
fn visibility(kind: &str) -> &'static str {
    if kind == "pilot" {
        "AND (id=$1 OR is_public OR $2)"
    } else {
        "AND ($1::bigint IS NOT NULL OR $2)"
    }
}

pub(super) async fn toggle_choice(s: &BotState, u: &User, kind: &str, id: i64) -> Result<()> {
    let (_, table, _) = directory(kind)?;
    let query = format!(
        "SELECT id FROM {table} WHERE active {} AND id=$3",
        visibility(kind)
    );
    let allowed: Option<i64> = sqlx::query_scalar(&query)
        .bind(u.id)
        .bind(u.is_admin)
        .bind(id)
        .fetch_optional(&s.pool)
        .await?;
    if allowed.is_none() {
        bail!("Этот вариант больше недоступен. Откройте фильтр заново.");
    }
    let mut sessions = s.sessions.lock().await;
    let f = &mut sessions.entry(u.id).or_default().filters;
    let ids = choices_mut(f, kind)?;
    if ids.contains(&id) {
        ids.retain(|v| *v != id);
    } else {
        ids.push(id);
    }
    f.page = 0;
    Ok(())
}

pub(super) async fn show_picker(
    bot: &Bot,
    chat: ChatId,
    s: &BotState,
    u: &User,
    kind: &str,
    page: u32,
) -> Result<()> {
    panels::leave_form(bot, chat, s, u).await;
    let (title, table, name) = directory(kind)?;
    let visibility = visibility(kind);
    let count: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM {table} WHERE active {visibility}"
    ))
    .bind(u.id)
    .bind(u.is_admin)
    .fetch_one(&s.pool)
    .await?;
    let last_page = (count.saturating_sub(1).max(0) / PAGE_SIZE as i64) as u32;
    let page = page.min(last_page);
    let rows: Vec<(i64, String)> = sqlx::query_as(&format!(
        "SELECT id,{name} FROM {table} WHERE active {visibility} ORDER BY id LIMIT $3 OFFSET $4"
    ))
    .bind(u.id)
    .bind(u.is_admin)
    .bind(PAGE_SIZE as i64)
    .bind(i64::from(page) * PAGE_SIZE as i64)
    .fetch_all(&s.pool)
    .await?;
    let mut f = s
        .sessions
        .lock()
        .await
        .entry(u.id)
        .or_default()
        .filters
        .clone();
    let ids = choices_mut(&mut f, kind)?;
    let mut keys: Vec<Vec<B>> = rows
        .into_iter()
        .map(|(id, name)| {
            vec![B::callback(
                format!("{} {name}", if ids.contains(&id) { "✅" } else { "⬜️" }),
                format!("set:{kind}:{id}:{page}"),
            )]
        })
        .collect();
    let mut navigation = vec![];
    if page > 0 {
        navigation.push(B::callback("←", format!("pickpage:{kind}:{}", page - 1)));
    }
    if page < last_page {
        navigation.push(B::callback("→", format!("pickpage:{kind}:{}", page + 1)));
    }
    if !navigation.is_empty() {
        keys.push(navigation);
    }
    keys.push(vec![B::callback(
        "↺ Любые — сбросить этот фильтр",
        format!("clear:{kind}"),
    )]);
    keys.push(vec![B::callback("✓ Готово · К фильтрам", "filters")]);
    keys.push(vec![B::callback("🔎 Показать видео", "search")]);
    let selected = if ids.is_empty() {
        "Подходят любые варианты.".into()
    } else {
        format!(
            "Выбрано: {}. {}",
            ids.len(),
            selected_names(&s.pool, u, kind, ids).await?.join(", ")
        )
    };
    show_panel(bot, chat, s, PanelKind::Search,
        format!("{title} · страница {} из {}\n\n{selected}\n\nОтметьте один или несколько вариантов. Повторное нажатие снимает отметку. Если ничего не выбрано, фильтр не ограничивает поиск.", page + 1, last_page + 1), K::new(keys)).await?;
    Ok(())
}

pub(super) async fn selected_names(
    pool: &PgPool,
    u: &User,
    kind: &str,
    ids: &[i64],
) -> Result<Vec<String>> {
    let (_, table, name) = directory(kind)?;
    let query = if kind == "pilot" {
        format!("SELECT {name} FROM {table} WHERE id=ANY($1) AND active AND (id=$2 OR is_public OR $3) ORDER BY id")
    } else {
        format!("SELECT {name} FROM {table} WHERE id=ANY($1) AND ($2::bigint IS NOT NULL OR $3) ORDER BY id")
    };
    let names: Vec<String> = sqlx::query_scalar(&query)
        .bind(ids)
        .bind(u.id)
        .bind(u.is_admin)
        .fetch_all(pool)
        .await?;
    // Keep Telegram's text limit even with a large directory or long names.
    let mut shown: Vec<_> = names
        .iter()
        .take(8)
        .map(|n| n.chars().take(50).collect::<String>())
        .collect();
    if names.len() > shown.len() {
        shown.push(format!("и ещё {}", names.len() - shown.len()));
    }
    if names.len() < ids.len() {
        shown.push(format!("недоступно: {}", ids.len() - names.len()));
    }
    Ok(shown)
}
