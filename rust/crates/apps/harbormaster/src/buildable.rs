//! `harbormaster.buildable.search` (`HarbormasterBuildableSearchEngine`)
//! and the legacy `harbormaster.querybuildables`.
//!
//! Ported: every constraint, the builtin orders and paging, for buildables
//! of diffs and commits. Left to PHP: buildables of other objects, objects
//! and containers named by monograms or of other types, saved queries and
//! order vectors.

use phab_conduit::pager::{Cursor, PageSize, Paging};
use phab_conduit::search::{self, Clauses};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use serde_json::{Map, Value, json};

use crate::constraints::{self, Ids};
use crate::query::{Order, Query};
use crate::storage::{self, Buildable};
use crate::visibility;

const VALID_CONSTRAINTS: &[&str] = &[
    "objectPHIDs",
    "containerPHIDs",
    "statuses",
    "manual",
    "ids",
    "phids",
];

/// The constraints of `HarbormasterBuildableQuery`.
#[derive(Debug, Default)]
struct Filters {
    ids: Option<Vec<i64>>,
    phids: Option<Vec<String>>,
    objects: Option<Vec<String>>,
    containers: Option<Vec<String>>,
    statuses: Option<Vec<String>>,
    manual: Option<i64>,
}

impl Filters {
    /// `buildWhereClauseParts()`.
    fn clauses(&self) -> Clauses {
        let mut clauses = Clauses::default();
        if let Some(ids) = &self.ids {
            clauses.in_list("buildable.id", ids.iter().map(|id| json!(id)));
        }
        constraints::in_list(&mut clauses, "buildable.phid", self.phids.as_ref());
        constraints::in_list(
            &mut clauses,
            "buildable.buildablePHID",
            self.objects.as_ref(),
        );
        constraints::in_list(
            &mut clauses,
            "buildable.containerPHID",
            self.containers.as_ref(),
        );
        constraints::in_list(
            &mut clauses,
            "buildable.buildableStatus",
            self.statuses.as_ref(),
        );
        if let Some(manual) = self.manual {
            clauses.condition("buildable.isManualBuildable = ?", [json!(manual)]);
        }
        clauses
    }
}

/// `resolvePHIDs()`: the PHIDs of the visible objects named, or a bogus
/// value matching nothing.
async fn resolve(request: &ApiRequest<'_>, names: &[String]) -> Result<Vec<String>, MethodError> {
    let phids = visibility::visible_named_objects(request, names).await?;
    if phids.is_empty() {
        return Ok(vec!["-".to_owned()]);
    }
    Ok(phids)
}

/// One buildable: `getFieldValuesForConduit()` and the extensions' fields.
fn result(request: &ApiRequest<'_>, buildable: &Buildable) -> Result<Value, MethodError> {
    let policies = storage::attached(buildable.policies.as_ref())?;
    let mut fields = Map::new();
    fields.insert(
        "objectPHID".to_owned(),
        json!(buildable.object_phid.as_str()),
    );
    fields.insert("containerPHID".to_owned(), json!(buildable.container_phid));
    fields.insert(
        "buildableStatus".to_owned(),
        json!({"value": buildable.status}),
    );
    fields.insert("isManual".to_owned(), json!(buildable.is_manual));
    fields.insert(
        "uri".to_owned(),
        json!(crate::uri(request, &format!("/B{}", buildable.id))?),
    );
    Ok(crate::search_result(
        buildable.id,
        buildable.phid.as_str(),
        fields,
        buildable.dates,
        policies.view_and_edit(),
        Map::new(),
    ))
}

/// `harbormaster.buildable.search`.
pub struct BuildableSearch;

impl Method for BuildableSearch {
    fn name(&self) -> &'static str {
        "harbormaster.buildable.search"
    }

    fn params(&self) -> &'static [&'static str] {
        crate::SEARCH_PARAMS
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let constraints = constraints::parse(request, VALID_CONSTRAINTS)?;
            let strict = request.strictly_typed;
            let objects = constraints::string_list(&constraints, "objectPHIDs")?;
            let containers = constraints::string_list(&constraints, "containerPHIDs")?;
            let statuses = constraints::string_list(&constraints, "statuses")?;
            let manual = constraints::bool(&constraints, "manual", strict)?;
            let ids = Ids::parse(&constraints, strict)?;
            let filters = Filters {
                ids: ids.ids,
                phids: ids.phids,
                objects: match objects {
                    Some(names) => Some(resolve(request, &names).await?),
                    None => None,
                },
                containers: match containers {
                    Some(names) => Some(resolve(request, &names).await?),
                    None => None,
                },
                statuses,
                manual: manual.map(i64::from),
            };
            search::requested_attachments(request.value("attachments"), &[])?;
            let order = Order::parse(request.value("order"), "HarbormasterBuildableQuery", false)?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            let page = Query::<Buildable>::new(request, filters.clauses(), order)
                .page(&paging)
                .await?;
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|buildable| result(request, buildable))
                .collect::<Result<_, _>>()?;
            Ok(search::response(
                request,
                &data,
                &paging,
                &page,
                |buildable| buildable.id.to_string(),
            ))
        })
    }
}

