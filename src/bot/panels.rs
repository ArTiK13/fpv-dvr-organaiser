use super::*;
use teloxide::{types::MessageId, ApiError, RequestError};

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub(super) enum PanelKind {
    Form,
    Search,
    Subscriptions,
    Notifications,
    Selection,
}

#[derive(Clone)]
struct Panel {
    id: MessageId,
}
#[derive(Default)]
struct ChatPanels {
    inline: HashMap<PanelKind, Panel>,
    menu: Option<MessageId>,
}
/// Only messages created as controls enter this registry. Cards, links and files never do.
#[derive(Clone, Default)]
pub(super) struct Panels(Arc<Mutex<HashMap<i64, Arc<Mutex<ChatPanels>>>>>);
impl Panels {
    async fn chat(&self, chat: ChatId) -> Arc<Mutex<ChatPanels>> {
        self.0.lock().await.entry(chat.0).or_default().clone()
    }
}

async fn update(
    bot: &Bot,
    chat: ChatId,
    panels: &mut ChatPanels,
    kind: PanelKind,
    text: String,
    keys: K,
) -> Result<()> {
    if let Some(old) = panels.inline.get(&kind) {
        match bot
            .edit_message_text(chat, old.id, text.clone())
            .reply_markup(keys.clone())
            .await
        {
            Ok(_) | Err(RequestError::Api(ApiError::MessageNotModified)) => {
                panels.inline.insert(kind, Panel { id: old.id });
                return Ok(());
            }
            // A deleted/expired control can be recreated; transport failures must not produce duplicates.
            Err(RequestError::Api(
                ApiError::MessageToEditNotFound | ApiError::MessageCantBeEdited,
            )) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let message = bot
        .send_message(chat, text.clone())
        .reply_markup(keys.clone())
        .await?;
    panels.inline.insert(kind, Panel { id: message.id });
    Ok(())
}

pub(super) async fn show_panel(
    bot: &Bot,
    chat: ChatId,
    s: &BotState,
    kind: PanelKind,
    text: impl Into<String>,
    keys: K,
) -> Result<()> {
    let state = s.panels.chat(chat).await;
    // Serialize Telegram edits for this chat without holding a global/session mutex over HTTP.
    let mut state = state.lock().await;
    update(bot, chat, &mut state, kind, text.into(), keys).await
}

pub(super) async fn finish_panel(
    bot: &Bot,
    chat: ChatId,
    s: &BotState,
    kind: PanelKind,
    text: impl Into<String>,
    keys: K,
) -> Result<()> {
    let state = s.panels.chat(chat).await;
    let mut state = state.lock().await;
    update(bot, chat, &mut state, kind, text.into(), keys).await?;
    // Keep the result in the chat and stop treating it as an editable control.
    state.inline.remove(&kind);
    Ok(())
}

pub(super) async fn remove_input(bot: &Bot, chat: ChatId, id: MessageId, s: &BotState) {
    if let Err(error) = bot.delete_message(chat, id).await {
        // Telegram may refuse old/missing messages. This never rolls back a completed action.
        tracing::debug!(error=%s.config.redact(&error.to_string()), "could not remove dialog input");
    }
}

pub(super) async fn leave_form(bot: &Bot, chat: ChatId, s: &BotState, u: &User) {
    let had_form = s
        .sessions
        .lock()
        .await
        .entry(u.id)
        .or_default()
        .user_form
        .take()
        .is_some();
    if had_form {
        let state = s.panels.chat(chat).await;
        let panel = state.lock().await.inline.remove(&PanelKind::Form);
        if let Some(panel) = panel {
            remove_input(bot, chat, panel.id, s).await;
        }
    }
}

pub(super) async fn reply_menu(
    bot: &Bot,
    chat: ChatId,
    s: &BotState,
    text: &str,
    keys: KeyboardMarkup,
) -> Result<()> {
    let state = s.panels.chat(chat).await;
    let mut state = state.lock().await;
    // Telegram cannot edit a reply keyboard into a message; replace only the tracked menu.
    let new = bot.send_message(chat, text).reply_markup(keys).await?;
    if let Some(old) = state.menu.replace(new.id) {
        remove_input(bot, chat, old, s).await;
    }
    Ok(())
}
