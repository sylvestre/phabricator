//! `PhabricatorEmailPreferencesSettingsPanel`: the mail tags of the
//! installed applications, saved as the `mailtags` setting.

use axum::response::Response;
use phab_app_settings::editor::{self, Change};
use phab_app_settings::mail_tags::{self, CHOICES, EDITORS, Editor};
use phab_app_settings::setting::MAIL_TAGS;
use phab_remarkup::Mode;
use serde_json::{Value, json};

use super::{Content, PanelRequest, html};
use crate::error::PageError;
use crate::state::WebState;

const INSTRUCTIONS: &str = "You can adjust **Application Settings** here to customize when you \
are emailed and notified.

| Setting | Effect
| ------- | -------
| Email | You will receive an email and a notification, but the notification will be marked \
\"read\".
| Notify | You will receive an unread notification only.
| Ignore | You will receive nothing.

If an update makes several changes (like adding CCs to a task, closing it, and adding a \
comment) you will receive the strongest notification any of the changes is configured to \
deliver.

These preferences **only** apply to objects you are connected to (for example, Revisions \
where you are a reviewer or tasks you are CC'd on). To receive email alerts when other \
objects are created, configure [[ /herald/ | Herald Rules ]].";

/// A choice of a tag.
pub(crate) struct ChoiceView {
    pub(super) value: i64,
    pub(super) label: &'static str,
    pub(super) selected: bool,
}

/// A tag and its select.
pub(crate) struct TagView {
    pub(super) key: &'static str,
    pub(super) label: &'static str,
    pub(super) choices: Vec<ChoiceView>,
}

/// The tags of an application.
pub(crate) struct TagGroupView {
    pub(super) label: &'static str,
    pub(super) tags: Vec<TagView>,
}

/// The panel.
pub(crate) struct MailView {
    pub(super) action: String,
    pub(super) instructions: String,
    pub(super) groups: Vec<TagGroupView>,
}

/// The editors of the applications the user may use
/// (`getAllEditorsWithTags()`).
async fn installed_editors<S: WebState>(
    panel: &PanelRequest<'_, S>,
) -> Result<Vec<&'static Editor>, PageError> {
    let classes: Vec<&str> = EDITORS.iter().map(|editor| editor.application).collect();
    let visible = panel
        .viewer
        .can_see_applications(panel.state, &classes)
        .await?;
    Ok(EDITORS
        .iter()
        .zip(visible)
        .filter_map(|(editor, visible)| visible.then_some(editor))
        .collect())
}

/// `GET` shows the tags, a post saves them and goes back to the panel
/// with `?saved=true`.
pub(super) async fn handle<S: WebState>(panel: PanelRequest<'_, S>) -> Result<Response, PageError> {
    let editors = installed_editors(&panel).await?;
    let (preferences, global) = panel.preferences().await?;
    // `isFormPost()`.
    let is_save = panel.request.is_post()
        && panel.request.exists("__form__")
        && !panel.request.exists("__hisec__");
    if is_save {
        panel.require_csrf()?;
        panel.require_writable()?;
        let posted = panel.request.array("mailtags");
        let value = mail_tags::new_value(&editors, preferences.get(MAIL_TAGS), &posted)?;
        let change = Change {
            key: MAIL_TAGS,
            value,
            is_default: false,
        };
        let context = panel.context();
        let viewer = &panel.viewer.user.phid;
        let saved = editor::apply(
            panel.state.database(),
            &context,
            (viewer, viewer),
            vec![change],
            panel.state.now(),
        )
        .await?;
        if saved.is_err() {
            return Err(PageError::Fallback("settings PHP refuses"));
        }
        let uri = format!("{}?saved=true", panel.uri());
        return Ok(panel.request.redirect(panel.state.config(), &uri));
    }

    // `getSettingValue()`: the stored tags, or the global ones.
    let current = match preferences.values.get(MAIL_TAGS) {
        Some(value) => value.clone(),
        None => global
            .as_ref()
            .and_then(|global| global.get(MAIL_TAGS))
            .cloned()
            .unwrap_or_else(|| json!([])),
    };
    if !matches!(current, Value::Array(_) | Value::Object(_)) {
        return Err(PageError::Fallback("mail tags which are not an array"));
    }
    let instructions = crate::remarkup::render(
        panel.state,
        &panel.viewer.user.viewer(),
        INSTRUCTIONS,
        Mode::Html,
    )
    .await?;
    let groups = mail_tags::groups(&editors, &current)
        .into_iter()
        .map(|group| TagGroupView {
            label: group.label,
            tags: group
                .tags
                .into_iter()
                .map(|tag| TagView {
                    key: tag.key,
                    label: tag.label,
                    choices: CHOICES
                        .iter()
                        .map(|(value, label)| ChoiceView {
                            value: *value,
                            label,
                            selected: *value == tag.value,
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();
    let view = MailView {
        action: panel.uri(),
        instructions: html(&instructions),
        groups,
    };
    Ok(panel.render(Content {
        mail: Some(view),
        ..Content::default()
    }))
}
