//! `FeedPushWorker::loadFeedStory()`: the story with a chronological key,
//! as `PhabricatorFeedQuery` loads it for the omnipotent viewer
//! (`withChronologicalKeys()`, then `executeOne()`).
//!
//! The omnipotent viewer passes every policy, so a story is left out only
//! when PHP's `PhabricatorFeedStory::loadAllFromRows()` and
//! `willFilterPage()` drop it: its class doesn't exist, an object it needs
//! doesn't load (`PhabricatorObjectQuery`), or none of its transactions shows
//! in feed (`isVisibleInFeed()`). This is ported for stories about
//! revisions, projects and users; any other story is
//! [`StoryError::Unsupported`], and so is a story on which PHP would throw
//! instead of answering (the transaction handles of malformed values, a
//! comment transaction without its comment).

use std::collections::{HashMap, HashSet};

use phab_app_differential::feed::{self, FeedVisibility};
use phab_app_differential::{Comment, Transaction, storage};
use phab_app_project::policy as project_policy;
use phab_core::{Config, Phid, PhidType};
use phab_db::{Database, DbError, edges, rows};
use phab_policy::{ApplicationPolicies, Viewer, application};
use serde_json::Value;

use crate::story::{StoryError, StoryRow, TRANSACTION_STORY};

/// The objects a story can be about, with where their transactions are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `DifferentialRevision`, `DifferentialTransaction`.
    Revision,
    /// `PhabricatorProject`, `PhabricatorProjectTransaction`.
    Project,
    /// `PhabricatorUser`, `PhabricatorUserTransaction`.
    User,
}

impl Kind {
    fn of(object: &Phid) -> Option<Self> {
        match object.phid_type() {
            PhidType::REVISION => Some(Self::Revision),
            PhidType::PROJECT => Some(Self::Project),
            PhidType::USER => Some(Self::User),
            _ => None,
        }
    }

    fn phid_type(self) -> PhidType {
        match self {
            Self::Revision => PhidType::REVISION,
            Self::Project => PhidType::PROJECT,
            Self::User => PhidType::USER,
        }
    }

    /// Whether `phid` is a transaction of this kind (`PHID-XACT-DREV-...`).
    fn is_transaction(self, phid: &Phid) -> bool {
        phid.phid_type() == PhidType::TRANSACTION
            && phid.subtype() == Some(self.phid_type().as_str())
    }

    async fn load_transactions(
        self,
        database: &Database,
        phids: &[Phid],
    ) -> Result<Vec<Transaction>, DbError> {
        match self {
            Self::Revision => storage::load_transactions(database, phids).await,
            Self::Project => {
                storage::load_application_transactions(
                    database,
                    "project",
                    "project_transaction",
                    phids,
                )
                .await
            }
            Self::User => {
                storage::load_application_transactions(database, "user", "user_transaction", phids)
                    .await
            }
        }
    }

    /// The objects of `phids` the omnipotent viewer loads.
    async fn load_objects(
        self,
        database: &Database,
        config: &Config,
        phids: &[Phid],
        now: i64,
    ) -> Result<HashSet<Phid>, StoryError> {
        Ok(match self {
            // `DifferentialRevisionQuery` lets the omnipotent viewer load a
            // revision whose repository doesn't load.
            Self::Revision => storage::load_revisions(database, phids)
                .await?
                .into_iter()
                .map(|revision| revision.phid)
                .collect(),
            // `PhabricatorProjectQuery` drops projects whose ancestors don't
            // load.
            Self::Project => {
                let projects = phab_app_project::storage::load_by_phid(database, phids).await?;
                let (visible, _) = project_policy::visible_projects(
                    database,
                    config,
                    &Viewer::omnipotent(),
                    projects,
                    now,
                )
                .await
                .map_err(|error| match error {
                    project_policy::VisibilityError::Database(error) => StoryError::from(error),
                    project_policy::VisibilityError::Loader(error) => StoryError::from(error),
                    project_policy::VisibilityError::Policy(error) => StoryError::from(error),
                })?;
                visible.into_iter().map(|project| project.phid).collect()
            }
            Self::User => load_users(database, phids).await?,
        })
    }

