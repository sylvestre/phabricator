//! `BugzillaAccountSearchConduitAPIMethod` and
//! `BMOExternalAccountSearchConduitAPIMethod` (moz-extensions): the BMO user
//! ids of Phabricator users, and back.

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::php;
use serde_json::{Value, json};

use crate::APPLICATION;
use crate::external_account::{self, AccountFilter};

/// `bugzilla.account.search`.
pub struct BugzillaAccountSearch;

/// `bmoexternalaccount.search`.
pub struct BmoExternalAccountSearch;

/// A `list<string>` parameter as PHP's query methods accept it: an array
/// of strings or integers, which `%Ls` and `digestForIndex()` treat as
/// strings. Anything else makes PHP fail.
fn string_list(value: Option<&Value>) -> Result<Vec<String>, MethodError> {
    let items: Vec<&Value> = match value {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(entries)) => entries.values().collect(),
        _ => return Err(MethodError::Unsupported("account search without a list")),
    };
    if items.is_empty() {
        // qsprintf() refuses empty lists.
        return Err(MethodError::Unsupported(
            "account search with an empty list",
        ));
    }
    items
        .into_iter()
        .map(|item| match item {
            Value::String(text) => Ok(text.clone()),
            Value::Number(number) if !number.is_f64() => Ok(number.to_string()),
            _ => Err(MethodError::Unsupported(
                "account search with an unusual value",
            )),
        })
        .collect()
}

/// The shared body of both methods.
async fn search(request: &ApiRequest<'_>, filter: AccountFilter<'_>) -> Result<Value, MethodError> {
    if !request.viewer.is_logged_in() {
        return Err(MethodError::Unsupported("account search without a user"));
    }
    let database = &request.context.database;
    let Some(provider) = external_account::bmo_provider_config(database).await? else {
        return Err(MethodError::Unsupported(
            "account search needs exactly one BMO provider",
        ));
    };
    let accounts = external_account::load_accounts(database, &provider, filter).await?;
    let identifiers = external_account::first_identifiers(database, &accounts).await?;

    let mut results = Vec::with_capacity(accounts.len());
    for account in &accounts {
        // head() of no identifiers is false, and PHP fails calling a method
        // on it.
        let Some(bmo_id) = identifiers.get(&account.phid) else {
            return Err(MethodError::Unsupported(
                "external account without an identifier",
            ));
        };
        if !php::is_truthy_str(bmo_id) {
            continue;
        }
        results.push(json!({"id": bmo_id, "phid": account.user_phid}));
    }
    Ok(Value::Array(results))
}

impl Method for BugzillaAccountSearch {
    fn name(&self) -> &'static str {
        "bugzilla.account.search"
    }

    fn params(&self) -> &'static [&'static str] {
        &["ids", "phids"]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let ids = request.value("ids").filter(|ids| php::is_truthy(ids));
            let phids = request.value("phids").filter(|phids| php::is_truthy(phids));
            match (ids, phids) {
                (None, None) => Ok(Value::Array(Vec::new())),
                (Some(ids), _) => {
                    let ids = string_list(Some(ids))?;
                    search(request, AccountFilter::RawIdentifiers(&ids)).await
                }
                (None, Some(phids)) => {
                    let phids = string_list(Some(phids))?;
                    search(request, AccountFilter::UserPhids(&phids)).await
                }
            }
        })
    }
}

impl Method for BmoExternalAccountSearch {
    fn name(&self) -> &'static str {
        "bmoexternalaccount.search"
    }

    fn params(&self) -> &'static [&'static str] {
        &["accountids"]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let ids = string_list(request.value("accountids"))?;
            search(request, AccountFilter::RawIdentifiers(&ids)).await
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_lists_like_php() {
        assert_eq!(
            string_list(Some(&json!(["12", 34]))),
            Ok(vec!["12".to_owned(), "34".to_owned()])
        );
        assert_eq!(
            string_list(Some(&json!({"a": "5"}))),
            Ok(vec!["5".to_owned()])
        );
        for unusual in [
            json!(null),
            json!("12"),
            json!([]),
            json!([1.5]),
            json!([null]),
        ] {
            assert!(
                string_list(Some(&unusual)).is_err(),
                "PHP fails on {unusual}, so Rust must leave it to PHP"
            );
        }
        assert!(string_list(None).is_err());
    }
}
