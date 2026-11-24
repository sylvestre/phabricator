//! `diffusion.findsymbols` (`DiffusionFindSymbolsConduitAPIMethod`): the
//! symbols of `repository_symbol` (`DiffusionSymbolQuery`) with their
//! paths and repositories, those of repositories the viewer can't see left
//! out.
//!
//! Ported: every parameter. Left to PHP: values which are not strings.

use std::collections::HashMap;
use std::fmt::Write as _;

use phab_app_differential::repository::{self, Repository};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture, pager};
use phab_core::{Phid, php};
use phab_db::rows;
use serde_json::{Value, json};

/// `diffusion.findsymbols`.
pub struct FindSymbols;

/// A string parameter, `None` when it is missing or `null`.
fn optional_string(request: &ApiRequest<'_>, key: &str) -> Result<Option<String>, MethodError> {
    match request.value(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(MethodError::Unsupported("parameters which are not strings")),
    }
}

/// A setting `DiffusionSymbolQuery` tests for truthiness.
fn truthy(value: Option<&String>) -> Option<&str> {
    value
        .map(String::as_str)
        .filter(|text| php::is_truthy_str(text))
}

/// `PhabricatorRepositorySymbol::getURI()` of a symbol in `repository`:
/// `DiffusionRequest::generateURI()` of its browse page on the default
/// branch, as a production URI.
fn symbol_uri(
    request: &ApiRequest<'_>,
    repository: &Repository,
    path: Option<&str>,
    line: &str,
) -> Result<String, MethodError> {
    let mut blob = String::new();
    if let Some(path) = path.filter(|path| !path.is_empty()) {
        let path = path
            .trim_start_matches('/')
            .replace(';', ";;")
            .replace('$', "$$");
        blob = escape_uri(&path);
    }
    if let Some(branch) = repository.default_branch() {
        blob = format!("{}/{blob}", raw_encode(&raw_encode(&branch)));
    }
    let path = format!("{}browse/{blob}${}", repository.uri(), escape_uri(line));
    let config = &request.context.config;
    let base = match config.nonempty_string("phabricator.production-uri") {
        Some(uri) => uri,
        None => config.base_uri().map_err(MethodError::internal)?,
    };
    Ok(format!("{}{path}", base.trim_end_matches('/')))
}

/// `rawurlencode()`.
fn raw_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// `phutil_escape_uri()`: `rawurlencode()` keeping slashes.
fn escape_uri(text: &str) -> String {
    raw_encode(text).replace("%2F", "/")
}

/// `loadPaths()`: the paths of these IDs, by ID.
async fn load_paths(
    database: &phab_db::Database,
    path_ids: &[i64],
) -> Result<HashMap<i64, String>, MethodError> {
    let sql = format!(
        "SELECT id, path FROM {} WHERE id IN ({})",
        database.table("repository", "repository_path"),
        phab_db::placeholders(path_ids.len()),
    );
    Ok(
        phab_db::bind_all(phab_db::sqlx::query(&sql), path_ids.iter())
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(|row| Ok((rows::int(row, "id")?, rows::text(row, "path")?)))
            .collect::<Result<_, phab_db::DbError>>()?,
    )
}

