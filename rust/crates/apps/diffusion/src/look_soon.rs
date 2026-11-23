//! `diffusion.looksoon` (`DiffusionLookSoonConduitAPIMethod`): asks the
//! pull daemon to update repositories soon, with a `needs-update` status
//! message (`writeStatusMessage()`) on each repository the viewer can see.
//!
//! Ported: every parameter (`urgency` does nothing). Left to PHP:
//! identifiers which are not lists of strings, and calls which may not
//! write (shadow mode, read-only installs).

use phab_app_differential::repository;
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use serde_json::{Value, json};

use crate::import::repository::{CODE_OKAY, STATUS_NEEDS_UPDATE, write_status_message};

/// The identifiers of the call: `repositories`, or the deprecated
/// `callsigns` when it is falsy.
fn identifiers(request: &ApiRequest<'_>) -> Result<Vec<String>, MethodError> {
    let value = match request.value("repositories") {
        Some(value) if phab_core::php::is_truthy(value) => Some(value),
        _ => request
            .value("callsigns")
            .filter(|value| phab_core::php::is_truthy(value)),
    };
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(items) => items.values().collect(),
        _ => return Err(MethodError::Unsupported("identifiers which are not lists")),
    };
    items
        .into_iter()
        .map(|item| match item {
            Value::String(identifier) => Ok(identifier.clone()),
            _ => Err(MethodError::Unsupported(
                "identifiers which are not strings",
            )),
        })
        .collect()
}

/// `diffusion.looksoon`.
pub struct LookSoon;

impl LookSoon {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let identifiers = identifiers(request)?;
        if identifiers.is_empty() {
            return Ok(Value::Null);
        }
        phab_app_files::storage::require_writes(request)?;
        let context = request.context;
        let database = &context.database;
        let found = repository::load_by_identifiers(database, &identifiers).await?;
        // One update per repository, as `PhabricatorRepositoryQuery`
        // returns each once, newest first.
        let mut repositories: Vec<repository::Repository> = Vec::new();
        for found in found.into_values() {
            if !repositories.iter().any(|known| known.id == found.id) {
                repositories.push(found);
            }
        }
        repositories.sort_by_key(|repository| std::cmp::Reverse(repository.id));
        let visible = repository::visible(
            database,
            &context.config,
            &request.viewer,
            repositories,
            request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        for repository in visible {
            write_status_message(
                database,
                repository.id,
                (STATUS_NEEDS_UPDATE, Some(CODE_OKAY)),
                &json!([]),
                request.now,
            )
            .await?;
        }
        Ok(Value::Null)
    }
}

impl Method for LookSoon {
    fn name(&self) -> &'static str {
        "diffusion.looksoon"
    }

    fn params(&self) -> &'static [&'static str] {
        &["callsigns", "repositories", "urgency"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}
