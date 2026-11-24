//! `diffusion.uri.edit` (`DiffusionURIEditConduitAPIMethod`): the
//! transactions of `DiffusionURIEditEngine` on an existing repository URI,
//! applied as `DiffusionURIEditor` does: the URI row, the transactions,
//! then `applyFinalEffects()`, which saves the repository as its Observe
//! URI leaves it (hosted or not, its `remote-uri` and its credential),
//! rebuilds `repository_uriindex` from the normalized URIs of its enabled
//! URIs and clears the `needs-update` status message; then the publish
//! task (no mail, no feed, no Herald).
//!
//! Ported edit types: `uri`, `io`, `display` and `disable`, once each, on
//! URIs which are not builtin, of Git repositories. Left to PHP: creating
//! URIs, the `repository` and `credential` types, builtin URIs and
//! repositories with builtin URIs in use (their URIs come from the
//! `diffusion.ssh-*` options and the base URI), URIs `PhutilURI` may read
//! differently (see [`crate::remote_uri`]), repositories served by a
//! cluster, empty URIs, URIs and repositories the viewer can't see or
//! edit, edits without effect, and several validation errors at once.

use phab_app_differential::repository;
use phab_conduit::edit::{self, Identifier, Planned, Publication, RawTransaction};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Config, Phid, PhidType, php};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, rows};
use serde_json::{Map, Value, json};

use crate::import::repository::{STATUS_NEEDS_UPDATE, write_status_message};
use crate::remote_uri::{self, RemoteUri};

/// `PhabricatorRepositoryURIPHIDType::TYPECONST`.
const URI_PHID_TYPE: PhidType = PhidType::new(*b"RURI");
/// `getAvailableIOTypeOptions()` of a URI which is not builtin.
const IO_TYPES: [&str; 4] = ["default", "none", "observe", "mirror"];
/// `getAvailableDisplayTypeOptions()`.
const DISPLAY_TYPES: [&str; 3] = ["default", "always", "never"];
/// `PhabricatorRepositoryURI::IO_OBSERVE`.
const IO_OBSERVE: &str = "observe";
/// `PhabricatorRepositoryURI::IO_READWRITE`.
const IO_READWRITE: &str = "readwrite";

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// One transaction of the request.
#[derive(Debug, Clone, PartialEq)]
enum Change {
    Uri(String),
    Io(String),
    Display(String),
    Disable(bool),
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
                "uri" => Change::Uri(text()?),
                "io" => Change::Io(text()?),
                "display" => Change::Display(text()?),
                "disable" => {
                    Change::Disable(edit::boolean(kind, &xaction.value, request.strictly_typed)?)
                }
                _ => return Err(unsupported("transactions not ported")),
            })
        })
        .collect()
}

/// A `repository_uri` row.
#[derive(Debug, Clone, PartialEq)]
struct UriRow {
    id: i64,
    phid: String,
    uri: String,
    builtin_protocol: Option<String>,
    builtin_identifier: Option<String>,
    io: String,
    display: String,
    disabled: bool,
    credential: Option<String>,
}

const URI_COLUMNS: &str = "id, phid, uri, builtinProtocol, builtinIdentifier, ioType, \
     displayType, isDisabled, credentialPHID";

impl UriRow {
    fn from_row(row: &MySqlRow) -> Result<Self, MethodError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: rows::text(row, "phid")?,
            uri: rows::text(row, "uri")?,
            builtin_protocol: rows::opt_text(row, "builtinProtocol")?,
            builtin_identifier: rows::opt_text(row, "builtinIdentifier")?,
            io: rows::text(row, "ioType")?,
            display: rows::text(row, "displayType")?,
            disabled: rows::int(row, "isDisabled")? != 0,
            credential: rows::opt_text(row, "credentialPHID")?,
        })
    }

    /// `isBuiltin()`.
    fn is_builtin(&self) -> bool {
        self.builtin_protocol
            .as_deref()
            .is_some_and(php::is_truthy_str)
    }

    /// `getEffectiveIOType()` of a URI which is not builtin.
    fn effective_io(&self) -> &str {
        if self.io == "default" {
            "none"
        } else {
            &self.io
        }
    }

    /// `getEffectiveURI()`.
    fn effective_uri(&self) -> Result<RemoteUri, MethodError> {
        RemoteUri::parse(&self.uri)
            .map(RemoteUri::effective)
            .ok_or(unsupported("URIs PhutilURI may read differently"))
    }
}

