//! `willFilterPage()` of the email feed queries, as the omnipotent viewer,
//! for the stories which are not about revisions: `buildStories()` drops
//! them, but they count in the page and move the cursor, so whether they
//! load and show in feed (`isVisibleInFeed()`) must be known.
//!
//! Ported: stories about projects (`PhabricatorProjectTransaction`) and
//! applications (`PhabricatorApplicationApplicationTransaction`), whose
//! modular transaction types never hide themselves. Stories about other
//! objects are left to PHP.

use std::borrow::Cow;
use std::collections::HashMap;

use phab_conduit::ApiRequest;
use phab_core::{Phid, php};
use phab_db::{Database, rows};
use phab_policy::Viewer;
use serde_json::Value;

use super::store::strict_text;
use super::{Fail, Result};
use crate::story::{self, StoryRow, TRANSACTION_STORY, Unported, VisibleStory};

/// The applications of this fork (`PhabricatorApplication::
/// getAllApplications()`): an application story whose object isn't one of
/// them can't load.
const APPLICATIONS: &[&str] = &[
    "PhabricatorAlmanacApplication",
    "PhabricatorApplicationsApplication",
    "PhabricatorAuditApplication",
    "PhabricatorAuthApplication",
    "PhabricatorCalendarApplication",
    "PhabricatorCelerityApplication",
    "PhabricatorConduitApplication",
    "PhabricatorConfigApplication",
    "PhabricatorDaemonsApplication",
    "PhabricatorDashboardApplication",
    "PhabricatorDifferentialApplication",
    "PhabricatorDiffusionApplication",
    "PhabricatorFavoritesApplication",
    "PhabricatorFeedApplication",
    "PhabricatorFilesApplication",
    "PhabricatorFlagsApplication",
    "PhabricatorHarbormasterApplication",
    "PhabricatorHelpApplication",
    "PhabricatorHeraldApplication",
    "PhabricatorHomeApplication",
    "PhabricatorMetaMTAApplication",
    "PhabricatorNotificationsApplication",
    "PhabricatorPassphraseApplication",
    "PhabricatorPeopleApplication",
    "PhabricatorPolicyApplication",
    "PhabricatorProjectApplication",
    "PhabricatorReviewHelperApplication",
    "PhabricatorSearchApplication",
    "PhabricatorSettingsApplication",
    "PhabricatorSpacesApplication",
    "PhabricatorSubscriptionsApplication",
    "PhabricatorSupportApplication",
    "PhabricatorSystemApplication",
    "PhabricatorTestApplication",
    "PhabricatorTransactionsApplication",
    "PhabricatorTypeaheadApplication",
];

/// `PhabricatorProjectSilencedEdgeType`.
const PROJECT_SILENCED: i64 = 61;

/// The kind of object of a story the email feed ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Project,
    Application,
}

impl Kind {
    fn of(object: &str) -> Option<Self> {
        if object.starts_with("PHID-PROJ-") {
            Some(Self::Project)
        } else if object.starts_with("PHID-APPS-") {
            Some(Self::Application)
        } else {
            None
        }
    }

    /// The PHID prefix of its transactions.
    fn transaction_prefix(self) -> &'static str {
        match self {
            Self::Project => "PHID-XACT-PROJ-",
            Self::Application => "PHID-XACT-APPS-",
        }
    }

    fn table(self, database: &Database) -> String {
        match self {
            Self::Project => database.table("project", "project_transaction"),
            Self::Application => {
                database.table("application", "application_applicationtransaction")
            }
        }
    }
}

/// A story the page holds, in order.
enum Slot {
    /// About a revision: `story::load_visible()` decides.
    Revision(u64),
    /// About a project or an application.
    Other(Box<StoryRow>, Kind, Phid, Vec<Phid>),
}

/// What a story's data says, as `Candidate::parse()` of `story` reads it.
enum Parsed {
    /// No transactions: never visible.
    Hidden,
    Revision,
    Other(Kind, Phid, Vec<Phid>),
}

