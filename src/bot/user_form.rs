use crate::{
    db::UserInput,
    presentation::{invalid, names},
};
use anyhow::Result;
use teloxide::types::{InlineKeyboardButton as B, InlineKeyboardMarkup as K};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Step {
    Telegram,
    FullName,
    Nickname,
    Username,
    Aliases,
    Role,
    Visibility,
    Review,
}
impl Step {
    fn previous(self) -> Self {
        match self {
            Self::Telegram | Self::FullName => Self::Telegram,
            Self::Nickname => Self::FullName,
            Self::Username => Self::Nickname,
            Self::Aliases => Self::Username,
            Self::Role => Self::Aliases,
            Self::Visibility => Self::Role,
            Self::Review => Self::Visibility,
        }
    }
    fn next(self) -> Self {
        match self {
            Self::Telegram => Self::FullName,
            Self::FullName => Self::Nickname,
            Self::Nickname => Self::Username,
            Self::Username => Self::Aliases,
            Self::Aliases => Self::Role,
            Self::Role => Self::Visibility,
            Self::Visibility | Self::Review => Self::Review,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct UserForm {
    pub token: Uuid,
    pub input: UserInput,
    pub step: Step,
    editing: bool,
}
impl UserForm {
    pub fn new() -> Self {
        Self {
            token: Uuid::new_v4(),
            step: Step::Telegram,
            editing: false,
            input: UserInput {
                id: None,
                telegram_id: 0,
                full_name: None,
                nickname: None,
                telegram_username: None,
                aliases: vec![],
                is_admin: false,
                is_public: true,
            },
        }
    }
    pub fn prefilled(input: UserInput) -> Self {
        Self {
            input,
            step: Step::Review,
            ..Self::new()
        }
    }
    pub fn summary(&self) -> String {
        let v = &self.input;
        format!(
            "Telegram ID: {}\nФИО: {}\nНик: {}\nTelegram: {}\nИмена папок: {}\nРоль: {}\nВидео: {}",
            v.telegram_id,
            v.full_name.as_deref().unwrap_or("не указано"),
            v.nickname.as_deref().unwrap_or("не указан"),
            v.telegram_username
                .as_ref()
                .map(|s| format!("@{s}"))
                .unwrap_or("не указан".into()),
            if v.aliases.is_empty() {
                "поиск по заполненным именам и нику".into()
            } else {
                names(&v.aliases)
            },
            if v.is_admin {
                "администратор"
            } else {
                "пилот"
            },
            if v.is_public {
                "публичные — доступны другим пилотам"
            } else {
                "приватные — доступны владельцу и администраторам"
            }
        )
    }
    pub fn view(&self) -> (String, K) {
        let cb =
            |label: &str, action: &str| B::callback(label, format!("uf:{}:{action}", self.token));
        let mut rows = vec![];
        let prompt = match self.step {
            Step::Telegram => "Шаг 1 из 7. Отправьте числовой Telegram ID пользователя.\nПример: 1004106925. Username вместо ID не подойдёт.",
            Step::FullName => "Шаг 2 из 7. Как зовут пользователя? Отправьте ФИО или имя. Поле можно пропустить.",
            Step::Nickname => "Шаг 3 из 7. Отправьте ник пилота. Поле можно пропустить.",
            Step::Username => "Шаг 4 из 7. Отправьте ник в Telegram, например @pilot. Поле можно пропустить.",
            Step::Aliases => "Шаг 5 из 7. Как подписаны папки пилота? Отправьте одно или несколько имён через запятую.\nЕсли пропустить, будут использоваться ФИО и заполненные ники.",
            Step::Role => { rows.push(vec![cb("👤 Пилот", "pilot"), cb("⚙️ Администратор", "admin")]); "Шаг 6 из 7. Выберите роль. Администратор может менять каталог и видеть все видео." },
            Step::Visibility => { rows.push(vec![cb("🌐 Публичные", "public"), cb("🔒 Приватные", "private")]); "Шаг 7 из 7. Кто может видеть видео этого пилота? Администраторы имеют доступ в обоих случаях." },
            Step::Review => {
                rows.push(vec![cb("✅ Сохранить", "save")]);
                rows.push(vec![cb("Изменить ID", "edit_id"), cb("ФИО", "edit_name"), cb("Ник", "edit_nick")]);
                rows.push(vec![cb("Telegram", "edit_username"), cb("Имена папок", "edit_aliases")]);
                rows.push(vec![cb("Роль", "edit_role"), cb("Доступ к видео", "edit_visibility")]);
                "Проверьте данные перед сохранением."
            }
        };
        if matches!(
            self.step,
            Step::FullName | Step::Nickname | Step::Username | Step::Aliases
        ) {
            rows.push(vec![cb("Пропустить / очистить", "skip")]);
        }
        let mut navigation = vec![];
        if self.step != Step::Telegram {
            navigation.push(cb("← Назад", "back"));
        }
        navigation.push(cb("Отмена", "cancel"));
        rows.push(navigation);
        let text = if self.step == Step::Review {
            format!(
                "👤 {}\n\n{}\n\n{prompt}",
                if self.input.id.is_some() {
                    "Редактирование пользователя"
                } else {
                    "Добавление пользователя"
                },
                self.summary()
            )
        } else {
            format!(
                "👤 {}\n\n{prompt}",
                if self.input.id.is_some() {
                    "Изменение пользователя"
                } else {
                    "Добавление пользователя"
                }
            )
        };
        (text, K::new(rows))
    }
    pub fn text(&mut self, text: &str) -> Result<()> {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > 200 {
            return Err(invalid("Введите от 1 до 200 символов."));
        }
        match self.step {
            Step::Telegram => {
                self.input.telegram_id = parse_id(text)?;
            }
            Step::FullName => self.input.full_name = Some(text.into()),
            Step::Nickname => self.input.nickname = Some(text.into()),
            Step::Username => {
                let name = text.trim_start_matches('@');
                if name.len() < 5
                    || name.len() > 32
                    || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    return Err(invalid("Ник Telegram должен содержать 5–32 латинских символа, цифры или _. Можно указать @ в начале или пропустить поле."));
                }
                self.input.telegram_username = Some(name.into());
            }
            Step::Aliases => {
                let aliases = text
                    .split([',', '\n'])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect::<Vec<_>>();
                if aliases.is_empty()
                    || aliases.len() > 20
                    || aliases.iter().any(|s| s.contains(['/', '\\']))
                {
                    return Err(invalid(
                        "Отправьте до 20 имён папок через запятую, без / и \\.",
                    ));
                }
                self.input.aliases = aliases;
            }
            _ => {
                return Err(invalid(
                    "На этом шаге выберите действие кнопкой под последним сообщением.",
                ))
            }
        }
        self.advance();
        Ok(())
    }
    pub fn action(&mut self, action: &str) -> Result<()> {
        match action {
            "back" => {
                self.editing = false;
                self.step = self.step.previous();
            }
            "skip" => {
                match self.step {
                    Step::FullName => self.input.full_name = None,
                    Step::Nickname => self.input.nickname = None,
                    Step::Username => self.input.telegram_username = None,
                    Step::Aliases => self.input.aliases.clear(),
                    _ => return Err(invalid("Это поле нельзя пропустить.")),
                };
                self.advance();
            }
            "pilot" | "admin" if self.step == Step::Role => {
                self.input.is_admin = action == "admin";
                self.advance();
            }
            "public" | "private" if self.step == Step::Visibility => {
                self.input.is_public = action == "public";
                self.advance();
            }
            value if self.step == Step::Review && value.starts_with("edit_") => {
                self.editing = true;
                self.step = match value {
                    "edit_id" => Step::Telegram,
                    "edit_name" => Step::FullName,
                    "edit_nick" => Step::Nickname,
                    "edit_username" => Step::Username,
                    "edit_aliases" => Step::Aliases,
                    "edit_role" => Step::Role,
                    "edit_visibility" => Step::Visibility,
                    _ => return Err(invalid("Неизвестное поле.")),
                };
            }
            _ => {
                return Err(invalid(
                    "Эта кнопка больше не актуальна. Используйте кнопки под последним сообщением.",
                ))
            }
        }
        self.token = Uuid::new_v4();
        Ok(())
    }
    fn advance(&mut self) {
        self.step = if self.editing {
            self.editing = false;
            Step::Review
        } else {
            self.step.next()
        };
        self.token = Uuid::new_v4();
    }
    pub fn validate(&self) -> Result<()> {
        if self.step != Step::Review {
            return Err(invalid("Сначала заполните данные пользователя."));
        }
        if self.input.telegram_id <= 0 {
            return Err(invalid("Укажите числовой Telegram ID."));
        }
        if self.input.aliases.is_empty()
            && [
                &self.input.full_name,
                &self.input.nickname,
                &self.input.telegram_username,
            ]
            .iter()
            .all(|v| v.is_none())
        {
            return Err(invalid("Укажите хотя бы ФИО, ник, ник Telegram или имя папки. Нажмите нужное поле под сообщением."));
        }
        Ok(())
    }
}
pub(super) fn parse_id(value: &str) -> Result<i64> {
    value.trim().parse::<i64>().ok().filter(|n| *n > 0 && *n <= 4_503_599_627_370_495)
        .ok_or_else(|| invalid("Нужен положительный числовой Telegram ID, например 1004106925. Отправьте только цифры."))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wizard_handles_optional_fields_back_and_explicit_privacy() {
        let mut f = UserForm::new();
        assert!(f.text("@pilot").is_err());
        assert_eq!(f.step, Step::Telegram);
        f.text("1004106925").unwrap();
        f.action("skip").unwrap();
        f.text("Kostyan").unwrap();
        assert!(f.text("неправильный ник").is_err());
        f.text("@kostyanbtw").unwrap();
        f.text("kostyanbtw, kostyan").unwrap();
        f.action("pilot").unwrap();
        f.action("private").unwrap();
        f.validate().unwrap();
        assert!(!f.input.is_admin && !f.input.is_public);
        assert_eq!(f.input.telegram_username.as_deref(), Some("kostyanbtw"));
        let previous = f.token;
        f.action("edit_aliases").unwrap();
        assert_ne!(previous, f.token);
        f.action("back").unwrap();
        assert_eq!(f.step, Step::Username);
    }
    #[test]
    fn id_alone_cannot_be_saved_and_premature_buttons_are_rejected() {
        let mut f = UserForm::new();
        assert!(f.action("admin").is_err());
        f.text("12345").unwrap();
        for _ in 0..4 {
            f.action("skip").unwrap();
        }
        f.action("pilot").unwrap();
        f.action("public").unwrap();
        assert!(f.validate().is_err());
        f.action("edit_aliases").unwrap();
        f.text("pilot_folder").unwrap();
        assert_eq!(f.step, Step::Review);
        assert!(f.validate().is_ok());
    }
}
