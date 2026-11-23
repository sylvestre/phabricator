//! `diffusion.repository.edit` (`DiffusionRepositoryEditConduitAPIMethod`):
//! the transactions of `DiffusionRepositoryEditEngine` on an existing
//! repository, applied as `PhabricatorRepositoryEditor` does: the
//! repository row, the transactions, the `needs-update` status message
//! cleared (`applyFinalEffects()`), and the publish task, which indexes the
//! repository (no mail, no feed, no Herald).
//!
//! Ported edit types: `name`, `callsign`, `shortName`, `description`
//! (without mentions, references or files, which add transactions),
//! `status` and `publish`, once each. Left to PHP: creating repositories,
//! the other types (policies, projects, refs, limits, symbols...), empty
//! names, repositories the viewer can't see or edit (PHP raises policy
//! exceptions), repositories served by a cluster or without a local path,
//! edits without effect, and several validation errors at once.

use phab_app_differential::repository;
use phab_conduit::edit::{self, Identifier, Planned, Publication, RawTransaction};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{PhidType, php};
use phab_db::{Database, rows};
use serde_json::{Map, Value, json};

use crate::import::repository::{STATUS_NEEDS_UPDATE, write_status_message};

/// The byte length of the `name` column (`sort255`).
const NAME_LENGTH: usize = 255;
/// `PhabricatorRepository::getStatusMap()`.
const STATUSES: [&str; 2] = ["active", "inactive"];

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// One transaction of the request.
#[derive(Debug, Clone, PartialEq)]
enum Change {
    Name(String),
    Callsign(String),
    Slug(String),
    Description(String),
    Status(String),
    Publish(bool),
}

/// The transactions of the request, as the edit types build them.
fn changes(request: &ApiRequest<'_>, raw: &[RawTransaction]) -> Result<Vec<Change>, MethodError> {
    edit::check_unique(
        &raw.iter()
            .map(|xaction| xaction.kind.as_str())
            .collect::<Vec<_>>(),
    )?;
    raw.iter()
        .map(|xaction| {
            let kind = xaction.kind.as_str();
            let text = || edit::string(kind, &xaction.value);
            Ok(match kind {
                "name" => Change::Name(text()?),
                "callsign" => Change::Callsign(text()?),
                "shortName" => Change::Slug(text()?),
                "description" => Change::Description(text()?),
                "status" => Change::Status(text()?),
                "publish" => {
                    Change::Publish(edit::boolean(kind, &xaction.value, request.strictly_typed)?)
                }
                _ => return Err(unsupported("transactions not ported")),
            })
        })
        .collect()
}

/// The columns of the repository the edit reads and writes.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    id: i64,
    phid: String,
    name: String,
    callsign: Option<String>,
    slug: Option<String>,
    details: Map<String, Value>,
}