fn parse(row: &StoryRow) -> Result<Parsed> {
    let Value::Object(data) = &row.story_data else {
        return Err(Fail::Unsupported("feed story data is not a dictionary"));
    };
    let transactions: Vec<&Value> = match data.get("transactionPHIDs") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(entries)) => entries.values().collect(),
        _ => return Err(Fail::Unsupported("feed story without transactions")),
    };
    let transactions: Vec<Phid> = transactions
        .into_iter()
        .map(|phid| phid.as_str().map(Phid::new))
        .collect::<Option<_>>()
        .ok_or(Fail::Unsupported("feed story with invalid transactions"))?;
    if transactions.is_empty() {
        return Ok(Parsed::Hidden);
    }
    let object = match data.get("objectPHID") {
        Some(Value::String(phid)) if php::is_truthy_str(phid) => phid.clone(),
        _ => return Err(Fail::Unsupported("feed story without a usable object")),
    };
    if object.starts_with("PHID-DREV-") {
        return Ok(Parsed::Revision);
    }
    let kind = Kind::of(&object).ok_or(Fail::Unsupported(
        "feed story about objects other than revisions, projects and applications",
    ))?;
    if !transactions
        .iter()
        .all(|phid| phid.as_str().starts_with(kind.transaction_prefix()))
    {
        return Err(Fail::Unsupported(
            "feed story with transactions of another object type",
        ));
    }
    Ok(Parsed::Other(kind, Phid::new(object), transactions))
}

/// A transaction row of a project or an application.
struct Transaction {
    author_phid: String,
    object_phid: String,
    has_comment: bool,
    kind: String,
    old_value: Value,
    new_value: Value,
    metadata: Value,
}

fn json_column(row: &phab_db::sqlx::mysql::MySqlRow, column: &str) -> Result<Value> {
    match strict_text(row, column)?.as_deref() {
        None | Some("") => Ok(Value::Null),
        Some(text) => {
            serde_json::from_str(text).map_err(|_| Fail::Unsupported("invalid JSON in a column"))
        }
    }
}

async fn load_transactions(
    database: &Database,
    kind: Kind,
    phids: &[Phid],
) -> Result<HashMap<String, Transaction>> {
    let sql = format!(
        "SELECT phid, authorPHID, objectPHID, commentPHID, transactionType, oldValue, newValue, \
         metadata FROM {} WHERE phid IN ({})",
        kind.table(database),
        phab_db::placeholders(phids.len()),
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().map(Phid::as_str))
        .fetch_all(database.pool())
        .await?;
    let mut transactions = HashMap::new();
    for row in &found {
        transactions.insert(
            rows::text(row, "phid")?,
            Transaction {
                author_phid: rows::text(row, "authorPHID")?,
                object_phid: rows::text(row, "objectPHID")?,
                has_comment: rows::opt_phid(row, "commentPHID")?.is_some(),
                kind: rows::text(row, "transactionType")?,
                old_value: json_column(row, "oldValue")?,
                new_value: json_column(row, "newValue")?,
                metadata: json_column(row, "metadata")?,
            },
        );
    }
    Ok(transactions)
}

/// Whether the object loads for the omnipotent viewer.
async fn object_loads(
    database: &Database,
    config: &phab_core::Config,
    now: i64,
    kind: Kind,
    object: &Phid,
) -> Result<bool> {
    match kind {
        Kind::Application => Ok(APPLICATIONS
            .iter()
            .any(|class| object.as_str().strip_prefix("PHID-APPS-") == Some(class))),
        Kind::Project => Ok(super::store::load_project(database, config, now, object)
            .await?
            .is_some()),
    }
}

/// Whether a transaction shows in feed; `None` when PHP throws or the
/// answer is not ported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    Yes,
    No,
    Unknown,
}

/// The integer of `edge:type`, which PHP's `switch` compares loosely.
fn edge_type(transaction: &Transaction) -> Option<i64> {
    match transaction.metadata.get("edge:type")? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn metadata_truthy(transaction: &Transaction, key: &str) -> bool {
    transaction.metadata.get(key).is_some_and(php::is_truthy)
}

fn scalar(value: &Value) -> Option<Cow<'_, str>> {
    match value {
        Value::String(text) => Some(Cow::Borrowed(text)),
        Value::Number(number) => Some(Cow::Owned(number.to_string())),
        _ => None,
    }
}

fn list(value: &Value) -> Option<Vec<&Value>> {
    match value {
        Value::Array(items) => Some(items.iter().collect()),
        Value::Object(entries) => Some(entries.values().collect()),
        _ => None,
    }
}

/// `array_diff($from, $other)`: how many values are missing, and the first.
fn missing(from: &[&Value], other: &[&Value]) -> Option<(usize, Option<String>)> {
    let mut count = 0;
    let mut first = None;
    for value in from {
        let text = scalar(value)?;
        let present = other
            .iter()
            .any(|candidate| scalar(candidate).as_deref() == Some(&*text));
        if !present {
            count += 1;
            first.get_or_insert_with(|| text.into_owned());
        }
    }
    Some((count, first))
}

