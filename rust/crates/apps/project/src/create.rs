//! `project.create` (`ProjectCreateConduitAPIMethod`, frozen): a name,
//! then an icon, a color and hashtags when given, then members, applied by
//! [`crate::editor`] without a `core:create` transaction.
//!
//! Left to PHP: calls without a `members` list (PHP's `array_fuse()`
//! refuses `null`), values of other types, and what the editor leaves to
//! it.

use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::php;
use serde_json::{Value, json};

use crate::edit::strings;
use crate::editor::{self, Change, Operation, Target};
use crate::search;

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// `project.create`.
pub struct ProjectCreate;

impl ProjectCreate {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        editor::check_create_capability(request).await?;
        let Some(Value::String(name)) = request.value("name") else {
            return Err(unsupported("names which are not strings"));
        };
        let members = match request.value("members") {
            Some(value @ Value::Array(_)) => strings(value)?,
            _ => return Err(unsupported("calls without a list of members")),
        };
        // `if ($request->getValue(...))`.
        let given = |key| request.value(key).filter(|value| php::is_truthy(value));
        let text = |value: &Value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| unsupported("values which are not strings"))
        };
        let mut changes = vec![Change::Name(name.clone())];
        if let Some(icon) = given("icon") {
            changes.push(Change::Icon(text(icon)?));
        }
        if let Some(color) = given("color") {
            changes.push(Change::Color(text(color)?));
        }
        if let Some(tags) = given("tags") {
            changes.push(Change::Slugs(strings(tags)?));
        }
        changes.push(Change::Members(Operation::Add, members));
        let outcome = editor::apply(request, Target { project: None }, changes).await?;
        // `buildProjectInfoDictionary()` of the object the editor saved,
        // whose members and slugs are still the empty lists
        // `initializeNewProject()` attached.
        Ok(json!({
            "id": outcome.id,
            "phid": outcome.phid,
            "name": outcome.name,
            "profileImagePHID": null,
            "icon": outcome.icon,
            "color": outcome.color,
            "members": [],
            "slugs": [],
            "dateCreated": request.now,
            "dateModified": request.now,
        }))
    }
}

impl Method for ProjectCreate {
    fn name(&self) -> &'static str {
        "project.create"
    }

    fn params(&self) -> &'static [&'static str] {
        &["name", "members", "icon", "color", "tags"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}