/// A `list<...>` parameter of a legacy method: `withIDs()` and the like
/// take any array, and `null` is no constraint.
fn legacy_list<'v>(
    request: &'v ApiRequest<'_>,
    key: &str,
) -> Result<Option<&'v [Value]>, MethodError> {
    match request.value(key) {
        None | Some(Value::Null) => Ok(None),
        // An empty list makes an empty "IN ()", which qsprintf() rejects.
        Some(Value::Array(list)) if list.is_empty() => Err(MethodError::Unsupported("empty lists")),
        Some(Value::Array(list)) => Ok(Some(list)),
        Some(_) => Err(MethodError::Unsupported("lists which are not lists")),
    }
}

/// `%Ls` of a legacy list.
pub(crate) fn legacy_strings(
    request: &ApiRequest<'_>,
    key: &str,
) -> Result<Option<Vec<String>>, MethodError> {
    legacy_list(request, key)?
        .map(|list| {
            list.iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or(MethodError::Unsupported(
                    "lists of values other than strings",
                ))
        })
        .transpose()
}

/// `%Ld` of a legacy list.
fn legacy_ids(request: &ApiRequest<'_>) -> Result<Option<Vec<i64>>, MethodError> {
    legacy_list(request, "ids")?
        .map(|list| {
            list.iter()
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
                .ok_or(MethodError::Unsupported("IDs which are not integers"))
        })
        .transpose()
}

/// `(int)` of `manualBuildables`, which `isManualBuildable = %d` compares.
fn legacy_manual(value: Option<&Value>) -> Result<Option<i64>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(manual)) => Ok(Some(i64::from(*manual))),
        Some(Value::Number(number)) if !number.is_f64() => number
            .as_i64()
            .map(Some)
            .ok_or(MethodError::Unsupported("huge integers")),
        Some(Value::String(text)) if text.is_empty() => Ok(Some(0)),
        Some(Value::String(text)) if text.bytes().all(|b| b.is_ascii_digit()) => text
            .parse()
            .map(Some)
            .map_err(|_| MethodError::Unsupported("huge integers")),
        Some(_) => Err(MethodError::Unsupported("an unusual manualBuildables")),
    }
}

/// `ConduitAPIMethod::newPager()`: the limit, 100 by default, between 1
/// and 1000, and the cursors.
pub(crate) fn legacy_paging(request: &ApiRequest<'_>) -> Result<Paging, MethodError> {
    let size = match request.value("limit") {
        None | Some(Value::Null) => 100,
        Some(Value::Number(number)) if !number.is_f64() => {
            let limit = number
                .as_i64()
                .ok_or(MethodError::Unsupported("a huge limit"))?;
            usize::try_from(limit.clamp(1, 1000)).map_err(MethodError::internal)?
        }
        Some(_) => return Err(MethodError::Unsupported("an unusual limit")),
    };
    Ok(Paging {
        size: PageSize {
            shown: json!(size),
            value: size,
        },
        after: Cursor::parse(request.value("after"))?,
        before: Cursor::parse(request.value("before"))?,
    })
}

/// `harbormaster.querybuildables`.
pub struct QueryBuildables;

impl Method for QueryBuildables {
    fn name(&self) -> &'static str {
        "harbormaster.querybuildables"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "ids",
            "phids",
            "buildablePHIDs",
            "containerPHIDs",
            "manualBuildables",
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
            let filters = Filters {
                ids: legacy_ids(request)?,
                phids: legacy_strings(request, "phids")?,
                objects: legacy_strings(request, "buildablePHIDs")?,
                containers: legacy_strings(request, "containerPHIDs")?,
                statuses: None,
                manual: legacy_manual(request.value("manualBuildables"))?,
            };
            let paging = legacy_paging(request)?;
            let page = Query::<Buildable>::new(request, filters.clauses(), Order::Newest)
                .page(&paging)
                .await?;
            let production = crate::production_uri(request, "")?;
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|buildable| {
                    let monogram = format!("B{}", buildable.id);
                    json!({
                        // getID() is the string PHP read from the row.
                        "id": buildable.id.to_string(),
                        "phid": buildable.phid.as_str(),
                        "monogram": monogram,
                        "uri": format!("{production}/{monogram}"),
                        "buildableStatus": buildable.status,
                        "buildableStatusName": status_name(&buildable.status),
                        "buildablePHID": buildable.object_phid.as_str(),
                        "containerPHID": buildable.container_phid,
                        "isManualBuildable": buildable.is_manual,
                    })
                })
                .collect();
            let cursor = |buildable: Option<Buildable>| buildable.map(|row| row.id.to_string());
            Ok(json!({
                "data": data,
                "cursor": {
                    "limit": paging.size.value,
                    "after": cursor(page.next),
                    "before": cursor(page.previous),
                },
            }))
        })
    }
}

/// `HarbormasterBuildableStatus::getDisplayName()`.
fn status_name(status: &str) -> String {
    match status {
        "preparing" => "Preparing".to_owned(),
        "building" => "Building".to_owned(),
        "passed" => "Passed".to_owned(),
        "failed" => "Failed".to_owned(),
        other => format!("Unknown (\"{other}\")"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_like_php() {
        assert_eq!(legacy_manual(Some(&json!(true))), Ok(Some(1)));
        assert_eq!(legacy_manual(Some(&json!("0"))), Ok(Some(0)));
        assert_eq!(legacy_manual(Some(&json!(2))), Ok(Some(2)));
        assert_eq!(legacy_manual(None), Ok(None));
        assert!(legacy_manual(Some(&json!("true"))).is_err());
    }

    #[test]
    fn status_names() {
        assert_eq!(status_name("passed"), "Passed");
        assert_eq!(status_name("x"), "Unknown (\"x\")");
    }
}