/// The columns of the repository the edit reads and writes.
#[derive(Debug, Clone, PartialEq)]
struct RepositoryRow {
    id: i64,
    phid: String,
    has_callsign: bool,
    has_slug: bool,
    view_policy: String,
    details: Map<String, Value>,
}

/// The URI and its repository, with `CAN_VIEW` on the repository (the
/// URI's query) and `CAN_EDIT`, which the URI's extended policy requires.
async fn load(
    request: &ApiRequest<'_>,
    identifier: Identifier,
) -> Result<(UriRow, RepositoryRow), MethodError> {
    let context = request.context;
    let database = &context.database;
    let table = database.table("repository", "repository_uri");
    let (column, value) = match identifier {
        Identifier::Id(id) => ("id", id.to_string()),
        Identifier::Phid(phid) => ("phid", phid),
    };
    let sql = format!("SELECT {URI_COLUMNS}, repositoryPHID FROM {table} WHERE {column} = ?");
    let row = phab_db::sqlx::query(&sql)
        .bind(value)
        .fetch_optional(database.pool())
        .await?
        .ok_or(unsupported("unknown URIs"))?;
    let uri = UriRow::from_row(&row)?;
    let repository_phid = rows::text(&row, "repositoryPHID")?;

    let found = repository::load(database, &[Phid::new(repository_phid)]).await?;
    let visible = repository::visible(
        database,
        &context.config,
        &request.viewer,
        found,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    let found = visible
        .into_iter()
        .next()
        .ok_or(unsupported("URIs of repositories the viewer can't see"))?;
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
        return Err(unsupported("URIs of repositories the viewer can't edit"));
    }
    if uri.is_builtin() {
        return Err(unsupported("builtin URIs"));
    }
    let sql = format!(
        "SELECT id, phid, callsign, repositorySlug, versionControlSystem, viewPolicy, \
         details, almanacServicePHID FROM {} WHERE id = ?",
        database.table("repository", "repository")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(found.id)
        .fetch_one(database.pool())
        .await?;
    if rows::text(&row, "versionControlSystem")? != "git" {
        return Err(unsupported("URIs of repositories other than Git"));
    }
    if rows::opt_text(&row, "almanacServicePHID")?.is_some_and(|phid| !phid.is_empty()) {
        return Err(unsupported("repositories served by a cluster"));
    }
    let details = match rows::json(&row, "details")? {
        Value::Object(details) => details,
        Value::Array(list) if list.is_empty() => Map::new(),
        _ => return Err(unsupported("repository details which are not a dictionary")),
    };
    let repository = RepositoryRow {
        id: rows::int(&row, "id")?,
        phid: rows::text(&row, "phid")?,
        has_callsign: rows::opt_text(&row, "callsign")?.is_some(),
        has_slug: rows::opt_text(&row, "repositorySlug")?.is_some(),
        view_policy: rows::text(&row, "viewPolicy")?,
        details,
    };
    Ok((uri, repository))
}

/// Whether `newBuiltinURIs()` enables the builtin URI of `protocol` and
/// `identifier`; `None` for builtin URIs it doesn't make.
fn builtin_enabled(
    config: &Config,
    repository: &RepositoryRow,
    base_protocol: &str,
    (protocol, identifier): (&str, Option<&str>),
) -> Option<bool> {
    let has_identifier = match identifier? {
        "callsign" => repository.has_callsign,
        "shortname" => repository.has_slug,
        "id" => true,
        _ => return None,
    };
    let allow_http =
        repository.view_policy == "public" || config.truthy("diffusion.allow-http-auth");
    let supported = match protocol {
        "ssh" => config.nonempty_string("phd.user").is_some(),
        "https" => base_protocol == "https" && allow_http,
        "http" => !config.truthy("security.require-https") && allow_http,
        _ => return None,
    };
    Some(supported && has_identifier)
}

/// `PhabricatorRepositoryURIQuery::withRepositories()` then
/// `attachURIs()`: the URIs of the repository (newest first), without the
/// builtin URIs `newBuiltinURIs()` disables or doesn't make. Builtin URIs
/// in use are left to PHP.
async fn repository_uris(
    request: &ApiRequest<'_>,
    repository: &RepositoryRow,
    base: &RemoteUri,
) -> Result<Vec<UriRow>, MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT {URI_COLUMNS} FROM {} WHERE repositoryPHID = ? ORDER BY id DESC",
        database.table("repository", "repository_uri")
    );
    let mut uris = Vec::new();
    for row in phab_db::sqlx::query(&sql)
        .bind(&repository.phid)
        .fetch_all(database.pool())
        .await?
    {
        let uri = UriRow::from_row(&row)?;
        if let Some(protocol) = uri.builtin_protocol.as_deref().filter(|_| uri.is_builtin()) {
            let key = (protocol, uri.builtin_identifier.as_deref());
            if builtin_enabled(&request.context.config, repository, &base.protocol, key)
                == Some(true)
            {
                return Err(unsupported("repositories with builtin URIs in use"));
            }
            continue;
        }
        uris.push(uri);
    }
    Ok(uris)
}

