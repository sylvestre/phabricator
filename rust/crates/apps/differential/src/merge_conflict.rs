//! `DifferentialMergeConflictStatusField` (moz-extensions): whether a
//! revision's active diff merges cleanly into its target branch, as
//! `RevisionMergeConflictWorker` stored it, and how Conduit
//! (`getConduitDictionaryValue()`), the revision page
//! (`renderPropertyViewValue()`) and the search engine's revision lists
//! (`loadConflictedRevisionPHIDs()`, Bug 2094527) show it.
//!
//! Values of types PHP would coerce or choke on (a number where a string
//! is expected, say) are left to PHP (`MethodError::Unsupported`).

use std::collections::{HashMap, HashSet};

use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Config, Phid, php};
use phab_db::{Database, rows};
use serde_json::{Map, Value};

use crate::repository::{self, Repository};
use crate::storage::{self, Revision};
use crate::{diff_search, search};

/// `getFieldKey()`.
pub const FIELD_KEY: &str = "differential:merge-conflict-status";

/// `getFieldKeyForConduit()`.
pub const CONDUIT_KEY: &str = "merge.conflict.status";

/// `MergeConflictConfigOptions::OPTION_ENABLED`.
const OPTION_ENABLED: &str = "merge.conflict.enabled";

/// `MergeConflictConfigOptions::OPTION_REPOSITORIES`.
const OPTION_REPOSITORIES: &str = "merge.conflict.repository-phids";

// The keys of the stored payload.
const KEY_STATUS: &str = "status";
const KEY_REASON: &str = "reason";
const KEY_REASON_CODE: &str = "reasonCode";
const KEY_BASE_DIFF_CREATION_METHOD: &str = "baseDiffCreationMethod";
const KEY_BASE_DIFF_SOURCE_CONTROL_SYSTEM: &str = "baseDiffSourceControlSystem";
const KEY_BASE_COMMIT: &str = "checkedAgainstBaseCommit";
const KEY_BASE_REVISION_PHID: &str = "checkedAgainstBaseRevisionPHID";
const KEY_DIFF_PHID: &str = "checkedAgainstDiffPHID";
const KEY_DIFF_ID: &str = "checkedAgainstDiffID";
const KEY_STACK_DIFF_PHIDS: &str = "checkedAgainstStackDiffPHIDs";
const KEY_EPOCH: &str = "epoch";

// Derived for the Conduit payload only.
const KEY_IS_STALE: &str = "isStale";
const KEY_HINT: &str = "hint";

/// `newRedactedValue()`'s reason.
const REDACTED_REASON: &str = "This revision lands on top of revisions you do not have \
     permission to see, so what the check ran against is not shown.";

const UNUSUAL: MethodError = MethodError::Unsupported("a merge conflict status of an unusual type");

/// `isFieldEnabled()`: without it, the field doesn't exist.
pub fn is_field_enabled(config: &Config) -> bool {
    config.truthy(OPTION_ENABLED)
}

/// `RevisionMergeConflictWorker::isEnabledForRepository()`.
pub fn is_enabled_for_repository(
    config: &Config,
    repository: &Repository,
) -> Result<bool, MethodError> {
    if !config.truthy(OPTION_ENABLED) || repository.vcs != "git" {
        return Ok(false);
    }
    let Some(allowed) = config
        .value(OPTION_REPOSITORIES)
        .filter(|allowed| php::is_truthy(allowed))
    else {
        return Ok(false);
    };
    // in_array($phid, $allowed, true).
    let phid = repository.phid.as_str();
    let is_phid = |value: &Value| value.as_str() == Some(phid);
    match allowed {
        Value::Array(list) => Ok(list.iter().any(is_phid)),
        Value::Object(map) => Ok(map.values().any(is_phid)),
        _ => Err(MethodError::Unsupported(
            "an unusual merge.conflict.repository-phids",
        )),
    }
}

