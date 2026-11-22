//! The panels of `PhabricatorEditEngineSettingsPanel`: a form of the
//! panel's settings, saved through the edit engine
//! (`PhabricatorEditEngine::buildEditResponse()`).

use axum::response::Response;
use phab_app_settings::editor::{self, Change};
use phab_app_settings::preferences::Preferences;
use phab_app_settings::setting::{self, Control, Setting};
use phab_core::php;
use phab_remarkup::Mode;
use serde_json::{Map, Value};

use super::{Content, PanelRequest, html};
use crate::error::PageError;
use crate::state::WebState;

/// `PhabricatorCookies::COOKIE_SUBMIT`, which says the last form saved.
const SUBMIT_COOKIE: &str = "phfrm";
/// `getEditEngineSubmitKey()` of `PhabricatorSettingsEditEngine`.
const SUBMIT_KEY: &str = "edit-engine/settings.settings";

/// An option of a select.
pub(crate) struct OptionView {
    pub(super) value: String,
    pub(super) label: String,
    pub(super) selected: bool,
}

/// Options, in an `<optgroup>` when they have a label.
pub(crate) struct GroupView {
    pub(super) label: Option<String>,
    pub(super) options: Vec<OptionView>,
}

/// A field of the form.
pub(crate) struct FieldView {
    pub(super) key: &'static str,
    pub(super) label: &'static str,
    pub(super) instructions: Option<String>,
    /// The options of a select.
    pub(super) select: Option<Vec<GroupView>>,
    /// The value of a text field.
    pub(super) text: Option<String>,
}

/// The form of a panel.
pub(crate) struct FormView {
    pub(super) title: &'static str,
    pub(super) action: String,
    pub(super) saved: bool,
    pub(super) fields: Vec<FieldView>,
}

/// The settings of the panel; `None` when one of them needs PHP.
fn panel_settings<S: WebState>(panel: &PanelRequest<'_, S>) -> Option<Vec<&'static Setting>> {
    let context = panel.context();
    let settings = setting::for_panel(panel.panel.key);
    settings
        .iter()
        .all(|setting| setting.is_supported(&context))
        .then_some(settings)
}

/// The value the form starts with: `getDefaultValue()` of the
/// preferences, the global setting if there is one.
fn effective_default(
    setting: &Setting,
    context: &setting::Context<'_>,
    global: Option<&Map<String, Value>>,
) -> Value {
    global
        .and_then(|global| global.get(setting.key))
        .cloned()
        .unwrap_or_else(|| setting.default_value(context))
}

/// The value a field reads from the form (`readValueFromSubmit()`):
/// `null` when it is missing, newlines normalized (`getStr()`).
fn submitted<S: WebState>(panel: &PanelRequest<'_, S>, key: &str) -> Value {
    panel.request.str(key).map_or(Value::Null, |value| {
        Value::String(value.replace("\r\n", "\n").replace('\r', "\n"))
    })
}

async fn render<S: WebState>(
    mut panel: PanelRequest<'_, S>,
    settings: &[&'static Setting],
    (preferences, global): (&Preferences, Option<&Map<String, Value>>),
) -> Result<Response, PageError> {
    // `updateEphemeralCookies()`: the cookie is read once, then cleared.
    let saved = panel.request.cookie(SUBMIT_COOKIE) == Some(SUBMIT_KEY);
    if panel.request.nonempty_cookie(SUBMIT_COOKIE).is_some()
        && panel.request.cookie_domain(panel.state.config()).is_some()
    {
        let (config, now) = (panel.state.config(), panel.state.now());
        panel.request.clear_cookie(config, SUBMIT_COOKIE, now);
    }
    let instructions: Vec<&str> = settings
        .iter()
        .map(|setting| setting.instructions.unwrap_or_default())
        .collect();
    let rendered = crate::remarkup::render_all(
        panel.state,
        &panel.viewer.user.viewer(),
        &instructions,
        Mode::Html,
    )
    .await?;
    let context = panel.context();
    let fields = settings
        .iter()
        .zip(rendered)
        .map(|(setting, instructions)| {
            let stored = preferences.get(setting.key);
            let current = php::string_cast(stored);
            let default = effective_default(setting, &context, global);
            let (select, text) = match setting.control(&context, &default) {
                Some(Control::Select(groups)) => (
                    Some(
                        groups
                            .into_iter()
                            .map(|group| GroupView {
                                label: group.label,
                                options: group
                                    .options
                                    .into_iter()
                                    .map(|(value, label)| OptionView {
                                        selected: value == current,
                                        value,
                                        label,
                                    })
                                    .collect(),
                            })
                            .collect(),
                    ),
                    None,
                ),
                Some(Control::Text) => (None, Some(current.into_owned())),
                None => (None, None),
            };
            FieldView {
                key: setting.key,
                label: setting.name,
                instructions: setting.instructions.map(|_| html(&instructions)),
                select,
                text,
            }
        })
        .collect();
    let form = FormView {
        title: panel.panel.name,
        action: panel.uri(),
        saved,
        fields,
    };
    Ok(panel.render(Content {
        form: Some(form),
        ..Content::default()
    }))
}

/// `GET` shows the form, a post of it (`editEngine=true`) saves it and
/// goes back to the panel.
pub(super) async fn handle<S: WebState>(
    mut panel: PanelRequest<'_, S>,
) -> Result<Response, PageError> {
    let settings =
        panel_settings(&panel).ok_or(PageError::Fallback("the translations of developer mode"))?;
    let (preferences, global) = panel.preferences().await?;
    // `isFormOrHisecPost()` and the edit engine's own marker.
    let is_save = panel.request.is_post()
        && panel.request.exists("__form__")
        && panel.request.bool("editEngine");
    if !is_save {
        return render(panel, &settings, (&preferences, global.as_ref())).await;
    }
    panel.require_csrf()?;
    panel.require_writable()?;
    let changes = settings
        .iter()
        .map(|setting| {
            let value = submitted(&panel, setting.key);
            // `setIsDefaultTransaction()`: the value the form started with.
            let is_default =
                phab_app_settings::preferences::identical(&value, preferences.get(setting.key));
            Change {
                key: setting.key,
                value,
                is_default,
            }
        })
        .collect();
    let context = panel.context();
    let viewer = &panel.viewer.user.phid;
    let saved = editor::apply(
        panel.state.database(),
        &context,
        (viewer, viewer),
        changes,
        panel.state.now(),
    )
    .await?;
    if saved.is_err() {
        // PHP shows the form with the errors.
        return Err(PageError::Fallback("settings PHP refuses"));
    }
    let (config, now) = (panel.state.config(), panel.state.now());
    let uri = panel.uri();
    panel
        .request
        .set_temporary_cookie(config, SUBMIT_COOKIE, SUBMIT_KEY, now);
    Ok(panel.request.redirect(config, &uri))
}