/// The validation error of a new I/O type (`validateTransaction()` of
/// `TYPE_IO`).
fn io_error(edited: &UriRow, others: &[UriRow], new: &str) -> Option<String> {
    if !IO_TYPES.contains(&new) {
        return Some(format!(
            "Value \"{new}\" is not a valid IO setting for this URI. Available types for \
             this URI are: {}.",
            IO_TYPES.join(", ")
        ));
    }
    if new != IO_OBSERVE {
        return None;
    }
    // Observe: no other Observe or Read/Write URI.
    let conflict = others
        .iter()
        .filter(|uri| uri.id != edited.id)
        .map(UriRow::effective_io)
        .find(|io| *io == IO_READWRITE || *io == IO_OBSERVE)?;
    Some(
        if conflict == IO_OBSERVE {
            "You can not set this URI to use Observe IO because another URI for this repository \
         is already configured in Observe IO mode. A repository can not observe two \
         different remotes simultaneously. Turn off IO for the other URI first."
        } else {
            "You can not set this URI to use Observe IO because another URI for this repository \
         is already configured in Read/Write IO mode. A repository can not simultaneously be \
         writable and observe a remote. Turn off IO for the other URI first."
        }
        .to_owned(),
    )
}

/// The values of the transactions, the URI as they leave it, and the
/// validation errors.
fn plan(
    row: &UriRow,
    others: &[UriRow],
    changes: Vec<Change>,
) -> Result<(Vec<Planned>, UriRow, Vec<String>), MethodError> {
    let mut edited = row.clone();
    let mut transactions = Vec::new();
    let mut errors = Vec::new();
    for change in changes {
        let (kind, old, new) = match change {
            Change::Uri(uri) => {
                if uri.is_empty() {
                    // A missing field error, which edits drop.
                    return Err(unsupported("empty URIs on edits"));
                }
                if uri.chars().count() > 255 {
                    return Err(unsupported("URIs longer than their column"));
                }
                if uri != row.uri {
                    let error = remote_uri::validation_error(&uri)
                        .ok_or(unsupported("URIs PhutilURI may read differently"))?;
                    errors.extend(error);
                }
                edited.uri.clone_from(&uri);
                ("diffusion.uri.uri", json!(row.uri), json!(uri))
            }
            Change::Io(io) => {
                errors.extend(io_error(row, others, &io));
                edited.io.clone_from(&io);
                ("diffusion.uri.io", json!(row.io), json!(io))
            }
            Change::Display(display) => {
                if !DISPLAY_TYPES.contains(&display.as_str()) {
                    errors.push(format!(
                        "Value \"{display}\" is not a valid display setting for this URI. \
                         Available types for this URI are: {}.",
                        DISPLAY_TYPES.join(", ")
                    ));
                }
                edited.display.clone_from(&display);
                ("diffusion.uri.display", json!(row.display), json!(display))
            }
            Change::Disable(disabled) => {
                edited.disabled = disabled;
                (
                    "diffusion.uri.disable",
                    json!(i64::from(row.disabled)),
                    json!(i64::from(disabled)),
                )
            }
        };
        transactions.push(Planned::new(URI_PHID_TYPE, kind, (old, new)));
    }
    Ok((transactions, edited, errors))
}