/// `setValueFromStorage()`: the decoded array, anything else (no row,
/// invalid JSON, a scalar) being an empty one. A list's keys are its
/// indexes, as in PHP; nested values are as `json_encode()` gives them
/// back.
pub fn decode(stored: Option<&str>) -> Map<String, Value> {
    match stored.and_then(|stored| serde_json::from_str::<Value>(stored).ok()) {
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| (key.clone(), php::round_trip(value)))
            .collect(),
        Some(Value::Array(list)) => list
            .iter()
            .enumerate()
            .map(|(index, value)| (index.to_string(), php::round_trip(value)))
            .collect(),
        _ => Map::new(),
    }
}

/// `idx($value, $key)` passed as a `?string` parameter.
fn optional_string<'v>(
    value: &'v Map<String, Value>,
    key: &str,
) -> Result<Option<&'v str>, MethodError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(UNUSUAL),
    }
}

/// `idx($value, $key)` through `phutil_nonempty_string()`, which throws
/// on anything but a string or `null`.
fn nonempty_string<'v>(
    value: &'v Map<String, Value>,
    key: &str,
) -> Result<Option<&'v str>, MethodError> {
    Ok(optional_string(value, key)?.filter(|text| !text.is_empty()))
}

/// The stored stack's diff PHIDs, or `None` when it isn't an array.
fn stack_phids(value: &Map<String, Value>) -> Result<Option<Vec<&str>>, MethodError> {
    let items: Vec<&Value> = match value.get(KEY_STACK_DIFF_PHIDS) {
        Some(Value::Array(list)) => list.iter().collect(),
        Some(Value::Object(map)) => map.values().collect(),
        _ => return Ok(None),
    };
    items
        .into_iter()
        .map(|item| item.as_str().ok_or(UNUSUAL))
        .collect::<Result<_, _>>()
        .map(Some)
}

/// `isCheckedDiffStale()`: either one unknown is fresh.
pub fn is_checked_diff_stale(checked: Option<&str>, active: Option<&str>) -> bool {
    match (checked, active) {
        (Some(checked), Some(active)) if !checked.is_empty() && !active.is_empty() => {
            checked != active
        }
        _ => false,
    }
}

/// `isStatusStale()`.
async fn is_status_stale(
    database: &Database,
    revision: &Revision,
    value: &Map<String, Value>,
) -> Result<bool, MethodError> {
    let checked = optional_string(value, KEY_DIFF_PHID)?;
    let active = revision.active_diff_phid.as_ref().map(Phid::as_str);
    if is_checked_diff_stale(checked, active) {
        return Ok(true);
    }
    is_checked_stack_stale(database, value).await
}

