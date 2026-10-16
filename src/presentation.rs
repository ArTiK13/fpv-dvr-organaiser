//! Plain-text Telegram presentation. Internal state and database errors stay out of messages.
use anyhow::Error;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InputError(pub String);

pub fn invalid(message: impl Into<String>) -> Error {
    InputError(message.into()).into()
}

pub fn error_message(error: &Error) -> String {
    if let Some(e) = error.downcast_ref::<InputError>() {
        return e.0.clone();
    }
    if let Some(sqlx::Error::Database(e)) = error.downcast_ref::<sqlx::Error>() {
        if e.code().as_deref() == Some("23505") {
            return "Такая запись уже существует. Откройте список и измените существующую запись."
                .into();
        }
    }
    if error.downcast_ref::<serde_json::Error>().is_some() {
        return "Не удалось прочитать параметры команды. Для добавления пользователя нажмите «Добавить пользователя» — бот задаст вопросы по очереди.".into();
    }
    // Existing domain errors use Russian text; transport and database errors may include secrets.
    if error.chain().count() == 1
        && error
            .to_string()
            .starts_with(|c: char| ('А'..='я').contains(&c))
    {
        return error.to_string().chars().take(500).collect();
    }
    "Не удалось выполнить действие. Попробуйте ещё раз немного позже. Если ошибка повторяется, сообщите администратору.".into()
}

pub fn state(value: &str) -> &str {
    match value {
        "discovered" => "найдены новые файлы",
        "metadata_pending" => "читаются параметры видео",
        "classified" => "доступны в каталоге",
        "unclassified" => "нужно уточнить данные",
        "waiting_for_immich" => "ожидание обработки в Immich",
        "ready" => "готовы к выдаче",
        "missing" => "файл отсутствует",
        "conflict" => "файл изменился после проверки",
        "pending" => "в очереди",
        "running" => "отправляется",
        "done" => "завершено",
        "cancelled" => "отменено",
        "failed" => "не удалось завершить",
        _ => "состояние уточняется",
    }
}

pub fn issue(value: &str) -> &str {
    match value {
        "unknown_device" => "не распознан аппарат",
        "unknown_video_system" => "не распознана видеосистема",
        "file_changed" => "исходный файл изменился после проверки",
        "inactive_default_user" => "пилот по умолчанию отключён",
        "unknown_pilot" => "не распознан пилот",
        "missing_default_user" => "не задан пилот для папок без имени",
        "ambiguous_device_or_video_system"
        | "ambiguous"
        | "ambiguous_device"
        | "ambiguous_video_system"
        | "ambiguous_pilot" => "найдено несколько совпадений — уточните имена папок",
        "invalid_path_depth" | "invalid_structure" | "structure" => "неверная структура папок",
        _ => "не удалось определить данные видео",
    }
}

pub fn device_type(value: &str) -> &str {
    match value {
        "plane" => "самолёт",
        "quad" => "квадрокоптер",
        _ => "другой аппарат",
    }
}

pub fn delivery_mode(value: &str) -> &str {
    match value {
        "video" => "видео в Telegram",
        "document" => "файлы в Telegram",
        _ => "ссылка Immich",
    }
}

pub fn names(values: &[String]) -> String {
    if values.is_empty() {
        "не заданы".into()
    } else {
        values.join(", ")
    }
}

pub fn size(bytes: i64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} ГБ", bytes as f64 / 1e9)
    } else {
        format!("{:.1} МБ", bytes as f64 / 1e6)
    }
}

pub fn alert(key: &str, message: &str) -> String {
    if let Some(rest) = key.strip_prefix("classification:") {
        if let Some((id, code)) = rest.split_once(':') {
            return format!("Видео №{id}: {}", issue(code));
        }
    }
    if let Some(path) = key.strip_prefix("media:") {
        return format!("Не удалось прочитать параметры файла: {path}");
    }
    if key.starts_with("delivery:") {
        return "Не удалось завершить выдачу видео. Проверьте доступность Immich и состояние запросов.".into();
    }
    if key == "scanner" || key.starts_with("scan:") {
        return "Не удалось обновить каталог. Проверьте доступ к папке видео.".into();
    }
    message.chars().take(400).collect()
}
