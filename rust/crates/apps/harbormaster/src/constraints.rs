//! The search fields of the Harbormaster engines, read from `constraints`
//! like `PhabricatorSearchField::readValueFromConduitRequest()` then
//! `getValueForQuery()`.

use phab_conduit::search::{self, Clauses, Constraints};
use phab_conduit::{ApiRequest, MethodError};
use serde_json::{Value, json};

/// The values of a constraint read with `read`, or `None` when absent.
fn read<T>(
    constraints: &Constraints,
    key: &str,
    read: impl FnOnce(&str, &Value) -> Result<T, MethodError>,
) -> Result<Option<T>, MethodError> {
    constraints
        .get(&[key])
        .map(|(key, value)| read(key, value))
        .transpose()
}

/// `PhabricatorSearchStringListField` and `PhabricatorSearchCheckboxesField`
/// (`ConduitStringListParameterType`): an empty list is no constraint.
pub(crate) fn string_list(
    constraints: &Constraints,
    key: &str,
) -> Result<Option<Vec<String>>, MethodError> {
    Ok(read(constraints, key, |key, value| {
        search::string_list(key, value, true)
    })?
    .filter(|list| !list.is_empty()))
}

/// `PhabricatorPHIDsSearchField` (`ConduitPHIDListParameterType`, which
/// takes no empty list).
pub(crate) fn phid_list(
    constraints: &Constraints,
    key: &str,
) -> Result<Option<Vec<String>>, MethodError> {
    read(constraints, key, |key, value| {
        search::string_list(key, value, false)
    })
}

/// `PhabricatorSearchDatasourceField`: a nonempty string list, which
/// `evaluateTokens()` passes through unless tokens are functions.
pub(crate) fn datasource(
    constraints: &Constraints,
    key: &str,
) -> Result<Option<Vec<String>>, MethodError> {
    let tokens = read(constraints, key, |key, value| {
        search::string_list(key, value, false)
    })?;
    if tokens.iter().flatten().any(|token| token.contains('(')) {
        return Err(MethodError::Unsupported("typeahead functions"));
    }
    Ok(tokens)
}

/// `PhabricatorSearchThreeStateField` (`ConduitBoolParameterType`).
pub(crate) fn bool(
    constraints: &Constraints,
    key: &str,
    strict: bool,
) -> Result<Option<bool>, MethodError> {
    read(constraints, key, |key, value| {
        search::bool(key, value, strict)
    })
}

/// `PhabricatorSearchDateField` (`ConduitEpochParameterType`): epochs
/// above 30000000 are taken as they are; smaller ones go through
/// `PhabricatorTime::parseLocalTime()`, which is left to PHP.
pub(crate) fn date(
    constraints: &Constraints,
    key: &str,
    strict: bool,
) -> Result<Option<i64>, MethodError> {
    let Some(epoch) = read(constraints, key, |key, value| {
        search::epoch(key, value, strict)
    })?
    else {
        return Ok(None);
    };
    if epoch <= 30_000_000 {
        return Err(MethodError::Unsupported("dates which look like Ymd"));
    }
    Ok(Some(epoch))
}

/// `PhabricatorSearchTextField` (`ConduitStringParameterType`).
pub(crate) fn text(constraints: &Constraints, key: &str) -> Result<Option<String>, MethodError> {
    read(constraints, key, search::string)
}

/// The `ids` and `phids` of `PhabricatorIDsSearchEngineExtension`, which
/// come after the engine's own fields.
pub(crate) struct Ids {
    pub(crate) ids: Option<Vec<i64>>,
    pub(crate) phids: Option<Vec<String>>,
}

impl Ids {
    pub(crate) fn parse(constraints: &Constraints, strict: bool) -> Result<Self, MethodError> {
        Ok(Self {
            ids: read(constraints, "ids", |key, value| {
                search::int_list(key, value, strict)
            })?,
            phids: phid_list(constraints, "phids")?,
        })
    }

    /// `withIDs()` and `withPHIDs()`, on columns of `alias`.
    pub(crate) fn add_conditions(&self, clauses: &mut Clauses, alias: &str) {
        if let Some(ids) = &self.ids {
            clauses.in_list(&format!("{alias}.id"), ids.iter().map(|id| json!(id)));
        }
        if let Some(phids) = &self.phids {
            clauses.in_list(&format!("{alias}.phid"), search::strings(phids));
        }
    }
}

/// Adds `alias.column IN (...)` when `values` is a constraint.
pub(crate) fn in_list(clauses: &mut Clauses, column: &str, values: Option<&Vec<String>>) {
    if let Some(values) = values {
        clauses.in_list(column, search::strings(values));
    }
}

/// What every search reads before its constraints: the query key, then
/// the `constraints` map, which may only use `valid` keys.
pub(crate) fn parse(request: &ApiRequest<'_>, valid: &[&str]) -> Result<Constraints, MethodError> {
    search::check_query_key(request)?;
    Constraints::parse(request.value("constraints"), valid)
}