/// `isCheckedStackStale()`: whether a diff of the checked stack is no
/// longer its revision's active diff, with the two queries PHP runs as
/// the omnipotent viewer.
async fn is_checked_stack_stale(
    database: &Database,
    value: &Map<String, Value>,
) -> Result<bool, MethodError> {
    let Some(checked) = stack_phids(value)? else {
        return Ok(false);
    };
    if checked.len() < 2 {
        return Ok(false);
    }
    let mut distinct = checked.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let sql = format!(
        "SELECT phid, revisionID FROM {} WHERE phid IN ({})",
        database.table("differential", "differential_diff"),
        phab_db::placeholders(distinct.len())
    );
    let diffs: Vec<(String, Option<i64>)> =
        phab_db::bind_all(phab_db::sqlx::query(&sql), distinct.iter().copied())
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(|row| {
                Ok((
                    rows::text(row, "phid")?,
                    rows::opt_int(row, "revisionID")?.filter(|id| *id != 0),
                ))
            })
            .collect::<Result<_, phab_db::DbError>>()?;
    let mut revision_ids: Vec<i64> = diffs.iter().filter_map(|(_, id)| *id).collect();
    revision_ids.sort_unstable();
    revision_ids.dedup();
    // The active diff PHIDs of the revisions, by ID.
    let mut active: HashMap<i64, Option<String>> = HashMap::new();
    if !revision_ids.is_empty() {
        let sql = format!(
            "SELECT id, activeDiffPHID FROM {} WHERE id IN ({})",
            database.table("differential", "differential_revision"),
            phab_db::placeholders(revision_ids.len())
        );
        for row in phab_db::bind_all(phab_db::sqlx::query(&sql), revision_ids.iter().copied())
            .fetch_all(database.pool())
            .await?
        {
            active.insert(
                rows::int(&row, "id")?,
                rows::opt_text(&row, "activeDiffPHID")?,
            );
        }
    }
    // DifferentialDiffQuery::willFilterPage() drops the diffs whose
    // revision is gone.
    let diffs: Vec<(String, Option<i64>)> = diffs
        .into_iter()
        .filter(|(_, id)| id.is_none_or(|id| active.contains_key(&id)))
        .collect();
    if diffs.len() != checked.len() || diffs.iter().any(|(_, id)| id.is_none()) {
        return Ok(true);
    }
    let current: HashMap<&str, Option<&str>> = diffs
        .iter()
        .map(|(phid, id)| {
            let current = id.and_then(|id| active.get(&id)).and_then(Option::as_deref);
            (phid.as_str(), current)
        })
        .collect();
    // isCheckedStackStaleForDiffs().
    Ok(checked.iter().any(|phid| {
        current
            .get(phid)
            .copied()
            .flatten()
            .is_none_or(|current| current.is_empty() || current != *phid)
    }))
}

