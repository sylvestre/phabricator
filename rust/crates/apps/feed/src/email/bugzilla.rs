//! `BugStore`: the bug of a revision, and its name from Bugzilla's REST API.

use std::time::Duration;

use serde_json::{Value, json};

use super::store::{RevisionRow, Store};
use super::{Fail, Result};

/// What PHP shows when the bug's name can't be fetched.
const FAILED_NAME: &str = "(failed to fetch bug name)";
/// PHP's `default_socket_timeout`.
const TIMEOUT: Duration = Duration::from_secs(60);

/// `bugzilla.url` as `PhutilURI` keeps it once `setPath()` replaced its
/// path: only a scheme and an authority are ported.
fn bugzilla_origin(store: &Store<'_>) -> Result<String> {
    let url = store
        .config
        .string("bugzilla.url")
        .ok_or(Fail::Unsupported("no bugzilla.url"))?;
    let (scheme, rest) = url
        .split_once("://")
        .ok_or(Fail::Unsupported("an unusual bugzilla.url"))?;
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    let simple = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-:".contains(&byte))
    };
    if !matches!(scheme, "http" | "https") || !simple(authority) {
        return Err(Fail::Unsupported("an unusual bugzilla.url"));
    }
    Ok(format!("{scheme}://{authority}"))
}

/// `BugStore::resolveBug()`: the `SecureEmailBug`, `None` without a bug.
pub(crate) async fn resolve_bug(
    store: &Store<'_>,
    revision: &RevisionRow,
) -> Result<Option<(i64, Value)>> {
    let Some(value) = store.bug_field(&revision.phid).await? else {
        return Ok(None);
    };
    if !phab_core::php::is_truthy_str(&value) {
        return Ok(None);
    }
    // intval() of anything but digits is not ported.
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Fail::Unsupported("a bug field which is not a number"));
    }
    let id: i64 = value
        .parse()
        .map_err(|_| Fail::Unsupported("a huge bug number"))?;
    let link = format!("{}/show_bug.cgi?id={id}", bugzilla_origin(store)?);
    Ok(Some((id, json!({"bugId": id, "link": link}))))
}

/// `BugStore::queryName()`: the bug's summary, `None` if the request
/// fails (PHP turns the warning into an exception it catches).
async fn query_name(store: &mut Store<'_>, id: i64) -> Result<Option<String>> {
    let key = id.to_string();
    if let Some(name) = store.bug_names.get(&key) {
        return Ok(Some(name.clone()));
    }
    let api_key = store
        .config
        .string("bugzilla.automation_api_key")
        .ok_or(Fail::Unsupported("no Bugzilla API key"))?
        .to_owned();
    let query: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("include_fields", "summary")
        .append_pair("api_key", &api_key)
        .finish();
    let url = format!("{}/rest/bug/{key}?{query}", bugzilla_origin(store)?);
    if store.http.is_none() {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|error| Fail::Internal(error.to_string()))?;
        store.http = Some(client);
    }
    let client = store.http.clone().expect("the client was just built");
    let response = match client.get(&url).send().await {
        Ok(response) if response.status().as_u16() < 400 => response,
        _ => return Ok(None),
    };
    let Ok(body) = response.bytes().await else {
        return Ok(None);
    };
    // current($json->bugs)->summary: anything else fails in PHP.
    let decoded: Value = serde_json::from_slice(&body)
        .map_err(|_| Fail::Unsupported("an unusual Bugzilla answer"))?;
    let name = decoded
        .get("bugs")
        .and_then(Value::as_array)
        .and_then(|bugs| bugs.first())
        .and_then(|bug| bug.get("summary"))
        .and_then(Value::as_str)
        .ok_or(Fail::Unsupported("an unusual Bugzilla answer"))?
        .to_owned();
    store.bug_names.insert(key, name.clone());
    Ok(Some(name))
}

/// The `EmailBug` of a public revision.
pub(crate) async fn public_bug(store: &mut Store<'_>, revision: &RevisionRow) -> Result<Value> {
    let Some((id, secure)) = resolve_bug(store, revision).await? else {
        return Ok(Value::Null);
    };
    let name = query_name(store, id)
        .await?
        .unwrap_or_else(|| FAILED_NAME.to_owned());
    Ok(json!({"bugId": id, "name": name, "link": secure["link"]}))
}