    /// `shouldHideForFeed()` of a transaction of this kind.
    fn feed_visibility(
        self,
        transaction: &Transaction,
        comment: Option<&Comment>,
    ) -> FeedVisibility {
        match self {
            Self::Revision => feed::feed_visibility(transaction, comment),
            Self::Project => project_feed_visibility(transaction),
            Self::User => user_feed_visibility(transaction),
        }
    }
}

/// The users of `phids` which exist: `PhabricatorPeopleQuery` drops none.
async fn load_users(database: &Database, phids: &[Phid]) -> Result<HashSet<Phid>, DbError> {
    if phids.is_empty() {
        return Ok(HashSet::new());
    }
    let sql = format!(
        "SELECT phid FROM {} WHERE phid IN ({})",
        database.table("user", "user"),
        phab_db::placeholders(phids.len())
    );
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().map(Phid::as_str))
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| Ok(Phid::new(rows::text(row, "phid")?)))
        .collect()
}

/// `PhabricatorProjectTransaction::shouldHideForFeed()`: no project
/// transaction type overrides `shouldHideForFeed()` or `shouldHide()`, and
/// the class hides the silenced edge.
fn project_feed_visibility(transaction: &Transaction) -> FeedVisibility {
    if let Some(decided) = feed::hidden_for_feed_before_should_hide(transaction) {
        return decided;
    }
    if transaction.transaction_type == "core:edge"
        && feed::transaction_edge_type(transaction) == Some(edges::PROJECT_SILENCED)
    {
        return FeedVisibility::Hidden;
    }
    feed::should_hide(transaction, None)
}

/// `PhabricatorUserTransaction::shouldHideForFeed()`: approving and
/// disabling users and notifying them are never in feed.
fn user_feed_visibility(transaction: &Transaction) -> FeedVisibility {
    if matches!(
        transaction.transaction_type.as_str(),
        "user.approve" | "user.disable" | "notify"
    ) {
        return FeedVisibility::Hidden;
    }
    if let Some(decided) = feed::hidden_for_feed_before_should_hide(transaction) {
        return decided;
    }
    feed::should_hide(transaction, None)
}

/// Whether PHP's `array` functions accept `value`: a JSON list or object.
fn is_array(value: &Value) -> bool {
    matches!(value, Value::Array(_) | Value::Object(_))
}

/// Whether `loadAllFromRows()` gets through the transaction without
/// throwing: `getRequiredHandlePHIDs()` merges the values of subscriber,
/// file, edge and member transactions as arrays, and
/// `getMarkupFieldsForFeed()` reads the comment of a comment transaction.
fn loads_without_throwing(transaction: &Transaction, comment: Option<&Comment>) -> bool {
    let (old, new) = (&transaction.old_value, &transaction.new_value);
    match transaction.transaction_type.as_str() {
        "core:subscribers" | "core:file" | "project:members" => is_array(old) && is_array(new),
        // `PhabricatorEdgeChangeRecord` takes falsy values as no edges.
        "core:edge" => [old, new]
            .into_iter()
            .all(|value| is_array(value) || !phab_core::php::is_truthy(value)),
        "core:comment" => comment.is_some(),
        _ => true,
    }
}

/// The chronological key of task data, as `qsprintf('%Ls')` quotes it; only
/// integers and strings, on which PHP doesn't warn.
pub fn chronological_key(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some(number.to_string()),
        _ => None,
    }
}

/// The `feed_storydata` rows `PhabricatorFeedQuery::loadPage()` selects
/// for `key`: only stories with a reference, compared to the key as a
/// string, as PHP quotes it.
async fn load_rows(database: &Database, key: &str) -> Result<Vec<StoryRow>, DbError> {
    let sql = format!(
        "SELECT story.id, story.phid, story.chronologicalKey, story.storyType, \
         story.storyData, story.authorPHID, story.dateCreated FROM {} story JOIN {} ref \
         ON ref.chronologicalKey = story.chronologicalKey WHERE ref.chronologicalKey IN (?) \
         GROUP BY story.chronologicalKey ORDER BY story.chronologicalKey DESC",
        database.table("feed", "feed_storydata"),
        database.table("feed", "feed_storyreference"),
    );
    let rows = phab_db::sqlx::query(&sql)
        .bind(key)
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| {
            // Lisk decodes the JSON without failing: `null` if it isn't.
            let story_data = rows::opt_text(row, "storyData")?
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or(Value::Null);
            Ok(StoryRow {
                id: rows::uint(row, "id")?,
                phid: rows::text(row, "phid")?,
                chronological_key: rows::uint(row, "chronologicalKey")?,
                story_type: rows::text(row, "storyType")?,
                story_data,
                author_phid: rows::text(row, "authorPHID")?,
                date_created: rows::int(row, "dateCreated")?,
            })
        })
        .collect()
}