/// `isCheckedStackVisible()`: whether the viewer sees the diffs of the
/// checked stack but the revision's own, and the base revision.
async fn is_checked_stack_visible(
    request: &ApiRequest<'_>,
    value: &Map<String, Value>,
) -> Result<bool, MethodError> {
    // newCheckedDependencyPHIDs(): array_diff() compares strings, and
    // null is "".
    let own = optional_string(value, KEY_DIFF_PHID)?.unwrap_or("");
    let diff_phids: Vec<&str> = stack_phids(value)?
        .unwrap_or_default()
        .into_iter()
        .filter(|phid| *phid != own)
        .collect();
    let base_revision = nonempty_string(value, KEY_BASE_REVISION_PHID)?;
    if !diff_phids.is_empty() {
        let mut distinct: Vec<Phid> = diff_phids.iter().map(|phid| Phid::new(*phid)).collect();
        distinct.sort();
        distinct.dedup();
        let visible = diff_search::visible_diffs(request, &distinct).await?;
        if visible.len() != diff_phids.len() {
            return Ok(false);
        }
    }
    if let Some(phid) = base_revision {
        let context = request.context;
        let found = storage::load_revisions(&context.database, &[Phid::new(phid)]).await?;
        let visible = storage::visible_revisions(
            &context.database,
            &context.config,
            &request.viewer,
            found,
            request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        if visible.len() != 1 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `newViewerSafeValue()`: `newRedactedValue()` unless the viewer sees
/// what the verdict depended on.
async fn viewer_safe(
    request: &ApiRequest<'_>,
    value: &mut Map<String, Value>,
) -> Result<(), MethodError> {
    if !is_checked_stack_visible(request, value).await? {
        redact(value);
    }
    Ok(())
}

/// `newRedactedValue()`.
fn redact(value: &mut Map<String, Value>) {
    value.insert(
        KEY_REASON.to_owned(),
        Value::String(REDACTED_REASON.to_owned()),
    );
    for key in [
        KEY_REASON_CODE,
        KEY_BASE_DIFF_CREATION_METHOD,
        KEY_BASE_DIFF_SOURCE_CONTROL_SYSTEM,
    ] {
        value.insert(key.to_owned(), Value::Null);
    }
}

/// `newHintForValue()`: none for a stale verdict.
fn hint_for_value(
    value: &Map<String, Value>,
    is_stale: bool,
) -> Result<Option<String>, MethodError> {
    if is_stale {
        return Ok(None);
    }
    Ok(new_hint(
        optional_string(value, KEY_REASON_CODE)?,
        optional_string(value, KEY_BASE_DIFF_CREATION_METHOD)?,
        optional_string(value, KEY_BASE_DIFF_SOURCE_CONTROL_SYSTEM)?,
    ))
}

/// `newHint()`: what to do about the cause a check named.
pub fn new_hint(
    reason_code: Option<&str>,
    creation_method: Option<&str>,
    source_control_system: Option<&str>,
) -> Option<String> {
    let hint = match reason_code? {
        "base-missing" => return Some(base_missing_hint(creation_method, source_control_system)),
        "base-not-ancestor" => {
            "Rebasing onto the target branch and updating the revision gives the check a base \
             it can merge from."
        }
        "patch-does-not-apply" => {
            "The stack no longer applies to the commit it records as its base. Rebasing the \
             stack and updating its revisions will fix this."
        }
        "parent-landed" => {
            "Updating this revision on top of the current target branch drops the landed parent \
             from the stack."
        }
        "parent-abandoned" => {
            "Removing the dependency on the abandoned revision, or reparenting this revision \
             onto an open one, lets the stack be checked."
        }
        "parent-other-repository" => {
            "A stack has to land into a single repository. Removing the cross-repository \
             dependency lets the check run."
        }
        "stack-not-linear" => {
            "Merge checks follow one parent at a time. Editing the revision so it depends on a \
             single parent lets the stack be checked."
        }
        "patch-too-large" => {
            "Splitting the change into smaller revisions brings the stack under the size the \
             check can handle."
        }
        "command-failed" | "internal-error" => {
            "This is a fault on the Phabricator side rather than a problem with the revision. \
             The details are in the daemon log."
        }
        _ => return None,
    };
    Some(hint.to_owned())
}

/// `newBaseMissingHint()`.
fn base_missing_hint(creation_method: Option<&str>, source_control_system: Option<&str>) -> String {
    if !is_mercurial_tooling(creation_method, source_control_system) {
        return "The base commit is not in this repository yet. Updating the revision re-runs \
                the check once it is; otherwise the check only re-runs when a commit touching \
                the same files lands."
            .to_owned();
    }
    match creation_method.filter(|method| !method.is_empty()) {
        Some(method) => format!(
            "This revision was submitted with \"{method}\", which records a Mercurial base \
             commit that does not exist in this Git repository. Submitting from a native Git \
             checkout lets the check run."
        ),
        None => "This revision records a Mercurial base commit, which does not exist in this \
                 Git repository. Submitting from a native Git checkout lets the check run."
            .to_owned(),
    }
}

/// `isMercurialTooling()`.
pub fn is_mercurial_tooling(
    creation_method: Option<&str>,
    source_control_system: Option<&str>,
) -> bool {
    if source_control_system == Some("hg") {
        return true;
    }
    let Some(method) = creation_method.filter(|method| !method.is_empty()) else {
        return false;
    };
    method.contains("cinnabar") || method.ends_with("-hg") || method.contains("-hg-")
}

/// `DifferentialRevision::isClosed()`.
fn is_closed(config: &Config, revision: &Revision) -> Result<bool, MethodError> {
    search::status_info(
        &revision.status,
        config.truthy("differential.close-on-accept"),
    )
    .and_then(|status| status["closed"].as_bool())
    .ok_or(MethodError::Unsupported(
        "a revision with an unknown status",
    ))
}

/// The repositories of these revisions the viewer sees (the ones
/// `DifferentialRevisionQuery` attaches) which merge conflicts are
/// checked for (`isEnabledForRevisionRepository()`).
async fn enabled_repositories(
    request: &ApiRequest<'_>,
    revisions: &[Revision],
) -> Result<HashSet<Phid>, MethodError> {
    let mut phids: Vec<Phid> = revisions
        .iter()
        .filter_map(|revision| revision.repository_phid.clone())
        .collect();
    phids.sort();
    phids.dedup();
    if phids.is_empty() {
        return Ok(HashSet::new());
    }
    let context = request.context;
    let found = repository::load(&context.database, &phids).await?;
    let visible = repository::visible(
        &context.database,
        &context.config,
        &request.viewer,
        found,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    let mut enabled = HashSet::new();
    for repository in visible {
        if is_enabled_for_repository(&context.config, &repository)? {
            enabled.insert(repository.phid);
        }
    }
    Ok(enabled)
}

/// The stored values of the field, by revision PHID.
async fn load_stored(
    database: &Database,
    revisions: &[&Revision],
) -> Result<HashMap<Phid, String>, MethodError> {
    if revisions.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT objectPHID, fieldValue FROM {} WHERE objectPHID IN ({}) AND fieldIndex = ?",
        database.table("differential", "differential_customfieldstorage"),
        phab_db::placeholders(revisions.len())
    );
    let query = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        revisions.iter().map(|revision| revision.phid.as_str()),
    )
    .bind(phab_core::hash::digest_for_index(FIELD_KEY.as_bytes()));
    query
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok((
                Phid::new(rows::text(row, "objectPHID")?),
                rows::text(row, "fieldValue")?,
            ))
        })
        .collect::<Result<_, phab_db::DbError>>()
        .map_err(Into::into)
}