/// What `applyFinalEffects()` writes: the repository's details and
/// credential, and the URI index.
struct FinalEffects {
    details: Map<String, Value>,
    credential: Option<String>,
    index: Vec<String>,
}

/// `applyFinalEffects()` once `edited` is saved.
fn final_effects(
    config: &Config,
    repository: &RepositoryRow,
    uris: &[UriRow],
    base: &RemoteUri,
) -> Result<FinalEffects, MethodError> {
    let mut details = repository.details.clone();
    let observe = uris.iter().find(|uri| uri.io == IO_OBSERVE);
    let credential = if let Some(observe) = observe {
        let remote = observe.effective_uri()?.to_uri_string();
        details.insert("hosting-enabled".to_owned(), json!(false));
        details.insert("remote-uri".to_owned(), json!(remote));
        observe.credential.clone()
    } else {
        details.insert("hosting-enabled".to_owned(), json!(true));
        details.insert("remote-uri".to_owned(), Value::Null);
        None
    };
    // getURINormalizerDomainMap().
    let mut domains = vec![("<base-uri>", base.domain.to_ascii_lowercase())];
    if let Some(ssh_host) = config.nonempty_string("diffusion.ssh-host") {
        if !ssh_host.is_ascii() {
            return Err(unsupported("SSH hosts which are not ASCII"));
        }
        domains.push(("<ssh-host>", ssh_host.to_ascii_lowercase()));
    }
    let index = uris
        .iter()
        .filter(|uri| !uri.disabled)
        .map(|uri| {
            let display = uri.effective_uri()?.to_uri_string();
            remote_uri::normalized_git(&display, &domains)
                .ok_or(unsupported("URIs PhutilURI may read differently"))
        })
        .collect::<Result<_, _>>()?;
    Ok(FinalEffects {
        details,
        credential,
        index,
    })
}

/// Writes the edit: the URI and its transactions, then the final effects.
async fn write(
    database: &Database,
    (publication, edited): (&Publication, &UriRow),
    (repository, effects): (&RepositoryRow, &FinalEffects),
    now: i64,
) -> Result<(), MethodError> {
    let mut transaction = database.pool().begin().await?;
    // Lisk saves every column; only these change.
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET uri = ?, ioType = ?, displayType = ?, isDisabled = ?, \
         credentialPHID = ?, dateModified = ? WHERE id = ?",
        database.table("repository", "repository_uri")
    ))
    .bind(&edited.uri)
    .bind(&edited.io)
    .bind(&edited.display)
    .bind(i64::from(edited.disabled))
    .bind(edited.credential.as_deref())
    .bind(now)
    .bind(edited.id)
    .execute(&mut *transaction)
    .await?;
    let table = database.table("repository", "repository_uritransaction");
    publication
        .write_transactions(&mut transaction, &table, now)
        .await?;
    transaction.commit().await?;

    // applyFinalEffects(): the repository, then the index.
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET details = ?, credentialPHID = ?, dateModified = ? WHERE id = ?",
        database.table("repository", "repository")
    ))
    .bind(php::json_encode(&php::round_trip(&Value::Object(
        effects.details.clone(),
    ))))
    .bind(effects.credential.as_deref())
    .bind(now)
    .bind(repository.id)
    .execute(database.pool())
    .await?;
    let table = database.table("repository", "repository_uriindex");
    let mut transaction = database.pool().begin().await?;
    phab_db::sqlx::query(&format!("DELETE FROM {table} WHERE repositoryPHID = ?"))
        .bind(&repository.phid)
        .execute(&mut *transaction)
        .await?;
    if !effects.index.is_empty() {
        let sql = format!(
            "INSERT INTO {table} (repositoryPHID, repositoryURI) VALUES {}",
            vec!["(?, ?)"; effects.index.len()].join(", ")
        );
        let mut query = phab_db::sqlx::query(&sql);
        for uri in &effects.index {
            query = query.bind(&repository.phid).bind(uri);
        }
        query.execute(&mut *transaction).await?;
    }
    transaction.commit().await?;
    write_status_message(
        database,
        repository.id,
        (STATUS_NEEDS_UPDATE, None),
        &json!([]),
        now,
    )
    .await?;
    let mut connection = database.pool().acquire().await?;
    publication.schedule(&mut connection, database, now).await?;
    Ok(())
}