/// `newObjectFromIdentifier()` with `CAN_VIEW`, then `CAN_EDIT`, which
/// every transaction needs (`applyCapabilityChecks()`).
async fn load(request: &ApiRequest<'_>, identifier: Identifier) -> Result<Row, MethodError> {
    let context = request.context;
    let database = &context.database;
    let identifier = match identifier {
        Identifier::Id(id) => id.to_string(),
        Identifier::Phid(phid) => phid,
    };
    let found = repository::load_by_identifiers(database, std::slice::from_ref(&identifier))
        .await?
        .remove(&identifier)
        .ok_or_else(|| unsupported("unknown repositories"))?;
    let visible = repository::visible(
        database,
        &context.config,
        &request.viewer,
        vec![found],
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    let found = visible
        .into_iter()
        .next()
        .ok_or_else(|| unsupported("repositories the viewer can't see"))?;
    if !repository::can_edit(
        database,
        &context.config,
        &request.viewer,
        &found,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?
    {
        return Err(unsupported("repositories the viewer can't edit"));
    }
    let sql = format!(
        "SELECT id, phid, name, callsign, repositorySlug, details, localPath, \
         almanacServicePHID FROM {} WHERE id = ?",
        database.table("repository", "repository")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(found.id)
        .fetch_one(database.pool())
        .await?;
    if rows::opt_text(&row, "almanacServicePHID")?.is_some() {
        return Err(unsupported("repositories served by a cluster"));
    }
    // applyFinalEffects() gives a local path to those without one.
    if rows::opt_text(&row, "localPath")?.is_none_or(|path| path.is_empty()) {
        return Err(unsupported("repositories without a local path"));
    }
    let details = match rows::json(&row, "details")? {
        Value::Object(details) => details,
        Value::Array(list) if list.is_empty() => Map::new(),
        _ => return Err(unsupported("repository details which are not a dictionary")),
    };
    Ok(Row {
        id: rows::int(&row, "id")?,
        phid: rows::text(&row, "phid")?,
        name: rows::text(&row, "name")?,
        callsign: rows::opt_text(&row, "callsign")?,
        slug: rows::opt_text(&row, "repositorySlug")?,
        details,
    })
}

/// `PhabricatorRepository::assertValidCallsign()`.
fn callsign_error(callsign: &str) -> Option<String> {
    if callsign.len() > 32 {
        return Some(format!(
            "The callsign \"{callsign}\" is not a valid repository callsign. Callsigns must be \
             no more than 32 bytes long."
        ));
    }
    if !callsign.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return Some(format!(
            "The callsign \"{callsign}\" is not a valid repository callsign. Callsigns may only \
             contain UPPERCASE letters."
        ));
    }
    None
}

/// `PhabricatorRepository::assertValidRepositorySlug()` of a nonempty
/// short name.
fn slug_error(slug: &str) -> Option<String> {
    let invalid = |rule: &str| {
        Some(format!(
            "The name \"{slug}\" is not a valid repository short name. Repository short names \
             {rule}"
        ))
    };
    let bytes = slug.as_bytes();
    if bytes.len() > 64 {
        return invalid("must not be longer than 64 characters.");
    }
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte))
    {
        return invalid("may only contain letters, numbers, periods, hyphens and underscores.");
    }
    if !bytes[0].is_ascii_alphanumeric() {
        return invalid("must begin with a letter or number.");
    }
    if !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return invalid("must end with a letter or number.");
    }
    if slug.contains("__") || slug.contains("--") || slug.contains("..") {
        return invalid("must not contain multiple consecutive underscores, hyphens, or periods.");
    }
    if bytes.iter().all(u8::is_ascii_uppercase) {
        return invalid("may not contain only uppercase letters.");
    }
    if bytes.iter().all(u8::is_ascii_digit) {
        return invalid("may not contain only numbers.");
    }
    if slug.contains(".git") {
        return invalid(
            "must not end in \".git\". This suffix will be added automatically in appropriate \
             contexts.",
        );
    }
    None
}

/// The repository other than `id` whose `column` is `value`, if any.
async fn taken(
    database: &Database,
    (column, value): (&str, &str),
    id: i64,
) -> Result<bool, MethodError> {
    let sql = format!(
        "SELECT id FROM {} WHERE {column} = ?",
        database.table("repository", "repository")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(value)
        .fetch_optional(database.pool())
        .await?
        .map(|row| rows::int(&row, "id"))
        .transpose()?;
    Ok(found.is_some_and(|other| other != id))
}

/// The unique names of a repository.
#[derive(Debug, Clone, Copy)]
enum Identity {
    Callsign,
    Slug,
}

/// The validation error of a new callsign or short name: invalid, or the
/// name of another repository.
async fn identity_error(
    database: &Database,
    id: i64,
    (identity, old, new): (Identity, Option<&String>, Option<&String>),
) -> Result<Option<String>, MethodError> {
    let Some(new) = new.filter(|new| Some(*new) != old) else {
        return Ok(None);
    };
    let (column, invalid) = match identity {
        Identity::Callsign => ("callsign", callsign_error(new)),
        Identity::Slug => ("repositorySlug", slug_error(new)),
    };
    if invalid.is_some() {
        return Ok(invalid);
    }
    if !taken(database, (column, new), id).await? {
        return Ok(None);
    }
    Ok(Some(match identity {
        Identity::Callsign => format!(
            "The selected callsign (\"{new}\") is already in use by another repository. \
             Choose a unique callsign."
        ),
        Identity::Slug => "The selected repository short name is already in use by another \
                           repository. Choose a unique short name."
            .to_owned(),
    }))
}

/// `isTracked()`.
fn is_tracked(details: &Map<String, Value>) -> bool {
    match details.get("tracking-enabled") {
        Some(Value::String(status)) if status == "active" => true,
        Some(Value::String(status)) if status == "inactive" => false,
        Some(status) => php::is_truthy(status),
        None => false,
    }
}

/// Whether remarkup has no effect on the edit: no mentions, references or
/// files.
fn is_inert(request: &ApiRequest<'_>, text: &str) -> bool {
    let config = phab_app_differential::markup::engine_config(&request.context.config);
    let engine = phab_remarkup::phabricator::engine(config);
    let prepared = engine.prepare(text.as_bytes(), phab_remarkup::Mode::Html);
    prepared.mentioned_usernames().is_empty()
        && prepared.object_references().is_empty()
        && prepared.file_attach_intent_ids().is_empty()
}

/// A string or `null`.
fn optional(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |text| json!(text))
}