/// The revisions of `revisions` whose field shows something: those of an
/// enabled repository the viewer sees, and not closed.
async fn shown<'r>(
    request: &ApiRequest<'_>,
    revisions: &'r [Revision],
) -> Result<Vec<&'r Revision>, MethodError> {
    let enabled = enabled_repositories(request, revisions).await?;
    let mut shown = Vec::new();
    for revision in revisions {
        let in_enabled = revision
            .repository_phid
            .as_ref()
            .is_some_and(|phid| enabled.contains(phid));
        if in_enabled && !is_closed(&request.context.config, revision)? {
            shown.push(revision);
        }
    }
    Ok(shown)
}

/// `loadConflictedRevisionPHIDs()` (Bug 2094527): the revisions of
/// `revisions` the revision lists mark "Merge Conflict", those not closed,
/// of a checked repository the viewer sees, whose stored status is a
/// conflict for their active diff (`isConflictForActiveDiff()`: a verdict
/// for an earlier diff is "Recomputing", not a conflict).
pub async fn conflicted_revision_phids(
    request: &ApiRequest<'_>,
    revisions: &[Revision],
) -> Result<HashSet<Phid>, MethodError> {
    let shown = shown(request, revisions).await?;
    let stored = load_stored(&request.context.database, &shown).await?;
    let mut conflicted = HashSet::new();
    for revision in shown {
        // `phutil_json_decode()`: what doesn't decode is skipped.
        let Some(Ok(value)) = stored
            .get(&revision.phid)
            .map(|stored| serde_json::from_str::<Value>(stored))
        else {
            continue;
        };
        let active = revision.active_diff_phid.as_ref().map(Phid::as_str);
        if is_conflict_for_active_diff(&value, active)? {
            conflicted.insert(revision.phid.clone());
        }
    }
    Ok(conflicted)
}

/// `isConflictForActiveDiff()`: a stored conflict whose checked diff is
/// the active one (or unknown).
fn is_conflict_for_active_diff(value: &Value, active: Option<&str>) -> Result<bool, MethodError> {
    let Value::Object(value) = value else {
        return Ok(false);
    };
    if value.get(KEY_STATUS).and_then(Value::as_str) != Some("conflict") {
        return Ok(false);
    }
    let checked = optional_string(value, KEY_DIFF_PHID)?;
    Ok(!is_checked_diff_stale(checked, active))
}

