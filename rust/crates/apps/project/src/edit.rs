//! `project.edit` (`ProjectEditConduitAPIMethod`): the transactions of
//! `PhabricatorProjectEditEngine`, through `buildConduitResponse()`, applied
//! by [`crate::editor`].
//!
//! Ported transaction types: `name`, `icon`, `color`, `slugs`,
//! `members.add`, `members.remove` and `members.set` with user PHIDs, once
//! each. Left to PHP: the others (`description` and other custom fields,
//! policies, `space`, `parent`, `milestone`, ...), usernames, identifiers
//! other than IDs and PHIDs, projects the viewer can't see, and edit forms
//! configured in the web UI.

use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use serde_json::{Value, json};

use crate::editor::{self, Change, Operation, Source, Target};
use crate::{Project, policy, search, storage};

/// `PhabricatorProjectEditEngine::ENGINECONST`.
const ENGINE_KEY: &str = "projects.project";

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// A list of strings.
pub(crate) fn strings(value: &Value) -> Result<Vec<String>, MethodError> {
    let Value::Array(items) = value else {
        return Err(unsupported("lists which are not lists"));
    };
    items
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect::<Option<_>>()
        .ok_or_else(|| unsupported("lists of values other than strings"))
}

/// The transactions of the request, as the edit types build them.
fn changes(request: &ApiRequest<'_>) -> Result<Vec<Change>, MethodError> {
    let Some(Value::Array(transactions)) = request.value("transactions") else {
        return Err(unsupported("unusual transaction lists"));
    };
    let mut changes = Vec::new();
    for transaction in transactions {
        let (Some(Value::String(kind)), Some(value)) =
            (transaction.get("type"), transaction.get("value"))
        else {
            return Err(unsupported("unusual transactions"));
        };
        let text = || {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| unsupported("values which are not strings"))
        };
        let change = match kind.as_str() {
            "name" => Change::Name(text()?),
            "icon" => Change::Icon(text()?),
            "color" => Change::Color(text()?),
            "slugs" => Change::Slugs(strings(value)?),
            "members.add" => Change::Members(Operation::Add, strings(value)?),
            "members.remove" => Change::Members(Operation::Remove, strings(value)?),
            "members.set" => Change::Members(Operation::Set, strings(value)?),
            _ => return Err(unsupported("transactions not ported")),
        };
        changes.push(change);
    }
    Ok(changes)
}

/// `newObjectFromIdentifier()` with `CAN_VIEW`: a project by ID or PHID,
/// which the viewer must see.
async fn load_project(
    request: &ApiRequest<'_>,
    identifier: &Value,
) -> Result<Project, MethodError> {
    let context = request.context;
    let database = &context.database;
    let found = match identifier {
        Value::Number(number) => match number.as_i64() {
            Some(id) => storage::load_by_id(database, id).await?,
            None => None,
        },
        Value::String(text) if text.bytes().all(|byte| byte.is_ascii_digit()) => {
            match text.parse() {
                Ok(id) => storage::load_by_id(database, id).await?,
                Err(_) => None,
            }
        }
        Value::String(text) if text.starts_with("PHID-PROJ-") => {
            storage::load_by_phid(database, &[Phid::new(text.as_str())])
                .await?
                .into_iter()
                .next()
        }
        _ => return Err(unsupported("identifiers other than IDs and PHIDs")),
    };
    // PHP raises an exception for unknown and hidden projects.
    let project = found.ok_or_else(|| unsupported("unknown projects"))?;
    let (visible, _) = policy::visible_projects(
        database,
        &context.config,
        &request.viewer,
        vec![project],
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    visible
        .into_iter()
        .next()
        .ok_or_else(|| unsupported("projects the viewer can't see"))
}

/// Forms configured in the web UI may change the fields.
pub(crate) async fn check_configuration(request: &ApiRequest<'_>) -> Result<(), MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT id FROM {} WHERE engineKey = ? LIMIT 1",
        database.table("search", "search_editengineconfiguration")
    );
    if phab_db::sqlx::query(&sql)
        .bind(ENGINE_KEY)
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Err(unsupported("configured project forms"));
    }
    Ok(())
}

/// `project.edit`.
pub struct ProjectEdit;

impl ProjectEdit {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        check_configuration(request).await?;
        let mut changes = changes(request)?;
        let identifier = request
            .value("objectIdentifier")
            .filter(|value| php::is_truthy(value));
        let target = if let Some(identifier) = identifier {
            Target {
                project: Some(load_project(request, identifier).await?),
            }
        } else {
            editor::check_create_capability(request).await?;
            changes.insert(0, Change::Create);
            Target { project: None }
        };
        if changes.is_empty() {
            return Err(unsupported("empty transaction lists"));
        }
        let outcome = editor::apply(request, target, changes, Source::Conduit).await?;
        Ok(json!({
            "object": {"id": outcome.id, "phid": outcome.phid},
            "transactions": outcome
                .transactions
                .iter()
                .map(|phid| json!({"phid": phid}))
                .collect::<Vec<_>>(),
        }))
    }
}

impl Method for ProjectEdit {
    fn name(&self) -> &'static str {
        "project.edit"
    }

    fn params(&self) -> &'static [&'static str] {
        &["transactions", "objectIdentifier"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}