/// What a story's data says it needs.
struct Needs {
    kind: Kind,
    object: Phid,
    /// `transactionPHIDs`, without duplicates, in order.
    transactions: Vec<Phid>,
}

/// Reads `objectPHID` and `transactionPHIDs`; `None` for a story without
/// transactions, which `isVisibleInFeed()` drops.
fn needs(row: &StoryRow) -> Result<Option<Needs>, StoryError> {
    let Value::Object(data) = &row.story_data else {
        return Err(StoryError::Unsupported(
            "feed story data is not a dictionary",
        ));
    };
    let listed: Vec<&Value> = match data.get("transactionPHIDs") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(entries)) => entries.values().collect(),
        // `foreach` over anything else warns.
        _ => return Err(StoryError::Unsupported("feed story without transactions")),
    };
    let mut transactions: Vec<Phid> = Vec::new();
    for phid in listed {
        let Some(phid) = phid.as_str() else {
            return Err(StoryError::Unsupported(
                "feed story with invalid transactions",
            ));
        };
        let phid = Phid::new(phid);
        if !transactions.contains(&phid) {
            transactions.push(phid);
        }
    }
    if transactions.is_empty() {
        return Ok(None);
    }
    let object = match data.get("objectPHID") {
        Some(Value::String(phid)) if phab_core::php::is_truthy_str(phid) => Phid::new(phid),
        // Without it, the story asks for a `null` handle.
        _ => return Err(StoryError::Unsupported("feed story without an object")),
    };
    let Some(kind) = Kind::of(&object) else {
        return Err(StoryError::Unsupported(
            "feed story about an object type which is not ported",
        ));
    };
    if !transactions.iter().all(|phid| kind.is_transaction(phid)) {
        return Err(StoryError::Unsupported(
            "feed story with transactions of another object type",
        ));
    }
    Ok(Some(Needs {
        kind,
        object,
        transactions,
    }))
}