/// `getConduitDictionaryValue()` of each revision, by PHID; `None` when
/// the field is disabled, and the key absent.
pub(crate) async fn conduit_values(
    request: &ApiRequest<'_>,
    revisions: &[Revision],
) -> Result<Option<HashMap<Phid, Value>>, MethodError> {
    if !is_field_enabled(&request.context.config) {
        return Ok(None);
    }
    let shown = shown(request, revisions).await?;
    let stored = load_stored(&request.context.database, &shown).await?;
    let mut values: HashMap<Phid, Value> = revisions
        .iter()
        .map(|revision| (revision.phid.clone(), Value::Null))
        .collect();
    for revision in shown {
        let value = decode(stored.get(&revision.phid).map(String::as_str));
        let value = conduit_value(request, revision, value).await?;
        values.insert(revision.phid.clone(), value);
    }
    Ok(Some(values))
}

/// `getConduitDictionaryValue()` of a revision whose field shows
/// something: the stored keys in their order, then `isStale`, the
/// redacted ones and `hint` when they weren't stored, without the stack.
async fn conduit_value(
    request: &ApiRequest<'_>,
    revision: &Revision,
    mut value: Map<String, Value>,
) -> Result<Value, MethodError> {
    let is_stale = is_status_stale(&request.context.database, revision, &value).await?;
    value.insert(KEY_IS_STALE.to_owned(), Value::Bool(is_stale));
    viewer_safe(request, &mut value).await?;
    let hint = hint_for_value(&value, is_stale)?;
    value.insert(KEY_HINT.to_owned(), hint.map_or(Value::Null, Value::String));
    value.shift_remove(KEY_STACK_DIFF_PHIDS);
    Ok(Value::Object(value))
}

/// The verdict of the property view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Computed for an earlier diff: a check is queued.
    Recomputing,
    /// `STATUS_CLEAN`.
    Clean,
    /// `STATUS_CONFLICT`.
    Conflict,
    /// `STATUS_UNKNOWN`, or any other status.
    Unknown,
}

impl Verdict {
    /// The icon's tooltip.
    pub fn label(self) -> &'static str {
        match self {
            Self::Recomputing => "Recomputing",
            Self::Clean => "Merges Cleanly",
            Self::Conflict => "Merge Conflict",
            Self::Unknown => "Unknown",
        }
    }

    /// The row's text.
    pub fn text(self) -> &'static str {
        match self {
            Self::Recomputing => "Recomputing for the latest diff",
            Self::Clean => "Landing merges cleanly into the target branch",
            Self::Conflict => {
                "Landing may fail: this does not merge cleanly into the target branch"
            }
            Self::Unknown => "Mergeability could not be determined",
        }
    }

    /// A short name for the page's classes.
    pub fn key(self) -> &'static str {
        match self {
            Self::Recomputing => "recomputing",
            Self::Clean => "clean",
            Self::Conflict => "conflict",
            Self::Unknown => "unknown",
        }
    }
}

/// What `renderPropertyViewValue()` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyView {
    /// The verdict.
    pub verdict: Verdict,
    /// The note under the verdict: the reason, or why a stale one isn't
    /// shown.
    pub verdict_note: Option<String>,
    /// Whether the "last checked" row shows, and when the check ran if
    /// that was recorded.
    pub last_checked: Option<Option<i64>>,
    /// What the check ran against (`newCheckedAgainstDescription()`).
    pub checked_against: Option<String>,
    /// `newHintForValue()`.
    pub hint: Option<String>,
}

/// `newCheckedAgainstDescription()`.
pub fn checked_against_description(diff_id: Option<i64>, base: Option<&str>) -> Option<String> {
    let diff_id = diff_id.filter(|id| *id != 0);
    let base = base.filter(|base| !base.is_empty());
    match (diff_id, base) {
        (Some(id), Some(base)) => Some(format!("Diff {id}, based on {base}")),
        (Some(id), None) => Some(format!("Diff {id}")),
        (None, Some(base)) => Some(format!("Based on {base}")),
        (None, None) => None,
    }
}