/// `isSelfSubscription()`; `None` where PHP throws.
fn is_self_subscription(transaction: &Transaction) -> Option<bool> {
    if transaction.kind != "core:subscribers" {
        return Some(false);
    }
    let old = list(&transaction.old_value)?;
    let new = list(&transaction.new_value)?;
    let (added, first_added) = missing(&old, &new)?;
    let (removed, first_removed) = missing(&new, &old)?;
    if added + removed != 1 {
        return Some(false);
    }
    Some(first_added.or(first_removed).unwrap_or_default() == transaction.author_phid)
}

/// `PhabricatorApplicationTransaction::shouldHide()` of a transaction
/// without comment, with `PhabricatorProjectTransaction`'s override.
fn should_hide(transaction: &Transaction, kind: Kind) -> Shown {
    let xaction_type = transaction.kind.as_str();
    if kind == Kind::Project
        && xaction_type == "core:edge"
        && edge_type(transaction) == Some(PROJECT_SILENCED)
    {
        return Shown::No;
    }
    if xaction_type == "core:history" {
        return Shown::No;
    }
    if xaction_type == "core:file" && metadata_truthy(transaction, "attach.implicit") {
        return Shown::No;
    }
    let is_create = metadata_truthy(transaction, "core.create");
    if is_create {
        match xaction_type {
            "core:create"
            | "core:view-policy"
            | "core:edit-policy"
            | "core:join-policy"
            | "core:interact-policy"
            | "core:space" => {}
            "core:subtype" => return Shown::No,
            _ => {
                let empty = match &transaction.old_value {
                    Value::Array(items) => items.is_empty(),
                    Value::Object(entries) => entries.is_empty(),
                    Value::Null => true,
                    Value::String(text) => text.is_empty(),
                    Value::Number(number) => number.as_i64() == Some(0) && !number.is_f64(),
                    Value::Bool(_) => false,
                };
                if empty {
                    return Shown::No;
                }
            }
        }
        if metadata_truthy(transaction, "core.default") {
            return Shown::No;
        }
    }
    match xaction_type {
        "core:view-policy"
        | "core:edit-policy"
        | "core:join-policy"
        | "core:interact-policy"
        | "core:space"
            if !is_create =>
        {
            if transaction.old_value.is_null() {
                Shown::No
            } else {
                Shown::Yes
            }
        }
        // Custom fields never hide their transactions:
        // shouldHideInApplicationTransactions() is false for all of them.
        "core:edge" => match edge_type(transaction) {
            Some(52 | 67 | 68) => Shown::No,
            _ => Shown::Yes,
        },
        _ => Shown::Yes,
    }
}

/// `shouldHideForFeed()` of a project or application transaction.
fn shown_in_feed(transaction: &Transaction, kind: Kind) -> Shown {
    match is_self_subscription(transaction) {
        None => return Shown::Unknown,
        Some(true) => return Shown::No,
        Some(false) => {}
    }
    match transaction.kind.as_str() {
        "token:give" | "core:mfa" | "core:inlinestate" => return Shown::No,
        "core:subscribers" if transaction.author_phid.starts_with("PHID-APPS-") => {
            return Shown::No;
        }
        "core:edge" if matches!(edge_type(transaction), Some(5 | 6 | 31 | 32 | 51 | 52)) => {
            return Shown::No;
        }
        _ => {}
    }
    should_hide(transaction, kind)
}

/// Whether a project or application story loads and shows in feed.
async fn other_visible(
    config: &phab_core::Config,
    now: i64,

    database: &Database,
    kind: Kind,
    object: &Phid,
    phids: &[Phid],
) -> Result<bool> {
    if !object_loads(database, config, now, kind, object).await? {
        return Ok(false);
    }
    let transactions = load_transactions(database, kind, phids).await?;
    let mut shown = Vec::new();
    for phid in phids {
        // A required transaction which doesn't load hides the story.
        let Some(transaction) = transactions.get(phid.as_str()) else {
            return Ok(false);
        };
        if transaction.object_phid != object.as_str() {
            return Err(Fail::Unsupported(
                "a story transaction about another object",
            ));
        }
        if transaction.has_comment {
            return Err(Fail::Unsupported(
                "a project or application transaction with a comment",
            ));
        }
        shown.push(shown_in_feed(transaction, kind));
    }
    if shown.contains(&Shown::Yes) {
        Ok(true)
    } else if shown.contains(&Shown::Unknown) {
        Err(Fail::Unsupported(
            "feed visibility of a transaction is not ported",
        ))
    } else {
        Ok(false)
    }
}

