//! `repository.query` (`RepositoryQueryConduitAPIMethod`): the frozen
//! repository query, through `diffusion.repository.search`'s query in its
//! default order.
//!
//! Ported: `ids`, `phids`, `callsigns`, `vcsTypes`, `uuids`, and what
//! `newQueryForRequest()` adds: `order` (by ID by default, as
//! `getDefaultOrderVector()` says), `limit` and the cursors. Left to PHP:
//! `remoteURIs` (normalized URIs), order vectors and remote URIs which
//! `PhutilURI` would rewrite.

use phab_app_differential::repository::Repository;
use phab_conduit::pager::{self, Cursor, PageSize, Paging};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::php;
use serde_json::{Value, json};

use crate::repository_search::{Filters, Order, Repositories};

/// `ConduitAPIMethod::newPager()`: the limit, 100 by default, between 1
/// and 1000.
fn page_size(value: Option<&Value>) -> Result<usize, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(100),
        Some(Value::Number(number)) if !number.is_f64() => {
            let limit = number
                .as_i64()
                .ok_or(MethodError::Unsupported("a huge limit"))?;
            usize::try_from(limit.clamp(1, 1000)).map_err(MethodError::internal)
        }
        Some(_) => Err(MethodError::Unsupported("an unusual limit")),
    }
}

/// A `list<string>` parameter; falsy is no constraint.
fn string_list(request: &ApiRequest<'_>, key: &str) -> Result<Option<Vec<String>>, MethodError> {
    match request.value(key) {
        Some(value) if !php::is_truthy(value) => Ok(None),
        None => Ok(None),
        Some(Value::Array(list)) => list
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or(MethodError::Unsupported(
                "lists of values other than strings",
            )),
        Some(_) => Err(MethodError::Unsupported("lists which are not lists")),
    }
}

/// A `list<int>` parameter; falsy is no constraint.
fn id_list(request: &ApiRequest<'_>) -> Result<Option<Vec<i64>>, MethodError> {
    match request.value("ids") {
        Some(value) if !php::is_truthy(value) => Ok(None),
        None => Ok(None),
        Some(Value::Array(list)) => list
            .iter()
            .map(|id| match id {
                Value::Number(number) if !number.is_f64() => number.as_i64(),
                Value::String(text)
                    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    text.parse().ok()
                }
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or(MethodError::Unsupported("IDs which are not integers")),
        Some(_) => Err(MethodError::Unsupported("lists which are not lists")),
    }
}

/// Whether `PhutilURI` keeps `uri` as it is: a scheme, maybe a user whom
/// `rawurlencode()` keeps, a host, maybe a port and a plain path, without
/// password, query or fragment.
fn round_trips(uri: &str) -> bool {
    let Some((scheme, rest)) = uri.split_once("://") else {
        return false;
    };
    let mut scheme_chars = scheme.chars();
    if !scheme_chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        || !scheme_chars.all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
    {
        return false;
    }
    let (authority, path) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
    let plain_path = path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._~+-".contains(c));
    let authority = match authority.split_once('@') {
        Some((user, host)) => {
            if user.is_empty()
                || host.is_empty()
                || !user
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._~-".contains(c))
            {
                return false;
            }
            host
        }
        None => authority,
    };
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    let host_ok = host.is_empty()
        || (host.starts_with(|c: char| c.is_ascii_alphanumeric())
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c)));
    let port_ok = port.is_none_or(|port| {
        !host.is_empty() && !port.starts_with('0') && port.parse::<u16>().is_ok_and(|port| port > 0)
    });
    plain_path && host_ok && port_ok && (!host.is_empty() || !path.is_empty())
}

/// `getRemoteURI()`: `(string)` of the `PhutilURI` of the `remote-uri`
/// detail, a local path becoming a `file://` URI.
fn remote_uri(repository: &Repository) -> Result<String, MethodError> {
    let raw = match repository.detail("remote-uri") {
        None | Some(Value::Null) => return Ok(String::new()),
        Some(Value::String(raw)) if raw.is_empty() => return Ok(String::new()),
        Some(Value::String(raw)) => raw,
        Some(_) => return Err(MethodError::Unsupported("an unusual remote URI")),
    };
    let uri = if raw.starts_with('/') {
        format!("file://{raw}")
    } else {
        raw.clone()
    };
    if round_trips(&uri) {
        Ok(uri)
    } else {
        Err(MethodError::Unsupported("remote URIs PhutilURI rewrites"))
    }
}

