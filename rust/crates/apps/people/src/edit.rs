//! `user.edit` (`UserEditConduitAPIMethod`), the transactions of
//! `PhabricatorUserEditEngine` through `buildConduitResponse()`, and the
//! deprecated `user.enable` and `user.disable`
//! (`UserEnableConduitAPIMethod`, `UserDisableConduitAPIMethod`), which
//! call `user.edit` once per user. All are applied by [`crate::editor`].
//!
//! Ported: `disabled` and `approved` with boolean values, once each, on
//! users given by ID or PHID. Left to PHP: the custom field transactions
//! (`realName`, `title`, `icon`, `blurb`), `mfa`, usernames and other
//! monograms, values which are not booleans, creating users (which no one
//! may do through Conduit), and `user.enable`/`user.disable` calls whose
//! edits PHP would apply only in part before failing.

use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::php;
use serde_json::{Value, json};

use crate::APPLICATION;
use crate::editor::{self, Change, Identifier};

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// The transactions of the request, as the edit types build them.
fn changes(request: &ApiRequest<'_>) -> Result<Vec<Change>, MethodError> {
    let Some(Value::Array(transactions)) = request.value("transactions") else {
        return Err(unsupported("unusual transaction lists"));
    };
    transactions
        .iter()
        .map(|transaction| {
            let (Some(Value::String(kind)), Some(value)) =
                (transaction.get("type"), transaction.get("value"))
            else {
                return Err(unsupported("unusual transactions"));
            };
            let Value::Bool(value) = *value else {
                return Err(unsupported("values which are not booleans"));
            };
            match kind.as_str() {
                "disabled" => Ok(Change::Disabled(value)),
                "approved" => Ok(Change::Approved(value)),
                _ => Err(unsupported("transactions not ported")),
            }
        })
        .collect()
}

/// `newObjectFromIdentifier()`: a user by ID or PHID.
async fn load_user(
    request: &ApiRequest<'_>,
    identifier: &Value,
) -> Result<editor::EditedUser, MethodError> {
    let database = &request.context.database;
    let found = match identifier {
        Value::Number(number) => match number.as_i64() {
            Some(id) => editor::load_user(database, Identifier::Id(id)).await?,
            None => return Err(unsupported("identifiers other than IDs and PHIDs")),
        },
        Value::String(text) if text.bytes().all(|byte| byte.is_ascii_digit()) => {
            match text.parse() {
                Ok(id) => editor::load_user(database, Identifier::Id(id)).await?,
                Err(_) => None,
            }
        }
        Value::String(text) if text.starts_with("PHID-USER-") => {
            editor::load_user(database, Identifier::Phid(text)).await?
        }
        _ => return Err(unsupported("identifiers other than IDs and PHIDs")),
    };
    // PHP raises an exception for unknown users.
    found.ok_or_else(|| unsupported("unknown users"))
}

/// `user.edit`.
pub struct UserEdit;

impl UserEdit {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        editor::check_environment(request).await?;
        let changes = changes(request)?;
        let Some(identifier) = request
            .value("objectIdentifier")
            .filter(|value| php::is_truthy(value))
        else {
            // `getCreateNewObjectPolicy()` is "no one".
            return Err(unsupported("creating users"));
        };
        let user = load_user(request, identifier).await?;
        let plan = editor::plan(request, user, &changes).await?;
        editor::write(request, &plan).await?;
        let transactions: Vec<Value> = plan
            .transaction_phids()
            .into_iter()
            .map(|phid| json!({"phid": phid}))
            .collect();
        Ok(json!({
            "object": {"id": plan.user_id(), "phid": plan.user_phid()},
            "transactions": transactions,
        }))
    }
}

impl Method for UserEdit {
    fn name(&self) -> &'static str {
        "user.edit"
    }

    fn params(&self) -> &'static [&'static str] {
        &["transactions", "objectIdentifier"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

/// `ERR-PERMISSIONS` of `user.enable` and `user.disable`.
const ERR_PERMISSIONS: &str = "ERR-PERMISSIONS";
/// `ERR-BAD-PHID` of `user.enable` and `user.disable`.
const ERR_BAD_PHID: &str = "ERR-BAD-PHID";
/// Their `defineErrorTypes()`.
const TOGGLE_ERRORS: &[(&str, &str)] = &[
    (ERR_PERMISSIONS, "Only admins can call this method."),
    (ERR_BAD_PHID, "Non existent user PHID."),
];

fn conduit_error(code: &str) -> MethodError {
    MethodError::Conduit {
        code: code.to_owned(),
        info: None,
    }
}

/// `user.enable` and `user.disable`: `user.edit` with a `disabled`
/// transaction on each user, in order.
async fn toggle(request: &ApiRequest<'_>, disabled: bool) -> Result<Value, MethodError> {
    if !request.viewer.is_admin {
        return Err(conduit_error(ERR_PERMISSIONS));
    }
    let phids: Vec<&str> = match request.value("phids") {
        Some(Value::Array(list)) if !list.is_empty() => list
            .iter()
            .map(Value::as_str)
            .collect::<Option<_>>()
            .ok_or_else(|| unsupported("PHIDs which are not strings"))?,
        // `%Ls` refuses empty lists and other values.
        _ => return Err(unsupported("PHID lists which are not lists")),
    };
    let database = &request.context.database;
    let sql = format!(
        "SELECT COUNT(*) AS found FROM {} WHERE phid IN ({})",
        database.table("user", "user"),
        phab_db::placeholders(phids.len())
    );
    let row = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().copied())
        .fetch_one(database.pool())
        .await?;
    let found = phab_db::rows::int(&row, "found")?;
    if usize::try_from(found).ok() != Some(phids.len()) {
        return Err(conduit_error(ERR_BAD_PHID));
    }

    editor::check_environment(request).await?;
    let changes = [Change::Disabled(disabled)];
    let mut plans = Vec::with_capacity(phids.len());
    for (index, phid) in phids.iter().enumerate() {
        let planned = match editor::load_user(database, Identifier::Phid(phid)).await? {
            Some(user) => editor::plan(request, user, &changes).await,
            None => Err(unsupported("users deleted meanwhile")),
        };
        match planned {
            Ok(plan) => plans.push(plan),
            // PHP has applied the edits of the users before.
            Err(MethodError::Conduit { .. }) if index > 0 => {
                return Err(unsupported("calls failing after some edits"));
            }
            Err(error) => return Err(error),
        }
    }
    for plan in &plans {
        editor::write(request, plan).await?;
    }
    Ok(Value::Null)
}

/// `user.enable`.
pub struct UserEnable;

impl Method for UserEnable {
    fn name(&self) -> &'static str {
        "user.enable"
    }

    fn params(&self) -> &'static [&'static str] {
        &["phids"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        TOGGLE_ERRORS
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(toggle(request, false))
    }
}

/// `user.disable`.
pub struct UserDisable;

impl Method for UserDisable {
    fn name(&self) -> &'static str {
        "user.disable"
    }

    fn params(&self) -> &'static [&'static str] {
        &["phids"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        TOGGLE_ERRORS
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(toggle(request, true))
    }
}