/// `loadRepositories()`: the repositories of these PHIDs the viewer can
/// see, by PHID.
async fn load_repositories(
    request: &ApiRequest<'_>,
    phids: &[Phid],
) -> Result<HashMap<String, Repository>, MethodError> {
    let context = request.context;
    let found = repository::load(&context.database, phids).await?;
    Ok(repository::visible(
        &context.database,
        &context.config,
        &request.viewer,
        found,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?
    .into_iter()
    .map(|repository| (repository.phid.as_str().to_owned(), repository))
    .collect())
}

impl FindSymbols {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let name = optional_string(request, "name")?;
        let prefix = optional_string(request, "namePrefix")?;
        let context = optional_string(request, "context")?;
        let language = optional_string(request, "language")?;
        let kind = optional_string(request, "type")?;
        let repository_phid = optional_string(request, "repositoryPHID")?;
        let (name, prefix) = (truthy(name.as_ref()), truthy(prefix.as_ref()));
        match (name, prefix) {
            (Some(_), Some(_)) => {
                return Err(pager::core_error(
                    "You can not set both a name and a name prefix!".to_owned(),
                ));
            }
            (None, None) => {
                return Err(pager::core_error(
                    "You must set a name or a name prefix!".to_owned(),
                ));
            }
            _ => {}
        }

        let mut conditions = Vec::new();
        let mut values = Vec::new();
        if let Some(context) = context {
            conditions.push("symbolContext = ?");
            values.push(context);
        }
        if let Some(name) = name {
            conditions.push("symbolName = ?");
            values.push(name.to_owned());
        }
        if let Some(prefix) = prefix {
            conditions.push("symbolName LIKE ?");
            values.push(format!("{}%", phab_db::like_escape(prefix)));
        }
        if let Some(phid) = repository_phid {
            conditions.push("repositoryPHID IN (?)");
            values.push(phid);
        }
        if let Some(language) = truthy(language.as_ref()) {
            conditions.push("symbolLanguage = ?");
            values.push(language.to_owned());
        }
        if let Some(kind) = truthy(kind.as_ref()) {
            conditions.push("symbolType = ?");
            values.push(kind.to_owned());
        }
        let database = &request.context.database;
        let sql = format!(
            "SELECT repositoryPHID, symbolContext, symbolName, symbolType, symbolLanguage, \
             pathID, lineNumber FROM {} WHERE {} ORDER BY symbolName ASC",
            database.table("repository", "repository_symbol"),
            conditions
                .iter()
                .map(|condition| format!("({condition})"))
                .collect::<Vec<_>>()
                .join(" AND "),
        );
        let symbols = phab_db::bind_all(phab_db::sqlx::query(&sql), values.iter())
            .fetch_all(database.pool())
            .await?;
        if symbols.is_empty() {
            return Ok(json!([]));
        }

        let path_ids = symbols
            .iter()
            .map(|symbol| rows::int(symbol, "pathID"))
            .collect::<Result<Vec<_>, _>>()?;
        let paths = load_paths(database, &path_ids).await?;

        let mut repository_phids = symbols
            .iter()
            .map(|symbol| rows::text(symbol, "repositoryPHID").map(Phid::new))
            .collect::<Result<Vec<_>, _>>()?;
        repository_phids.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        repository_phids.dedup();
        let repositories = load_repositories(request, &repository_phids).await?;

        let mut results = Vec::new();
        for (symbol, path_id) in symbols.iter().zip(path_ids) {
            let Some(repository) = repositories.get(&rows::text(symbol, "repositoryPHID")?) else {
                continue;
            };
            let path = paths.get(&path_id);
            let line = rows::int(symbol, "lineNumber")?.to_string();
            let uri = symbol_uri(request, repository, path.map(String::as_str), &line)?;
            results.push(json!({
                "name": rows::text(symbol, "symbolName")?,
                "context": rows::text(symbol, "symbolContext")?,
                "type": rows::text(symbol, "symbolType")?,
                "language": rows::text(symbol, "symbolLanguage")?,
                "path": path,
                "line": line,
                "uri": uri,
                "repositoryPHID": repository.phid.as_str(),
            }));
        }
        Ok(Value::Array(results))
    }
}

impl Method for FindSymbols {
    fn name(&self) -> &'static str {
        "diffusion.findsymbols"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "name",
            "namePrefix",
            "context",
            "language",
            "type",
            "repositoryPHID",
        ]
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
    fn escapes_like_php() {
        assert_eq!(escape_uri("a b/c;d"), "a%20b/c%3Bd");
        assert_eq!(raw_encode(&raw_encode("a/b")), "a%252Fb");
    }
}