/// `loadFeedStory()` without its exception: the story with chronological
/// key `key` if the omnipotent viewer loads it, `None` if PHP finds none.
pub async fn load_feed_story(
    database: &Database,
    config: &Config,
    key: &str,
    now: i64,
) -> Result<Option<StoryRow>, StoryError> {
    // `GROUP BY story.chronologicalKey` over a unique key: one row at most,
    // so `executeOne()` never throws.
    let Some(row) = load_rows(database, key).await?.into_iter().next() else {
        return Ok(None);
    };
    // `class_exists()` ignores case; the transaction story is the only
    // subclass of `PhabricatorFeedStory`.
    if !row.story_type.eq_ignore_ascii_case(TRANSACTION_STORY) {
        return Ok(None);
    }
    let Some(needs) = needs(&row)? else {
        return Ok(None);
    };
    let kind = needs.kind;

    // `PhabricatorObjectQuery` skips the types of uninstalled applications.
    let installed = application::application_for_phid_type(kind.phid_type())
        .is_none_or(|class| ApplicationPolicies::from_config(config).is_installed(class));
    if !installed {
        return Ok(None);
    }

    // `PhabricatorApplicationTransactionQuery`: the transactions, their
    // comments, and only those whose object loads.
    let transactions: HashMap<Phid, Transaction> = kind
        .load_transactions(database, &needs.transactions)
        .await?
        .into_iter()
        .map(|transaction| (transaction.phid.clone(), transaction))
        .collect();
    if transactions
        .values()
        .any(|transaction| transaction.object_phid.phid_type() != kind.phid_type())
    {
        return Err(StoryError::Unsupported(
            "feed story with a transaction about another object type",
        ));
    }
    let mut objects: Vec<Phid> = transactions
        .values()
        .map(|transaction| transaction.object_phid.clone())
        .chain([needs.object.clone()])
        .collect();
    objects.sort();
    objects.dedup();
    let loaded = kind.load_objects(database, config, &objects, now).await?;
    let comments: HashMap<Phid, Comment> = if kind == Kind::Revision {
        let mut phids: Vec<Phid> = transactions
            .values()
            .filter_map(|transaction| transaction.comment_phid.clone())
            .collect();
        phids.sort();
        phids.dedup();
        storage::load_comments(database, &phids)
            .await?
            .into_iter()
            .map(|comment| (comment.phid.clone(), comment))
            .collect()
    } else {
        // Projects and users have no comments.
        HashMap::new()
    };

    // `loadAllFromRows()` drops the story if an object it needs is missing.
    if !loaded.contains(&needs.object) {
        return Ok(None);
    }
    let mut story = Vec::new();
    for phid in &needs.transactions {
        let Some(transaction) = transactions
            .get(phid)
            .filter(|transaction| loaded.contains(&transaction.object_phid))
        else {
            return Ok(None);
        };
        let comment = transaction
            .comment_phid
            .as_ref()
            .and_then(|phid| comments.get(phid));
        story.push((transaction, comment));
    }

    // Then it loads the handles and markup of the stories it keeps.
    if !story
        .iter()
        .all(|(transaction, comment)| loads_without_throwing(transaction, *comment))
    {
        return Err(StoryError::Unsupported(
            "feed story which PHP fails to load",
        ));
    }

    // `willFilterPage()`: `isVisibleInFeed()`.
    let visibility: Vec<FeedVisibility> = story
        .iter()
        .map(|(transaction, comment)| kind.feed_visibility(transaction, *comment))
        .collect();
    match feed::story_visibility(&visibility) {
        FeedVisibility::Visible => Ok(Some(row)),
        FeedVisibility::Hidden => Ok(None),
        FeedVisibility::Unknown => Err(StoryError::Unsupported(
            "feed visibility of a transaction is not ported",
        )),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn transaction(kind: &str, old: Value, new: Value, metadata: Value) -> Transaction {
        Transaction {
            phid: Phid::new("PHID-XACT-PROJ-1"),
            author_phid: Phid::new("PHID-USER-alice"),
            object_phid: Phid::new("PHID-PROJ-1"),
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            comment_phid: None,
            transaction_type: kind.to_owned(),
            old_value: old,
            new_value: new,
            metadata,
        }
    }

    #[test]
    fn hides_project_and_user_transactions_like_php() {
        let silenced = transaction(
            "core:edge",
            json!([]),
            json!(["PHID-USER-bob"]),
            json!({"edge:type": 61}),
        );
        assert_eq!(project_feed_visibility(&silenced), FeedVisibility::Hidden);
        assert_eq!(user_feed_visibility(&silenced), FeedVisibility::Visible);
        let name = transaction("project:name", json!("a"), json!("b"), json!({}));
        assert_eq!(project_feed_visibility(&name), FeedVisibility::Visible);
        let created = transaction(
            "project:name",
            json!(null),
            json!("b"),
            json!({"core.create": true}),
        );
        assert_eq!(project_feed_visibility(&created), FeedVisibility::Hidden);
        for hidden in ["user.approve", "user.disable", "notify"] {
            let xaction = transaction(hidden, json!(false), json!(true), json!({}));
            assert_eq!(user_feed_visibility(&xaction), FeedVisibility::Hidden);
        }
        let rename = transaction("user.rename", json!("a"), json!("b"), json!({}));
        assert_eq!(user_feed_visibility(&rename), FeedVisibility::Visible);
    }

    #[test]
    fn finds_what_php_throws_on() {
        let members = transaction("project:members", json!(null), json!(["x"]), json!({}));
        assert!(!loads_without_throwing(&members, None));
        let edge = transaction("core:edge", json!(null), json!(["x"]), json!({}));
        assert!(loads_without_throwing(&edge, None));
        let edge = transaction("core:edge", json!("x"), json!([]), json!({}));
        assert!(!loads_without_throwing(&edge, None));
        let comment = transaction("core:comment", json!(null), json!(null), json!({}));
        assert!(!loads_without_throwing(&comment, None));
    }

    #[test]
    fn quotes_keys_like_qsprintf() {
        assert_eq!(chronological_key(&json!(7)).as_deref(), Some("7"));
        assert_eq!(chronological_key(&json!("7")).as_deref(), Some("7"));
        assert_eq!(chronological_key(&json!(1.5)), None);
        assert_eq!(chronological_key(&json!(null)), None);
        assert_eq!(chronological_key(&json!([7])), None);
    }
}