/// `renderPropertyViewValue()` for the request's viewer, who sees the
/// revision; `None` when the row doesn't show.
pub async fn property_view(
    request: &ApiRequest<'_>,
    revision: &Revision,
) -> Result<Option<PropertyView>, MethodError> {
    let context = request.context;
    if !is_field_enabled(&context.config) {
        return Ok(None);
    }
    let stored = load_stored(&context.database, &[revision]).await?;
    let mut value = decode(stored.get(&revision.phid).map(String::as_str));
    if !value.get(KEY_STATUS).is_some_and(php::is_truthy) {
        return Ok(None);
    }
    if shown(request, std::slice::from_ref(revision))
        .await?
        .is_empty()
    {
        return Ok(None);
    }
    viewer_safe(request, &mut value).await?;
    let is_stale = is_status_stale(&context.database, revision, &value).await?;
    let (verdict, verdict_note) = if is_stale {
        (
            Verdict::Recomputing,
            Some(
                "The last result was computed for an earlier diff, or for an earlier version \
                 of a revision below this one, so it is not shown."
                    .to_owned(),
            ),
        )
    } else {
        // A loose switch: `true` matches the first case.
        let verdict = match &value[KEY_STATUS] {
            Value::String(status) if status == "clean" => Verdict::Clean,
            Value::Bool(true) => Verdict::Clean,
            Value::String(status) if status == "conflict" => Verdict::Conflict,
            _ => Verdict::Unknown,
        };
        let note = optional_string(&value, KEY_REASON)?
            .filter(|reason| !reason.is_empty())
            .map(str::to_owned);
        (verdict, note)
    };
    let epoch = match value.get(KEY_EPOCH) {
        Some(epoch) if php::is_truthy(epoch) => Some(epoch.as_i64().ok_or(UNUSUAL)?),
        _ => None,
    };
    let diff_id = match value.get(KEY_DIFF_ID) {
        None | Some(Value::Null) => None,
        Some(id) => Some(id.as_i64().ok_or(UNUSUAL)?),
    };
    // newBaseCommitName(): the repository is a Git one, whose commits are
    // named by their first 12 characters without the repository.
    let base = nonempty_string(&value, KEY_BASE_COMMIT)?
        .map(|base| base.get(..12).unwrap_or(base).to_owned());
    let checked_against = checked_against_description(diff_id, base.as_deref());
    let last_checked = (epoch.is_some() || checked_against.is_some()).then_some(epoch);
    Ok(Some(PropertyView {
        verdict,
        verdict_note,
        last_checked,
        checked_against,
        hint: hint_for_value(&value, is_stale)?,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn finds_conflicts_for_the_active_diff_like_php() {
        let conflict = json!({"status": "conflict", "checkedAgainstDiffPHID": "PHID-DIFF-active"});
        let found = |value: &Value, active| is_conflict_for_active_diff(value, active).unwrap();
        assert!(found(&conflict, Some("PHID-DIFF-active")));
        // A conflict for an earlier diff is stale, not a conflict.
        assert!(!found(&conflict, Some("PHID-DIFF-newer")));
        for status in ["clean", "unknown"] {
            let mut value = conflict.clone();
            value["status"] = json!(status);
            assert!(!found(&value, Some("PHID-DIFF-active")), "{status}");
        }
        assert!(!found(&Value::Null, Some("PHID-DIFF-active")));
        assert!(!found(&json!([]), Some("PHID-DIFF-active")));
        assert!(!found(&json!({}), Some("PHID-DIFF-active")));
    }

    #[test]
    fn decodes_like_set_value_from_storage() {
        assert_eq!(decode(None), Map::new());
        assert_eq!(decode(Some("not json")), Map::new());
        assert_eq!(decode(Some("null")), Map::new());
        assert_eq!(decode(Some("\"clean\"")), Map::new());
        assert_eq!(decode(Some("[]")), Map::new());
        assert_eq!(
            Value::Object(decode(Some(r#"["a", {}]"#))),
            json!({"0": "a", "1": []})
        );
        // Duplicate keys keep the first one's place and the last value.
        let value = decode(Some(r#"{"b": 1, "a": {"0": "x"}, "b": 2}"#));
        assert_eq!(value.keys().collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(Value::Object(value), json!({"b": 2, "a": ["x"]}));
    }

    #[test]
    fn hints_like_php() {
        assert_eq!(new_hint(None, None, None), None);
        assert_eq!(new_hint(Some("no-recorded-base"), None, None), None);
        assert_eq!(
            new_hint(Some("internal-error"), None, None).as_deref(),
            Some(
                "This is a fault on the Phabricator side rather than a problem with the \
                 revision. The details are in the daemon log."
            )
        );
        assert_eq!(
            new_hint(Some("base-missing"), Some("moz-phab-git"), Some("git")).as_deref(),
            Some(
                "The base commit is not in this repository yet. Updating the revision re-runs \
                 the check once it is; otherwise the check only re-runs when a commit touching \
                 the same files lands."
            )
        );
        assert_eq!(
            new_hint(Some("base-missing"), Some("moz-phab-hg"), Some("git")).as_deref(),
            Some(
                "This revision was submitted with \"moz-phab-hg\", which records a Mercurial \
                 base commit that does not exist in this Git repository. Submitting from a \
                 native Git checkout lets the check run."
            )
        );
        assert_eq!(
            new_hint(Some("base-missing"), Some(""), Some("hg")).as_deref(),
            Some(
                "This revision records a Mercurial base commit, which does not exist in this \
                 Git repository. Submitting from a native Git checkout lets the check run."
            )
        );
    }

    #[test]
    fn tells_mercurial_tooling() {
        assert!(is_mercurial_tooling(None, Some("hg")));
        assert!(is_mercurial_tooling(Some("git-cinnabar"), Some("git")));
        assert!(is_mercurial_tooling(Some("moz-phab-hg"), None));
        assert!(is_mercurial_tooling(Some("arc-hg-x"), None));
        assert!(is_mercurial_tooling(Some("-hg"), None));
        assert!(!is_mercurial_tooling(Some("moz-phab-git"), Some("git")));
        assert!(!is_mercurial_tooling(Some("hg"), None));
        assert!(!is_mercurial_tooling(None, None));
    }

    #[test]
    fn compares_the_checked_diff() {
        assert!(!is_checked_diff_stale(None, Some("PHID-DIFF-1")));
        assert!(!is_checked_diff_stale(Some(""), Some("PHID-DIFF-1")));
        assert!(!is_checked_diff_stale(Some("PHID-DIFF-1"), None));
        assert!(!is_checked_diff_stale(
            Some("PHID-DIFF-1"),
            Some("PHID-DIFF-1")
        ));
        assert!(is_checked_diff_stale(
            Some("PHID-DIFF-1"),
            Some("PHID-DIFF-2")
        ));
    }

    #[test]
    fn describes_what_was_checked() {
        assert_eq!(
            checked_against_description(Some(3), Some("abc")).as_deref(),
            Some("Diff 3, based on abc")
        );
        assert_eq!(
            checked_against_description(Some(3), Some("")).as_deref(),
            Some("Diff 3")
        );
        assert_eq!(
            checked_against_description(Some(0), Some("abc")).as_deref(),
            Some("Based on abc")
        );
        assert_eq!(checked_against_description(None, None), None);
    }

    #[test]
    fn redacts_after_the_stored_keys() {
        let mut value = decode(Some(r#"{"status": "conflict", "reason": "D1 conflicts"}"#));
        value.insert(KEY_IS_STALE.to_owned(), Value::Bool(false));
        redact(&mut value);
        assert_eq!(
            value.keys().collect::<Vec<_>>(),
            [
                "status",
                "reason",
                "isStale",
                "reasonCode",
                "baseDiffCreationMethod",
                "baseDiffSourceControlSystem"
            ]
        );
        assert_eq!(value["reason"], REDACTED_REASON);
    }
}
