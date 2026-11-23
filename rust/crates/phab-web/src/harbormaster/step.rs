//! `/harbormaster/step/view/<id>/` (`HarbormasterStepViewController`): a
//! build step, its settings and description. Its edits link to PHP. Steps
//! whose settings name a credential (Passphrase isn't ported) and
//! timeline entries other than the creation go to PHP.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_harbormaster::web::{self as harbormaster, Lookup};
use phab_core::php;
use phab_remarkup::Mode;
use serde_json::Value;

use super::{Action, Scope, TransactionTable, method_error};
use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::timeline::Entry;
use crate::viewer::WebUser;
use crate::{remarkup, time};

/// The settings of `HarbormasterHTTPRequestBuildStepImplementation`
/// (`getFieldSpecifications()`), in their order.
const HTTP_FIELDS: &[(&str, &str)] = &[
    ("uri", "URI"),
    ("method", "HTTP Method"),
    ("content_type", "Content-Type header"),
    ("http_body", "HTTP Body"),
];

#[derive(Template)]
#[template(path = "hm_step.html")]
pub(crate) struct StepPage {
    shell: Shell,
    id: i64,
    name: String,
    plan_id: i64,
    /// The implementation's name, or the class of an unknown one.
    step_type: Result<&'static str, String>,
    created: String,
    properties: Vec<(&'static str, String)>,
    description_html: Option<String>,
    timeline: Vec<Entry>,
    actions: Vec<Action>,
}

/// A setting as a property: text as it is, other values as PHP would
/// print them.
fn setting_text(value: &Value) -> Result<Option<String>, PageError> {
    Ok(match value {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some(number.to_string()),
        _ => {
            return Err(PageError::Fallback(
                "a build step setting which is not text",
            ));
        }
    })
}

/// `GET /harbormaster/step/view/<id>/`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<StepPage>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    let scope = Scope::new(state.as_ref(), &viewer);
    let request = scope.request();
    let step = match harbormaster::step(&request, id)
        .await
        .map_err(method_error)?
    {
        Lookup::Visible(step) => step,
        Lookup::Missing | Lookup::Denied => return Err(PageError::NotFound),
    };
    let plan = harbormaster::plans_by_phid(&request, std::slice::from_ref(&step.plan_phid))
        .await
        .map_err(method_error)?
        .remove(&step.plan_phid)
        .ok_or(PageError::NotFound)?;
    let can_edit = harbormaster::can_edit_plans(&request, std::slice::from_ref(&plan))
        .await
        .map_err(method_error)?
        .first()
        .copied()
        .unwrap_or(false);

    let mut properties = Vec::new();
    if step.class_name == "HarbormasterHTTPRequestBuildStepImplementation" {
        if step
            .details
            .get("credential")
            .is_some_and(|credential| credential.as_str().is_some_and(php::is_truthy_str))
        {
            return Err(PageError::Fallback("a build step with a credential"));
        }
        for (key, label) in HTTP_FIELDS {
            if let Some(value) = step.details.get(*key)
                && let Some(text) = setting_text(value)?
            {
                properties.push((*label, text));
            }
        }
    }
    let description_html = if step.description.is_empty() {
        None
    } else {
        let markup = remarkup::render(
            state.as_ref(),
            &viewer.user.viewer(),
            &step.description,
            Mode::Html,
        )
        .await?;
        Some(String::from_utf8_lossy(markup.as_bytes()).into_owned())
    };
    let timeline = super::timeline(
        state.as_ref(),
        &scope,
        TransactionTable::Step,
        &step.phid,
        |transaction, author| {
            let kind = transaction.transaction_type.as_str();
            if kind == "harbormaster:step:create" {
                return Ok(Some(format!("{author} created this build step.")));
            }
            // What the creation set is hidden with it.
            if transaction.metadata_flag("core.create") {
                return Ok(None);
            }
            Err(PageError::Fallback("a build step transaction not ported"))
        },
    )
    .await?;
    let actions = vec![
        Action {
            name: "Edit Step",
            icon: "fa-pencil",
            uri: format!("/harbormaster/step/edit/{id}/"),
            enabled: can_edit,
        },
        Action {
            name: "Delete Step",
            icon: "fa-times",
            uri: format!("/harbormaster/step/delete/{id}/"),
            enabled: can_edit,
        },
    ];
    let name = harbormaster::step_name(&step).unwrap_or_default();
    let title = format!("Step {id}");
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(StepPage {
        shell: Shell::for_user(state.config(), &viewer, &title, current),
        id,
        name,
        plan_id: plan.id,
        step_type: harbormaster::implementation_name(&step.class_name)
            .ok_or_else(|| step.class_name.clone()),
        created: time::format_utc(step.dates.0),
        properties,
        description_html,
        timeline,
        actions,
    }))
}
