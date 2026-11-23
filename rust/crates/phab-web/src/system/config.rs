//! `/config/`, for administrators, read-only: the console
//! (`PhabricatorConfigConsoleController`), the settings lists
//! (`PhabricatorConfigSettingsListController`) and an option's page
//! (`PhabricatorConfigEditController` without its form). Saving values is
//! PHP's form, or `phab-cli config set`.
//!
//! Setup issues (`/config/issue/`) are the results of PHP's setup checks,
//! which examine PHP's own environment, so they stay with PHP.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_config::catalog::ConfigOption;
use phab_app_config::entry::{self, ConfigEntry};
use phab_app_config::json;
use phab_app_config::local::LocalSource;
use serde_json::Value;

use super::{Body, Item, Link, List, PageBuilder, Property, Section, SystemPage, no_query};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorConfigApplication`.
pub(crate) const APPLICATION: &str = "PhabricatorConfigApplication";

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> PageError {
    PageError::Internal(Box::new(error))
}

/// `GET /config/`.
pub(crate) async fn console<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    super::require_admin(&*state, &viewer, APPLICATION).await?;
    no_query(&uri)?;
    let item = |name: &str, uri: &str, icon: &str, description: &str| Item {
        name: name.to_owned(),
        uri: Some(uri.to_owned()),
        icon: Some(icon.to_owned()),
        attributes_html: vec![escape(description)],
        ..Item::default()
    };
    let menu = vec![
        item(
            "Settings",
            "/config/settings/",
            "fa-wrench",
            "Review and modify configuration settings.",
        ),
        item(
            "Setup Issues",
            "/config/issue/",
            "fa-exclamation-triangle",
            "Show unresolved issues with setup and configuration.",
        ),
        item(
            "Services",
            "/config/cluster/databases/",
            "fa-server",
            "View status information for databases, caches, repositories, and other services.",
        ),
        item(
            "Extensions/Modules",
            "/config/module/",
            "fa-gear",
            "Show installed extensions and modules.",
        ),
    ];
    Ok(PageBuilder::new("Configuration")
        .icon("fa-sliders")
        .section(Section::untitled(Body::List(List {
            items: menu,
            empty: String::new(),
        })))
        .section(Section::new(
            "Version Information",
            Body::Notice(
                "The versions of the libraries come from their Git working copies, which only \
                 PHP reads: open this page in the old interface to see them."
                    .to_owned(),
            ),
        ))
        .build(state.as_ref(), &viewer, &uri))
}

/// The settings navigation (`PhabricatorConfigSettingsController`).
fn settings_navigation(current: &str) -> Vec<Link> {
    vec![
        Link::new("Core Settings", "/config/settings/")
            .icon("fa-wrench")
            .inactive(current == "settings"),
        Link::new("Advanced Settings", "/config/settings/advanced/")
            .icon("fa-cogs")
            .inactive(current == "advanced"),
        Link::new("All Settings", "/config/settings/all/")
            .icon("fa-list")
            .inactive(current == "all"),
        Link::new("View History", "/config/settings/history/").icon("fa-history"),
    ]
}

/// `GET /config/settings/`.
pub(crate) async fn settings<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    settings_list(state.as_ref(), &viewer, "settings", &uri).await
}

/// `GET /config/settings/<advanced|all>/`.
pub(crate) async fn settings_filter<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(filter): Path<String>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    let filter = match filter.as_str() {
        "advanced" => "advanced",
        "all" => "all",
        // `PhabricatorConfigSettingsHistoryController`.
        "history" => return Err(PageError::Fallback("the configuration history")),
        _ => return Err(PageError::NotFound),
    };
    settings_list(state.as_ref(), &viewer, filter, &uri).await
}

async fn settings_list<S: WebState>(
    state: &S,
    viewer: &WebUser,
    filter: &'static str,
    uri: &Uri,
) -> Result<Page<SystemPage>, PageError> {
    super::require_admin(state, viewer, APPLICATION).await?;
    no_query(uri)?;
    let (show_core, show_advanced, title) = match filter {
        "settings" => (true, false, "Core Settings"),
        "advanced" => (false, true, "Advanced Settings"),
        _ => (true, true, "All Settings"),
    };
    let entries: HashMap<String, ConfigEntry> = entry::load_all(state.database())
        .await?
        .into_iter()
        .map(|entry| (entry.key.clone(), entry))
        .collect();
    let config = state.config();
    let mut items = Vec::new();
    for option in phab_app_config::options() {
        let advanced = option.is_locked(config);
        if (advanced && !show_advanced) || (!advanced && !show_core) {
            continue;
        }
        let stored = entries
            .get(&option.key)
            .is_some_and(|entry| !entry.is_deleted);
        let (icon, status) = if option.is_hidden(config) {
            ("fa-eye-slash", "Hidden")
        } else if advanced {
            (
                if stored {
                    "fa-lock remarkup-icon-indigo"
                } else {
                    "fa-lock"
                },
                "Locked",
            )
        } else if stored {
            ("fa-pencil remarkup-icon-indigo", "Editable")
        } else {
            ("fa-circle-o", "Default")
        };
        items.push(Item {
            name: option.key.clone(),
            uri: Some(format!("/config/edit/{}/", option.key)),
            icon: Some(icon.to_owned()),
            tags: vec![status.to_owned()],
            attributes_html: vec![escape(option.summary.as_deref().unwrap_or(""))],
            disabled: false,
        });
    }
    Ok(PageBuilder::new(title)
        .icon("fa-sliders")
        .nav(settings_navigation(filter))
        .section(Section::untitled(Body::List(List {
            items,
            empty: "No settings.".to_owned(),
        })))
        .build(state, viewer, uri))
}

/// `PhabricatorConfigType::newDisplayValue()` of the option's type.
fn display_value(option: &ConfigOption, value: &Value) -> Result<String, PageError> {
    Ok(match option.option_type.as_str() {
        "bool" => if phab_core::php::is_truthy(value) {
            "true"
        } else {
            "false"
        }
        .to_owned(),
        "string" | "int" | "enum" | "class" => phab_core::php::string_cast(value).into_owned(),
        "list<string>" | "list<regex>" => match value {
            Value::Array(list) => list
                .iter()
                .map(|item| phab_core::php::string_cast(item).into_owned())
                .collect::<Vec<_>>()
                .join("\n"),
            Value::Object(map) => map
                .values()
                .map(|item| phab_core::php::string_cast(item).into_owned())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => {
                return Err(PageError::Fallback(
                    "a configuration value of the wrong type",
                ));
            }
        },
        "set" => match value {
            Value::Object(map) => map.keys().cloned().collect::<Vec<_>>().join("\n"),
            Value::Array(list) if list.is_empty() => String::new(),
            Value::Array(list) => (0..list.len())
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => {
                return Err(PageError::Fallback(
                    "a configuration value of the wrong type",
                ));
            }
        },
        kind if kind.starts_with("custom:") => {
            return Err(PageError::Fallback("a custom configuration type"));
        }
        // `PhabricatorJSONConfigType`: `PhabricatorConfigJSON::prettyPrintJSON()`.
        _ => pretty_json(value),
    })
}

/// `PhabricatorConfigJSON::prettyPrintJSON()`.
fn pretty_json(value: &Value) -> String {
    let round_tripped = phab_core::php::round_trip(value);
    let text = match &round_tripped {
        Value::Array(list) if !list.is_empty() => json::encode_list_formatted(list),
        Value::Array(_) => json::encode_formatted(&serde_json::Map::new()),
        Value::Object(map) => json::encode_formatted(map),
        scalar => phab_core::php::json_encode(scalar),
    };
    text.replace("\\/", "/")
}

/// The environment file PHP's stack reads after the defaults
/// (`conf/local/ENVIRONMENT`), if any.
fn environment_name() -> Option<String> {
    let directory = std::env::var_os("PHAB_CONFIG_DIR").map_or_else(
        || PathBuf::from("/app/phabricator/conf/local"),
        PathBuf::from,
    );
    std::fs::read_to_string(directory.join("ENVIRONMENT"))
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// The keys `conf/custom/moz-extensions.conf.php` sets.
const MOZ_EXTENSIONS_KEYS: [&str; 2] = ["load-libraries", "events.listeners"];

/// Whether the config entry has transactions, which the page's timeline
/// shows.
async fn has_transactions<S: WebState>(state: &S, phid: &str) -> Result<bool, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT 1 FROM {} WHERE objectPHID = ? LIMIT 1",
        database.table("config", "config_transaction")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .is_some())
}

/// A value of the "Current Configuration" table, as HTML.
fn value_cell(option: &ConfigOption, value: Option<&Value>) -> Result<String, PageError> {
    Ok(match value {
        None => "<em>(No Value Configured)</em>".to_owned(),
        Some(value) => format!("<pre>{}</pre>", escape(&display_value(option, value)?)),
    })
}

/// `GET /config/edit/<key>/`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn option<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    let state = state.as_ref();
    super::require_admin(state, &viewer, APPLICATION).await?;
    no_query(&uri)?;
    let Some(option) = phab_app_config::option(&key) else {
        // Unknown and removed options get an editable "wild" page.
        return Err(PageError::Fallback("an unknown configuration option"));
    };
    let config = state.config();
    let environment = environment_name();
    match environment.as_deref() {
        None => {}
        Some("custom/moz-extensions") if !MOZ_EXTENSIONS_KEYS.contains(&key.as_str()) => {}
        Some(_) => return Err(PageError::Fallback("an option an environment file may set")),
    }
    let entry = entry::load(state.database(), &key).await?;
    if let Some(entry) = &entry
        && has_transactions(state, entry.phid.as_str()).await?
    {
        return Err(PageError::Fallback("the history of a configuration entry"));
    }
    let stored = entry.as_ref().filter(|entry| !entry.is_deleted);

    let hidden = option.is_hidden(config);
    let locked = option.is_locked(config);
    let mut page = PageBuilder::new(&key).icon("fa-sliders");
    if hidden {
        page = page.tag("Hidden").section(Section::untitled(Body::Notice(
            "This configuration is hidden and can not be edited or viewed from the web \
             interface."
                .to_owned(),
        )));
    } else if locked {
        let message = option.locked_message_html();
        page = page
            .tag("Locked")
            .section(Section::untitled(Body::Html(format!(
                "<p class=\"notice\">{message} <a href=\"https://secure.phabricator.com/diviner/\
             find/?name=Configuration%20Guide%3A%20Locked%20and%20Hidden%20Configuration&amp;\
             type=article&amp;jump=1\" target=\"_blank\">Learn more about locked and hidden \
             options.</a></p>"
            ))));
    }
    let mut properties = Vec::new();
    if let Some(description) = option
        .description
        .as_deref()
        .filter(|description| !description.is_empty())
    {
        properties.push(Property::html(
            "Description",
            super::remarkup_html(state, &viewer, description).await?,
        ));
    }
    if !hidden && !locked {
        let value = match stored {
            Some(entry) => format!(
                "<pre>{}</pre>",
                escape(&display_value(option, &entry.value)?)
            ),
            None => "<em>(Use Default)</em>".to_owned(),
        };
        properties.push(Property::html("Database Value", value));
    }
    page = page.section(Section::new(key.clone(), Body::Properties(properties)));

    if !option.examples.is_empty() {
        let mut rows = Vec::new();
        for example in &option.examples {
            let value = match &example.value {
                Value::Null => "<em>(empty)</em>".to_owned(),
                Value::Array(list) => format!(
                    "<pre>{}</pre>",
                    escape(
                        &list
                            .iter()
                            .map(|item| phab_core::php::string_cast(item).into_owned())
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                ),
                other => format!("<pre>{}</pre>", escape(&phab_core::php::string_cast(other))),
            };
            rows.push(vec![
                escape(example.description.as_deref().unwrap_or("")),
                value,
            ]);
        }
        page = page.section(Section::new(
            "Examples",
            Body::Table(super::Table {
                headers: vec!["Example", "Value"],
                rows,
                empty: String::new(),
            }),
        ));
    }

    if !hidden {
        // `renderDefaults()`: the stack, top first.
        let local = LocalSource::load(&local_json_path()).map_err(internal)?;
        let mut sources: Vec<(String, Option<Value>)> = vec![
            (
                "Database".to_owned(),
                stored.map(|entry| entry.value.clone()),
            ),
            ("Local Config".to_owned(), local.get(&key).cloned()),
        ];
        if let Some(environment) = environment {
            sources.push((format!("File '{environment}'"), None));
        }
        sources.push(("Global Default".to_owned(), Some(option.default.clone())));
        let mut rows = Vec::new();
        for (name, value) in &sources {
            rows.push(vec![escape(name), value_cell(option, value.as_ref())?]);
        }
        page = page.section(Section::new(
            "Current Configuration",
            Body::Table(super::Table {
                headers: vec!["Source", "Value"],
                rows,
                empty: String::new(),
            }),
        ));
    }
    Ok(page
        .nav(settings_navigation(""))
        .build(state, &viewer, &uri))
}

/// `local.json`, which PHP reads on each request.
fn local_json_path() -> PathBuf {
    std::env::var_os("PHAB_CONFIG_DIR")
        .map_or_else(
            || PathBuf::from("/app/phabricator/conf/local"),
            PathBuf::from,
        )
        .join("local.json")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn values_display_as_their_types_do() {
        let option = |key| phab_app_config::option(key).unwrap();
        assert_eq!(
            display_value(option("phabricator.show-prototypes"), &json!(true)).unwrap(),
            "true"
        );
        assert_eq!(
            display_value(option("phabricator.allowed-uris"), &json!(["a", "b"])).unwrap(),
            "a\nb"
        );
        assert_eq!(
            display_value(option("config.lock"), &json!({"a": true, "b": true})).unwrap(),
            "a\nb"
        );
        assert_eq!(pretty_json(&json!([])), "{}\n");
        assert_eq!(pretty_json(&json!("a/b")), "\"a/b\"");
        assert_eq!(
            pretty_json(&json!({"k": [1]})),
            "{\n  \"k\": [\n    1\n  ]\n}\n"
        );
    }
}