/// `willFilterPage()` of the feed queries, as the omnipotent viewer, for a
/// page of rows of any object.
pub(super) async fn filter(
    request: &ApiRequest<'_>,
    rows: Vec<StoryRow>,
) -> Result<Vec<VisibleStory>> {
    let context = request.context;
    let mut slots = Vec::new();
    let mut revision_rows = Vec::new();
    for row in rows {
        // Classes which don't exist (or aren't feed stories) don't load.
        if !row.story_type.eq_ignore_ascii_case(TRANSACTION_STORY) {
            continue;
        }
        match parse(&row)? {
            Parsed::Hidden => {}
            Parsed::Revision => {
                slots.push(Slot::Revision(row.id));
                revision_rows.push(row);
            }
            Parsed::Other(kind, object, phids) => {
                slots.push(Slot::Other(Box::new(row), kind, object, phids));
            }
        }
    }
    let mut revisions: HashMap<u64, VisibleStory> = if revision_rows.is_empty() {
        HashMap::new()
    } else {
        story::load_visible(
            &context.database,
            &context.config,
            &Viewer::omnipotent(),
            revision_rows,
            request.now,
            Unported::Fail,
        )
        .await?
        .visible
        .into_iter()
        .map(|story| (story.row.id, story))
        .collect()
    };
    let mut visible = Vec::new();
    for slot in slots {
        match slot {
            Slot::Revision(id) => {
                if let Some(story) = revisions.remove(&id) {
                    visible.push(story);
                }
            }
            Slot::Other(row, kind, object, phids) => {
                if other_visible(
                    &context.config,
                    request.now,
                    &context.database,
                    kind,
                    &object,
                    &phids,
                )
                .await?
                {
                    let mut story = VisibleStory::unfiltered(*row);
                    story.object = object;
                    visible.push(story);
                }
            }
        }
    }
    Ok(visible)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn transaction(kind: &str, old: Value, new: Value, metadata: Value) -> Transaction {
        Transaction {
            author_phid: "PHID-USER-alice".to_owned(),
            object_phid: "PHID-PROJ-1".to_owned(),
            has_comment: false,
            kind: kind.to_owned(),
            old_value: old,
            new_value: new,
            metadata,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn decides_feed_visibility_like_php() {
        let cases = [
            (
                "project:name",
                json!("a"),
                json!("b"),
                json!({}),
                Shown::Yes,
            ),
            (
                "project:name",
                json!(null),
                json!("b"),
                json!({"core.create": true}),
                Shown::No,
            ),
            (
                "core:create",
                json!(null),
                json!(null),
                json!({"core.create": true}),
                Shown::Yes,
            ),
            (
                "core:edge",
                json!([]),
                json!([]),
                json!({"edge:type": 61}),
                Shown::No,
            ),
            (
                "core:edge",
                json!([]),
                json!([]),
                json!({"edge:type": "13"}),
                Shown::Yes,
            ),
            (
                "core:edge",
                json!([]),
                json!([]),
                json!({"edge:type": 51}),
                Shown::No,
            ),
            (
                "core:subscribers",
                json!([]),
                json!(["PHID-USER-alice"]),
                json!({}),
                Shown::No,
            ),
            (
                "core:subscribers",
                json!([]),
                json!(["PHID-USER-bob"]),
                json!({}),
                Shown::Yes,
            ),
            (
                "core:subscribers",
                json!(null),
                json!([]),
                json!({}),
                Shown::Unknown,
            ),
            (
                "core:view-policy",
                json!(null),
                json!("users"),
                json!({}),
                Shown::No,
            ),
            (
                "core:view-policy",
                json!("admin"),
                json!("users"),
                json!({}),
                Shown::Yes,
            ),
            (
                "core:customfield",
                json!(null),
                json!("x"),
                json!({"customfield:key": "std:x"}),
                Shown::Yes,
            ),
            ("token:give", json!(null), json!("x"), json!({}), Shown::No),
        ];
        for (kind, old, new, metadata, expected) in cases {
            assert_eq!(
                shown_in_feed(
                    &transaction(kind, old, new, metadata.clone()),
                    Kind::Project
                ),
                expected,
                "{kind} {metadata}"
            );
        }
        assert_eq!(
            shown_in_feed(
                &transaction("core:edge", json!([]), json!([]), json!({"edge:type": 61})),
                Kind::Application
            ),
            Shown::Yes
        );
    }
}