/// `diffusion.uri.edit`.
pub struct UriEdit;

impl UriEdit {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let raw = edit::raw_transactions(request)?;
        let Some(identifier) = edit::identifier(request)? else {
            return Err(unsupported("creating repository URIs"));
        };
        edit::require_writes(request)?;
        let actor = edit::actor(request)?;
        let (row, repository) = load(request, identifier).await?;
        let config = &request.context.config;
        let base = config
            .base_uri()
            .ok()
            .and_then(RemoteUri::parse)
            .ok_or(unsupported("base URIs PhutilURI may read differently"))?;
        let uris = repository_uris(request, &repository, &base).await?;
        let changes = changes(request, &raw)?;
        let (transactions, mut edited, errors) = plan(&row, &uris, changes)?;
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
        // A new protocol or domain removes the credential.
        let uri_changed = transactions
            .iter()
            .any(|planned| planned.kind == "diffusion.uri.uri");
        if uri_changed {
            let (old, new) = (row.effective_uri()?, edited.effective_uri()?);
            if old.protocol != new.protocol || old.domain != new.domain {
                edited.credential = None;
            }
        }
        let saved: Vec<UriRow> = uris
            .iter()
            .map(|uri| {
                if uri.id == edited.id {
                    edited.clone()
                } else {
                    uri.clone()
                }
            })
            .collect();
        let effects = final_effects(config, &repository, &saved, &base)?;

        let database = &request.context.database;
        let publication = Publication::new(database, actor, row.phid.clone(), transactions).await?;
        write(
            database,
            (&publication, &edited),
            (&repository, &effects),
            request.now,
        )
        .await?;
        Ok(publication.response(row.id))
    }
}

impl Method for UriEdit {
    fn name(&self) -> &'static str {
        "diffusion.uri.edit"
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

    fn uri(id: i64, io: &str) -> UriRow {
        UriRow {
            id,
            phid: format!("PHID-RURI-{id}"),
            uri: "https://example.com/x.git".to_owned(),
            builtin_protocol: None,
            builtin_identifier: None,
            io: io.to_owned(),
            display: "default".to_owned(),
            disabled: false,
            credential: None,
        }
    }

    #[test]
    fn checks_io_types_like_php() {
        let edited = uri(1, "default");
        assert_eq!(io_error(&edited, &[uri(2, "mirror")], "observe"), None);
        assert_eq!(io_error(&edited, &[uri(1, "observe")], "observe"), None);
        assert!(
            io_error(&edited, &[uri(2, "observe")], "observe")
                .is_some_and(|error| error.contains("observe two different remotes"))
        );
        assert!(
            io_error(&edited, &[uri(2, "readwrite")], "observe")
                .is_some_and(|error| error.contains("Read/Write IO mode"))
        );
        assert!(
            io_error(&edited, &[], "readwrite")
                .is_some_and(|error| error.contains("default, none, observe, mirror"))
        );
    }
}