/// The values of the transactions, the repository as they leave it, and
/// the validation errors.
async fn plan(
    request: &ApiRequest<'_>,
    row: &Row,
    changes: Vec<Change>,
) -> Result<(Vec<Planned>, Row, Vec<String>), MethodError> {
    let database = &request.context.database;
    let mut edited = row.clone();
    let mut transactions = Vec::new();
    let mut errors = Vec::new();
    for change in changes {
        let (kind, old, new) = match change {
            Change::Name(name) => {
                if name.is_empty() {
                    // A missing field error, which edits drop.
                    return Err(unsupported("empty names on edits"));
                }
                if name.len() > NAME_LENGTH {
                    return Err(unsupported("names longer than their column"));
                }
                edited.name.clone_from(&name);
                ("repo:name", json!(row.name), json!(name))
            }
            Change::Callsign(callsign) => {
                let new = (!callsign.is_empty()).then_some(callsign);
                let check = (Identity::Callsign, row.callsign.as_ref(), new.as_ref());
                errors.extend(identity_error(database, row.id, check).await?);
                edited.callsign.clone_from(&new);
                (
                    "repo:callsign",
                    optional(row.callsign.as_deref()),
                    optional(new.as_deref()),
                )
            }
            Change::Slug(slug) => {
                let new = (!slug.is_empty()).then_some(slug);
                let check = (Identity::Slug, row.slug.as_ref(), new.as_ref());
                errors.extend(identity_error(database, row.id, check).await?);
                edited.slug.clone_from(&new);
                (
                    "repo:slug",
                    optional(row.slug.as_deref()),
                    optional(new.as_deref()),
                )
            }
            Change::Description(description) => {
                if !is_inert(request, &description) {
                    return Err(unsupported(
                        "descriptions with mentions, references or files",
                    ));
                }
                let old = row.details.get("description").cloned();
                edited
                    .details
                    .insert("description".to_owned(), json!(description));
                (
                    "repo:description",
                    old.unwrap_or(Value::Null),
                    json!(description),
                )
            }
            Change::Status(status) => {
                if !STATUSES.contains(&status.as_str()) {
                    errors.push(format!(
                        "Repository status \"{status}\" is not valid. Valid statuses are: {}.",
                        STATUSES.join(", ")
                    ));
                }
                // Both statuses are truthy strings.
                if !status.is_empty() && status != "0" {
                    edited
                        .details
                        .insert("newly-initialized".to_owned(), json!(false));
                }
                edited
                    .details
                    .insert("tracking-enabled".to_owned(), json!(status));
                (
                    "repo:activate",
                    json!(is_tracked(&row.details)),
                    json!(status),
                )
            }
            Change::Publish(publish) => {
                let disabled = row
                    .details
                    .get("herald-disabled")
                    .is_some_and(php::is_truthy);
                edited
                    .details
                    .insert("herald-disabled".to_owned(), json!(i64::from(!publish)));
                (
                    "repo:notify",
                    json!(i64::from(!disabled)),
                    json!(i64::from(publish)),
                )
            }
        };
        transactions.push(Planned::new(PhidType::REPOSITORY, kind, (old, new)));
    }
    Ok((transactions, edited, errors))
}