/// `toDictionary()`.
fn dictionary(repository: &Repository, production_uri: &str) -> Result<Value, MethodError> {
    let detail = |key: &str| repository.detail(key).cloned();
    let truthy = |key: &str| repository.detail(key).is_some_and(php::is_truthy);
    let is_git = repository.vcs == "git";
    Ok(json!({
        // Lisk keeps MySQL's strings.
        "id": repository.id.to_string(),
        "name": repository.name,
        "phid": repository.phid.as_str(),
        "callsign": repository.callsign,
        "monogram": repository.monogram(),
        "vcs": repository.vcs,
        "uri": format!("{}{}", production_uri.trim_end_matches('/'), repository.uri()),
        "remoteURI": remote_uri(repository)?,
        "description": detail("description"),
        "isActive": repository.is_tracked(),
        "isHosted": truthy("hosting-enabled"),
        "isImporting": truthy("importing"),
        "encoding": repository.detail("encoding").cloned().unwrap_or_else(|| json!("UTF-8")),
        "staging": {
            "supported": is_git,
            "prefix": "phabricator",
            "uri": if is_git { detail("staging-uri") } else { None },
        },
    }))
}

/// `repository.query`.
pub struct LegacyQuery;

impl Method for LegacyQuery {
    fn name(&self) -> &'static str {
        "repository.query"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "ids",
            "phids",
            "callsigns",
            "vcsTypes",
            "remoteURIs",
            "uuids",
            "order",
            "before",
            "after",
            "limit",
        ]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            if string_list(request, "remoteURIs")?.is_some() {
                return Err(MethodError::Unsupported("remote URI constraints"));
            }
            let filters = Filters {
                ids: id_list(request)?,
                phids: string_list(request, "phids")?,
                callsigns: string_list(request, "callsigns")?,
                types: string_list(request, "vcsTypes")?,
                uuids: string_list(request, "uuids")?,
                ..Filters::default()
            };
            let order = match request.value("order") {
                None | Some(Value::Null) => Order::Newest,
                order => Order::parse(order)?,
            };
            let size = page_size(request.value("limit"))?;
            let paging = Paging {
                size: PageSize {
                    shown: json!(size),
                    value: size,
                },
                after: Cursor::parse(request.value("after"))?,
                before: Cursor::parse(request.value("before"))?,
            };
            let query = Repositories {
                request,
                filters,
                order,
            };
            let (start, direction) = paging.start();
            let found = pager::execute(&query, start, direction, size + 1).await?;
            let page = paging.pager().slice(found);
            let config = &request.context.config;
            let production_uri = match config.nonempty_string("phabricator.production-uri") {
                Some(uri) => uri.to_owned(),
                None => config
                    .base_uri()
                    .map(str::to_owned)
                    .map_err(MethodError::internal)?,
            };
            page.items
                .iter()
                .map(|repository| dictionary(repository, &production_uri))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_plain_uris_only() {
        assert!(round_trips("https://hg.mozilla.org/mozilla-central"));
        assert!(round_trips("ssh://git.example.com:2222/repo.git"));
        assert!(round_trips("file:///repo/1"));
        assert!(round_trips("svn+ssh://user@svn.example.com/repo"));
        assert!(!round_trips("https://user:pass@example.com/repo"));
        assert!(!round_trips("https://us%20er@example.com/repo"));
        assert!(!round_trips("https://@example.com/repo"));
        assert!(!round_trips("https://example.com/repo?x=1"));
        assert!(!round_trips("git@github.com:mozilla/repo.git"));
        assert!(!round_trips("https://example.com:080/repo"));
        assert!(!round_trips("https://-bad/repo"));
    }
}