/// `diffusion.repository.edit`.
pub struct RepositoryEdit;

impl RepositoryEdit {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let raw = edit::raw_transactions(request)?;
        let Some(identifier) = edit::identifier(request)? else {
            return Err(unsupported("creating repositories"));
        };
        edit::require_writes(request)?;
        let actor = edit::actor(request)?;
        let row = load(request, identifier).await?;
        let changes = changes(request, &raw)?;
        let (transactions, edited, errors) = plan(request, &row, changes).await?;
        match errors.as_slice() {
            [] => {}
            [_] => return Err(edit::validation_error(&errors)),
            _ => return Err(unsupported("several validation errors")),
        }
        let transactions: Vec<Planned> = transactions
            .into_iter()
            .filter(Planned::has_effect)
            .collect();
        if transactions.is_empty() {
            return Err(unsupported("edits without effect"));
        }

        let database = &request.context.database;
        let publication = Publication::new(database, actor, row.phid.clone(), transactions).await?;
        let now = request.now;
        let mut transaction = database.pool().begin().await?;
        // Lisk saves every column; only these change.
        phab_db::sqlx::query(&format!(
            "UPDATE {} SET name = ?, callsign = ?, repositorySlug = ?, details = ?, \
             dateModified = ? WHERE id = ?",
            database.table("repository", "repository")
        ))
        .bind(&edited.name)
        .bind(edited.callsign.as_deref())
        .bind(edited.slug.as_deref())
        .bind(php::json_encode(&php::round_trip(&Value::Object(
            edited.details.clone(),
        ))))
        .bind(now)
        .bind(row.id)
        .execute(&mut *transaction)
        .await?;
        let table = database.table("repository", "repository_transaction");
        publication
            .write_transactions(&mut transaction, &table, now)
            .await?;
        transaction.commit().await?;
        // applyFinalEffects(), in PHP's transaction too; then the publish
        // task.
        write_status_message(
            database,
            row.id,
            (STATUS_NEEDS_UPDATE, None),
            &json!([]),
            now,
        )
        .await?;
        let mut connection = database.pool().acquire().await?;
        publication.schedule(&mut connection, database, now).await?;
        Ok(publication.response(row.id))
    }
}

impl Method for RepositoryEdit {
    fn name(&self) -> &'static str {
        "diffusion.repository.edit"
    }

    fn params(&self) -> &'static [&'static str] {
        &["transactions", "objectIdentifier"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_callsigns_like_php() {
        assert_eq!(callsign_error("MOZ"), None);
        assert!(callsign_error("Moz").is_some_and(|error| error.contains("UPPERCASE")));
        assert!(callsign_error(&"A".repeat(33)).is_some_and(|error| error.contains("32 bytes")));
    }

    #[test]
    fn checks_short_names_like_php() {
        assert_eq!(slug_error("mozilla-central"), None);
        assert_eq!(slug_error("a.b_c"), None);
        for (slug, rule) in [
            ("a b", "may only contain"),
            ("-a", "must begin"),
            ("a-", "must end"),
            ("a--b", "multiple consecutive"),
            ("ABC", "only uppercase"),
            ("123", "only numbers"),
            ("a.gitx", ".git"),
        ] {
            assert!(
                slug_error(slug).is_some_and(|error| error.contains(rule)),
                "{slug}"
            );
        }
    }

    #[test]
    fn reads_tracking_like_php() {
        let details = |value: Value| {
            let mut map = Map::new();
            map.insert("tracking-enabled".to_owned(), value);
            map
        };
        assert!(is_tracked(&details(json!("active"))));
        assert!(!is_tracked(&details(json!("inactive"))));
        assert!(is_tracked(&details(json!(true))));
        assert!(!is_tracked(&details(json!(false))));
        assert!(!is_tracked(&Map::new()));
    }
}
