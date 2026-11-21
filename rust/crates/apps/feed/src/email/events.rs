//! `EventKind`, `PhabricatorStory::buildStories()` and
//! `EmailEventBuilder::buildEvents()`: the email events of feed stories.

use phab_auth::User;
use phab_core::Phid;
use serde_json::{Value, json};

use super::comments::{self, PublicPings, SecurePings};
use super::recipients::{
    ResolveUsers, find_reviewer, is_actionable, status_text, translate_status,
};
use super::store::{RevisionRow, Store, TransactionRow};
use super::{Fail, Result, bugzilla, code, production_uri};
use crate::story::VisibleStory;

const ABANDON: &str = "revision-abandoned";
const METADATA_EDIT: &str = "revision-metadata-edited";
const RECLAIM: &str = "revision-reclaimed";
const ACCEPT: &str = "revision-accepted";
const COMMENT: &str = "revision-commented";
const UPDATE: &str = "revision-updated";
const REJECT: &str = "revision-requested-changes";
const REQUEST_REVIEW: &str = "revision-requested-review";
const CLOSE: &str = "revision-closed";
const LAND: &str = "revision-landed";
const CREATE: &str = "revision-created";
const PINGED: &str = "revision-comment-pinged";

/// `TransactionList`.
pub(crate) struct TransactionList {
    transactions: Vec<TransactionRow>,
}

impl TransactionList {
    /// `getAnyTransaction()`: the first one.
    fn any(&self) -> Result<&TransactionRow> {
        self.transactions
            .first()
            .ok_or(Fail::Throw("an empty transaction list"))
    }

    /// `getFirstTransaction()`: the lowest ID.
    fn first(&self) -> Result<&TransactionRow> {
        self.transactions
            .iter()
            .min_by_key(|transaction| transaction.id)
            .ok_or(Fail::Throw("an empty transaction list"))
    }

    /// `getAllTransactionsWithType()`.
    pub(crate) fn all_with_type(
        &self,
        kind: &str,
    ) -> impl Iterator<Item = &TransactionRow> + use<'_> {
        let kind = kind.to_owned();
        self.transactions
            .iter()
            .filter(move |transaction| transaction.transaction_type == kind)
    }

    /// `attemptGetTransactionWithType()`: throws for several.
    pub(crate) fn attempt_with_type(&self, kind: &str) -> Result<Option<&TransactionRow>> {
        let mut matching = self.all_with_type(kind);
        let first = matching.next();
        if matching.next().is_some() {
            return Err(Fail::Throw("Too many transactions match type"));
        }
        Ok(first)
    }

    /// `getTransactionWithType()`: throws for none.
    pub(crate) fn with_type(&self, kind: &str) -> Result<&TransactionRow> {
        self.attempt_with_type(kind)?
            .ok_or(Fail::Throw("Expected a transaction to match type"))
    }

    /// `containsType()`.
    fn contains(&self, kind: &str) -> bool {
        self.all_with_type(kind).next().is_some()
    }

    /// `containsOneOfType()`.
    fn contains_one_of(&self, kinds: &[&str]) -> bool {
        kinds.iter().any(|kind| self.contains(kind))
    }
}

/// `EventKind`.
#[derive(Debug, Clone, Copy)]
struct EventKind {
    public_kind: &'static str,
    main_type: Option<&'static str>,
}

impl EventKind {
    const fn new(public_kind: &'static str, main_type: Option<&'static str>) -> Self {
        Self {
            public_kind,
            main_type,
        }
    }

    /// `findMainTransaction()`.
    fn main_transaction<'t>(
        &self,
        transactions: &'t TransactionList,
    ) -> Result<&'t TransactionRow> {
        match self.main_type {
            Some(kind) => transactions.with_type(kind),
            None => transactions.any(),
        }
    }

    /// `findActor()`.
    fn actor(&self, transactions: &TransactionList, revision: &RevisionRow) -> Result<Phid> {
        if self.public_kind == CREATE {
            revision
                .author_phid
                .clone()
                .ok_or(Fail::Throw("a revision without author"))
        } else {
            Ok(self.main_transaction(transactions)?.author_phid.clone())
        }
    }

    /// `mainKind()`.
    async fn main(transactions: &TransactionList, store: &mut Store<'_>) -> Result<Option<Self>> {
        let kind = if transactions.contains("differential.revision.abandon") {
            Self::new(ABANDON, Some("differential.revision.abandon"))
        } else if transactions.contains("differential.revision.reclaim") {
            Self::new(RECLAIM, Some("differential.revision.reclaim"))
        } else if transactions.contains("differential.revision.accept") {
            Self::new(ACCEPT, Some("differential.revision.accept"))
        } else if transactions.contains("differential.revision.reject") {
            Self::new(REJECT, Some("differential.revision.reject"))
        } else if transactions.contains("differential.revision.close") {
            let close = transactions.with_type("differential.revision.close")?;
            let is_commit = close
                .metadata_value("isCommitClose")
                .is_some_and(phab_core::php::is_truthy);
            Self::new(
                if is_commit { LAND } else { CLOSE },
                Some("differential.revision.close"),
            )
        } else if transactions.contains("differential:update") {
            Self::new(UPDATE, Some("differential:update"))
        } else if transactions.contains_one_of(&[
            "core:customfield",
            "differential.revision.title",
            "differential.revision.reviewers",
        ]) {
            Self::new(METADATA_EDIT, None)
        } else if transactions.contains("differential.revision.request") {
            let request = transactions.with_type("differential.revision.request")?;
            let actor = store.user(&request.author_phid).await?;
            if actor.user_name != "phab-bot" {
                Self::new(REQUEST_REVIEW, Some("differential.revision.request"))
            } else if ["core:view-policy", "core:edit-policy", "core:edge"]
                .iter()
                .all(|kind| transactions.contains(kind))
            {
                Self::new(CREATE, None)
            } else {
                return Ok(None);
            }
        } else if transactions.contains_one_of(&["core:comment", "differential:inline"]) {
            Self::new(COMMENT, None)
        } else {
            return Ok(None);
        };
        Ok(Some(kind))
    }
}

/// `PhabricatorStory`.
struct Story {
    kind: EventKind,
    transactions: TransactionList,
    revision: RevisionRow,
    actor: User,
    key: String,
    id: u64,
    timestamp: i64,
}

impl Story {
    /// `getTransactionLink()`.
    fn transaction_link(&self, store: &Store<'_>) -> Result<String> {
        let first = self.transactions.first()?;
        production_uri(
            store.config,
            &format!("/{}#{}", self.revision.monogram(), first.id),
        )
    }
}

/// `PhabricatorStory::buildStories()`: the stories about revisions there
/// is something to mail about.
async fn build_stories(store: &mut Store<'_>, stories: Vec<VisibleStory>) -> Result<Vec<Story>> {
    struct Builder {
        kind: EventKind,
        transactions: TransactionList,
        revision_phid: Phid,
        row: crate::story::StoryRow,
        revision: Option<RevisionRow>,
        actor_phid: Option<Phid>,
    }

    let mut builders = Vec::new();
    for entry in stories {
        if !entry.object.as_str().starts_with("PHID-DREV") {
            continue;
        }
        let phids: Vec<Phid> = entry
            .transactions
            .iter()
            .map(|(transaction, _)| transaction.phid.clone())
            .collect();
        let loaded = store.transactions(&phids).await?;
        let mut transactions = Vec::new();
        for phid in &phids {
            let transaction = loaded
                .iter()
                .find(|transaction| transaction.phid == *phid)
                .ok_or(Fail::Unsupported("a story transaction which disappeared"))?;
            // Transactions by other authors, like Herald, are left out.
            if transaction.author_phid.as_str() == entry.row.author_phid {
                transactions.push(transaction.clone());
            }
        }
        let transactions = TransactionList { transactions };
        let Some(kind) = EventKind::main(&transactions, store).await? else {
            continue;
        };
        let revision_phid = kind.main_transaction(&transactions)?.object_phid.clone();
        builders.push(Builder {
            kind,
            transactions,
            revision_phid,
            row: entry.row,
            revision: None,
            actor_phid: None,
        });
    }
    if builders.is_empty() {
        return Ok(Vec::new());
    }

    let phids: Vec<Phid> = builders
        .iter()
        .map(|builder| builder.revision_phid.clone())
        .collect();
    let revisions = store.revisions(&phids).await?;
    let mut broadcast = Vec::new();
    let mut actor_phids = Vec::new();
    for mut builder in builders {
        let Some(revision) = revisions
            .iter()
            .find(|revision| revision.phid == builder.revision_phid)
        else {
            // The actor of the entry is never set.
            return Err(Fail::Unsupported("a entry whose revision is gone"));
        };
        // Drafts are not mailed about.
        if !revision.should_broadcast() {
            continue;
        }
        let actor = builder.kind.actor(&builder.transactions, revision)?;
        actor_phids.push(actor.clone());
        builder.actor_phid = Some(actor);
        builder.revision = Some(revision.clone());
        broadcast.push(builder);
    }
    if broadcast.is_empty() {
        return Ok(Vec::new());
    }

    let users = store.users(&actor_phids).await?;
    let mut stories = Vec::new();
    for builder in broadcast {
        let Some(actor) = users
            .iter()
            .find(|user| Some(&user.phid) == builder.actor_phid.as_ref())
        else {
            // Stories whose actor is not a user are dropped.
            continue;
        };
        stories.push(Story {
            kind: builder.kind,
            transactions: builder.transactions,
            revision: builder.revision.expect("broadcast stories have a revision"),
            actor: actor.clone(),
            key: builder.row.chronological_key.to_string(),
            id: builder.row.id,
            timestamp: builder.row.date_created,
        });
    }
    Ok(stories)
}

/// What the `try` of `buildEvents()` builds for a entry.
enum Contexts {
    /// The entry's contexts, then one per ping.
    Built(Vec<Value>),
    /// `continue`: an update without changes.
    Skip,
}

/// `ResolveRevisionStatus`.
fn lando_link(store: &Store<'_>, revision: &RevisionRow) -> Result<String> {
    let lando = store
        .config
        .string("lando.url")
        .ok_or(Fail::Unsupported("no lando.url"))?;
    with_path(lando, &format!("/{}/", revision.monogram()))
}

/// `(string) (new PhutilURI($uri))->setPath($path)` for a URI with a scheme
/// and an authority only.
fn with_path(uri: &str, path: &str) -> Result<String> {
    let (scheme, rest) = uri
        .split_once("://")
        .ok_or(Fail::Unsupported("an unusual URI"))?;
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if scheme.is_empty() || authority.is_empty() || authority.contains(['/', '?', '#']) {
        return Err(Fail::Unsupported("an unusual URI"));
    }
    Ok(format!("{scheme}://{authority}{path}"))
}

fn is_ready_to_land(revision: &RevisionRow) -> bool {
    revision.status == "accepted"
}

fn is_needing_review(revision: &RevisionRow) -> bool {
    revision.status == "needs-review"
}

/// Pings collected while building the body.
enum Pings {
    Secure(SecurePings),
    Public(PublicPings),
}

/// The comments of a body: their count for secure revisions, the messages
/// for public ones.
enum Comments {
    Secure(i64),
    Public(comments::PublicComments),
}

impl Comments {
    fn count(&self) -> i64 {
        match self {
            Self::Secure(count) => *count,
            Self::Public(_) => 0,
        }
    }

    fn main(&self) -> Value {
        match self {
            Self::Public(comments) => comments.main.clone(),
            Self::Secure(_) => Value::Null,
        }
    }

    fn inlines(&self) -> Value {
        match self {
            Self::Public(comments) => Value::from(comments.inlines.clone()),
            Self::Secure(_) => json!([]),
        }
    }
}

/// `resolveSecureComments()` or `resolvePublicComments()`.
async fn resolve_comments(
    store: &mut Store<'_>,
    entry: &Story,
    pings: &mut Pings,
) -> Result<Comments> {
    match pings {
        Pings::Secure(pings) => Ok(Comments::Secure(
            comments::secure_comments(store, &entry.transactions, pings).await?,
        )),
        Pings::Public(pings) => Ok(Comments::Public(
            comments::public_comments(store, &entry.transactions, &entry.revision, pings).await?,
        )),
    }
}

/// `EmailRevisionLanded::from()`.
async fn landed(store: &mut Store<'_>, users: &ResolveUsers<'_>, entry: &Story) -> Result<Value> {
    let close = entry
        .transactions
        .with_type("differential.revision.close")?;
    let commit_phid = match close.metadata_value("commitPHID") {
        Some(Value::String(phid)) => phid.clone(),
        None | Some(Value::Null) => return Err(Fail::Throw("a landing without commit")),
        Some(_) => return Err(Fail::Unsupported("an unusual landed commit")),
    };
    let commit = store
        .commit(&commit_phid)
        .await?
        .ok_or(Fail::Throw("a landed commit which doesn't exist"))?;
    let repository = store
        .repository(None, Some(commit.repository_id))
        .await?
        .ok_or(Fail::Throw("a commit without repository"))?;
    let hg_link = if repository.vcs == "hg" {
        store
            .observed_uris(&repository.phid)
            .await?
            .first()
            .map(|uri| format!("{uri}/rev/{}", commit.identifier))
    } else {
        None
    };
    let lando_link = match store.config.value("lando.url") {
        Some(Value::String(uri)) if phab_core::php::is_truthy_str(uri) => {
            Some(with_path(uri, &format!("/D{}/", entry.revision.id))?)
        }
        Some(value) if phab_core::php::is_truthy(value) => {
            return Err(Fail::Unsupported("an unusual lando.url"));
        }
        _ => None,
    };
    Ok(json!({
        "subscribers": users.subscribers(store).await?,
        "reviewers": users.reviewers(store, false).await?,
        "author": users.author(store).await?,
        "revisionHash": commit.identifier,
        "hgLink": hg_link,
        "landoLink": lando_link,
    }))
}

/// `ReviewersTransaction`: a value of a reviewers transaction, by PHID.
fn reviewer_map(value: &Value) -> Result<Vec<(String, Value)>> {
    match value {
        Value::Object(map) => Ok(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        Value::Array(items) if items.is_empty() => Ok(Vec::new()),
        _ => Err(Fail::Throw("array_keys() of a value which is not an array")),
    }
}

/// `SecureEmailRevisionMetadataEdited::from()`.
#[allow(clippy::too_many_lines)]
async fn metadata_edited(
    store: &mut Store<'_>,
    users: &ResolveUsers<'_>,
    entry: &Story,
) -> Result<Value> {
    let transactions = &entry.transactions;
    let revision = &entry.revision;
    let title_changed = transactions.contains("differential.revision.title");
    let bug_changed = match transactions.attempt_with_type("core:customfield")? {
        Some(transaction) => {
            transaction.metadata_value("customfield:key")
                == Some(&json!("differential:bugzilla-bug-id"))
        }
        None => false,
    };
    let changed_to_need_review =
        match transactions.attempt_with_type("differential.revision.status")? {
            Some(transaction) => {
                let text = |value: &Value| match value {
                    Value::String(text) => Ok(Some(text.clone())),
                    Value::Null => Ok(None),
                    _ => Err(Fail::Unsupported("an unusual revision status")),
                };
                let old = text(&transaction.old_value)?;
                let new = text(&transaction.new_value)?;
                new.as_deref() == Some("needs-review") && new != old
            }
            None => false,
        };

    let mut reviewers = Vec::new();
    let reviewer_transactions: Vec<&TransactionRow> = transactions
        .all_with_type("differential.revision.reviewers")
        .collect();
    if reviewer_transactions.is_empty() {
        for reviewer in users.reviewers(store, changed_to_need_review).await? {
            reviewers.push(json!({
                "name": reviewer["name"],
                "isActionable": reviewer["isActionable"],
                "status": reviewer["status"],
                "metadataChange": "no-change",
                "recipients": reviewer["recipients"],
            }));
        }
    } else {
        let mut processed: Vec<String> = Vec::new();
        for transaction in reviewer_transactions {
            let old = reviewer_map(&transaction.old_value)?;
            let new = reviewer_map(&transaction.new_value)?;
            let mut all: Vec<String> = Vec::new();
            for (phid, _) in old.iter().chain(&new) {
                if !all.contains(phid) {
                    all.push(phid.clone());
                }
            }
            let new_statuses = new
                .iter()
                .map(|(_, status)| status_text(status))
                .collect::<Result<Vec<_>>>()?;
            for phid in all {
                if processed.contains(&phid) {
                    continue;
                }
                processed.push(phid.clone());
                let lookup = |map: &[(String, Value)]| {
                    map.iter()
                        .find(|(key, _)| *key == phid)
                        .map(|(_, value)| value.clone())
                        .filter(|value| !value.is_null())
                };
                let (old_value, new_value) = (lookup(&old), lookup(&new));
                let status = match new_value.as_ref().or(old_value.as_ref()) {
                    Some(value) => translate_status(&status_text(value)?),
                    None => return Err(Fail::Throw("a reviewer without status")),
                };
                let truthy =
                    |value: &Option<Value>| value.as_ref().is_some_and(phab_core::php::is_truthy);
                let change = match (truthy(&old_value), truthy(&new_value)) {
                    (true, false) => "removed",
                    (false, true) => "added",
                    _ => "no-change",
                };
                let raw = revision
                    .reviewers
                    .iter()
                    .find(|reviewer| reviewer.phid.as_str() == phid);
                let actionable = match change {
                    "added" | "no-change" => {
                        let raw = raw.ok_or(Fail::Throw("a reviewer no longer on the revision"))?;
                        is_actionable(&new_statuses, raw)
                            && (change == "added" || changed_to_need_review)
                    }
                    _ => false,
                };
                let reviewer = find_reviewer(store, &Phid::new(phid.clone())).await?;
                let recipients = reviewer.recipients(store, &users.actor_email).await?;
                reviewers.push(json!({
                    "name": reviewer.name(),
                    "isActionable": actionable,
                    "status": status,
                    "metadataChange": change,
                    "recipients": recipients,
                }));
            }
        }
    }
    Ok(json!({
        "isReadyToLand": is_ready_to_land(revision),
        "isTitleChanged": title_changed,
        "isBugChanged": bug_changed,
        "author": users.author(store).await?,
        "reviewers": reviewers,
        "subscribers": users.subscribers(store).await?,
    }))
}

/// The body of the story's event, and the pings it collected.
#[allow(clippy::too_many_lines)]
async fn body(
    store: &mut Store<'_>,
    users: &ResolveUsers<'_>,
    entry: &Story,
    secure: bool,
) -> Result<Option<(Value, Pings)>> {
    let mut pings = if secure {
        Pings::Secure(SecurePings::default())
    } else {
        Pings::Public(PublicPings::default())
    };
    let revision = &entry.revision;
    let body = match entry.kind.public_kind {
        ABANDON => {
            let comments = resolve_comments(store, entry, &mut pings).await?;
            if secure {
                json!({
                    "reviewers": users.reviewers(store, false).await?,
                    "subscribers": users.subscribers(store).await?,
                    "commentCount": comments.count(),
                    "transactionLink": entry.transaction_link(store)?,
                })
            } else {
                json!({
                    "mainCommentMessage": comments.main(),
                    "inlineComments": comments.inlines(),
                    "transactionLink": entry.transaction_link(store)?,
                    "reviewers": users.reviewers(store, false).await?,
                    "subscribers": users.subscribers(store).await?,
                })
            }
        }
        RECLAIM | REQUEST_REVIEW => {
            let reviewers = users.reviewers(store, true).await?;
            let comments = resolve_comments(store, entry, &mut pings).await?;
            if secure {
                json!({
                    "reviewers": reviewers,
                    "subscribers": users.subscribers(store).await?,
                    "commentCount": comments.count(),
                    "transactionLink": entry.transaction_link(store)?,
                })
            } else {
                json!({
                    "mainCommentMessage": comments.main(),
                    "inlineComments": comments.inlines(),
                    "transactionLink": entry.transaction_link(store)?,
                    "reviewers": reviewers,
                    "subscribers": users.subscribers(store).await?,
                })
            }
        }
        COMMENT => {
            let comments = resolve_comments(store, entry, &mut pings).await?;
            if secure {
                json!({
                    "subscribers": users.subscribers(store).await?,
                    "reviewers": users.reviewers(store, false).await?,
                    "author": users.author(store).await?,
                    "transactionLink": entry.transaction_link(store)?,
                })
            } else {
                json!({
                    "transactionLink": entry.transaction_link(store)?,
                    "mainCommentMessage": comments.main(),
                    "inlineComments": comments.inlines(),
                    "subscribers": users.subscribers(store).await?,
                    "reviewers": users.reviewers(store, false).await?,
                    "author": users.author(store).await?,
                })
            }
        }
        CLOSE => {
            let comments = resolve_comments(store, entry, &mut pings).await?;
            if secure {
                json!({
                    "subscribers": users.subscribers(store).await?,
                    "reviewers": users.reviewers(store, false).await?,
                    "author": users.author(store).await?,
                    "commentCount": comments.count(),
                    "transactionLink": entry.transaction_link(store)?,
                })
            } else {
                json!({
                    "mainCommentMessage": comments.main(),
                    "inlineComments": comments.inlines(),
                    "transactionLink": entry.transaction_link(store)?,
                    "subscribers": users.subscribers(store).await?,
                    "reviewers": users.reviewers(store, false).await?,
                    "author": users.author(store).await?,
                })
            }
        }
        LAND => {
            if secure {
                json!({
                    "subscribers": users.subscribers(store).await?,
                    "reviewers": users.reviewers(store, false).await?,
                    "author": users.author(store).await?,
                })
            } else {
                landed(store, users, entry).await?
            }
        }
        REJECT => {
            let comments = resolve_comments(store, entry, &mut pings).await?;
            if secure {
                json!({
                    "transactionLink": entry.transaction_link(store)?,
                    "subscribers": users.subscribers(store).await?,
                    "reviewers": users.reviewers(store, false).await?,
                    "author": users.author(store).await?,
                    "commentCount": comments.count(),
                })
            } else {
                json!({
                    "transactionLink": entry.transaction_link(store)?,
                    "mainCommentMessage": comments.main(),
                    "inlineComments": comments.inlines(),
                    "subscribers": users.subscribers(store).await?,
                    "reviewers": users.reviewers(store, false).await?,
                    "author": users.author(store).await?,
                })
            }
        }
        CREATE => {
            let reviewers = users.reviewers(store, true).await?;
            if secure {
                json!({
                    "reviewers": reviewers,
                    "subscribers": users.subscribers(store).await?,
                })
            } else {
                json!({
                    "affectedFiles": code::affected_files(store, revision).await?,
                    "reviewers": reviewers,
                    "subscribers": users.subscribers(store).await?,
                })
            }
        }
        ACCEPT => {
            let comments = resolve_comments(store, entry, &mut pings).await?;
            if secure {
                json!({
                    "landoLink": lando_link(store, revision)?,
                    "isReadyToLand": is_ready_to_land(revision),
                    "subscribers": users.subscribers(store).await?,
                    "reviewers": users.reviewers(store, false).await?,
                    "author": users.author(store).await?,
                    "commentCount": comments.count(),
                    "transactionLink": entry.transaction_link(store)?,
                })
            } else {
                json!({
                    "mainCommentMessage": comments.main(),
                    "inlineComments": comments.inlines(),
                    "transactionLink": entry.transaction_link(store)?,
                    "landoLink": lando_link(store, revision)?,
                    "isReadyToLand": is_ready_to_land(revision),
                    "subscribers": users.subscribers(store).await?,
                    "reviewers": users.reviewers(store, false).await?,
                    "author": users.author(store).await?,
                })
            }
        }
        UPDATE => {
            let reviewers = users.reviewers(store, is_needing_review(revision)).await?;
            if !code::any_diff_changes(store, &entry.transactions).await? {
                return Ok(None);
            }
            if secure {
                json!({
                    "landoLink": lando_link(store, revision)?,
                    "newChangesLink": code::new_changes_link(store, &entry.transactions, revision).await?,
                    "isReadyToLand": is_ready_to_land(revision),
                    "reviewers": reviewers,
                    "subscribers": users.subscribers(store).await?,
                })
            } else {
                json!({
                    "affectedFiles": code::affected_files(store, revision).await?,
                    "landoLink": lando_link(store, revision)?,
                    "newChangesLink": code::new_changes_link(store, &entry.transactions, revision).await?,
                    "isReadyToLand": is_ready_to_land(revision),
                    "reviewers": reviewers,
                    "subscribers": users.subscribers(store).await?,
                })
            }
        }
        METADATA_EDIT => metadata_edited(store, users, entry).await?,
        _ => return Err(Fail::Internal("an unknown event kind".to_owned())),
    };
    Ok(Some((body, pings)))
}

/// The `try` of `buildEvents()`: the contexts of a entry's events.
async fn contexts(
    store: &mut Store<'_>,
    users: &ResolveUsers<'_>,
    entry: &Story,
    secure: bool,
) -> Result<Contexts> {
    let Some((body, pings)) = body(store, users, entry, secure).await? else {
        return Ok(Contexts::Skip);
    };
    let revision = &entry.revision;
    let actor = json!({
        "userName": entry.actor.user_name,
        "realName": entry.actor.real_name,
    });
    let link = production_uri(store.config, &format!("/{}", revision.monogram()))?;
    let context_revision = if secure {
        // SecureEmailRevision requires a bug.
        let (_, bug) = bugzilla::resolve_bug(store, revision)
            .await?
            .ok_or(Fail::Throw("a secure revision without bug"))?;
        json!({"revisionId": revision.id, "link": link, "bug": bug})
    } else {
        let bug = bugzilla::public_bug(store, revision).await?;
        let repository = store
            .repository(
                Some(
                    revision
                        .repository_phid
                        .as_ref()
                        .ok_or(Fail::Throw("a revision without repository"))?,
                ),
                None,
            )
            .await?
            .ok_or(Fail::Throw("a revision whose repository is gone"))?;
        json!({
            "revisionId": revision.id,
            "name": revision.title.replace('\n', " "),
            "link": link,
            "repositoryName": repository.name,
            "bug": bug,
        })
    };
    let context = |kind: &str, body: Value| {
        json!({
            "eventKind": kind,
            "actorName": entry.actor.user_name,
            "actor": actor,
            "revision": context_revision,
            "body": body,
        })
    };
    let mut contexts = vec![context(entry.kind.public_kind, body)];
    let transaction_link = entry.transaction_link(store)?;
    let ping_bodies = match pings {
        Pings::Secure(pings) => {
            pings
                .into_bodies(store, &users.actor_email, &transaction_link)
                .await?
        }
        Pings::Public(pings) => {
            pings
                .into_bodies(store, &users.actor_email, &transaction_link)
                .await?
        }
    };
    for body in ping_bodies {
        contexts.push(context(PINGED, body));
    }
    Ok(Contexts::Built(contexts))
}

/// `EmailEvent` (or `EmailIDEvent`) as a dictionary.
fn event(entry: &Story, with_id: bool, secure: bool, minimal: &Value, context: &Value) -> Value {
    let mut event = json!({
        "key": entry.key,
        "timestamp": entry.timestamp,
        "isSecure": secure,
        "minimalContext": minimal,
        "context": context,
    });
    if with_id {
        event["feedId"] = json!(entry.id);
    }
    event
}

/// `EmailEventBuilder::buildEvents()` of the stories
/// `PhabricatorStory::buildStories()` makes of `stories`.
pub(super) async fn build(
    store: &mut Store<'_>,
    stories: Vec<VisibleStory>,
    with_id: bool,
) -> Result<Vec<Value>> {
    let stories = build_stories(store, stories).await?;
    let mut events = Vec::new();
    for entry in &stories {
        let revision = &entry.revision;
        let projects = store
            .edge_destinations(&revision.phid, phab_db::edges::OBJECT_HAS_PROJECT)
            .await?;
        let secure = if projects.is_empty() {
            false
        } else {
            projects.contains(&store.secure_project().await?)
        };
        let actor_email = store
            .primary_email(&entry.actor.phid)
            .await?
            .ok_or(Fail::Unsupported("an actor without primary email address"))?;
        let users = ResolveUsers {
            revision,
            actor_email,
        };
        let minimal = json!({
            "revision": {
                "revisionId": revision.id,
                "link": production_uri(store.config, &format!("/{}", revision.monogram()))?,
            },
            "recipients": users.all_possible(store).await?,
        });
        match contexts(store, &users, entry, secure).await {
            Ok(Contexts::Built(contexts)) => {
                for context in contexts {
                    events.push(event(entry, with_id, secure, &minimal, &context));
                }
            }
            Ok(Contexts::Skip) => {}
            // PHP reports the error and sends a minimal event, which is
            // not secure.
            Err(Fail::Throw(_)) => {
                events.push(event(entry, with_id, false, &minimal, &Value::Null));
            }
            Err(other) => return Err(other),
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_uri_paths_like_phutil_uri() {
        assert_eq!(
            with_path("https://lando.test", "/D1/").unwrap(),
            "https://lando.test/D1/"
        );
        assert_eq!(
            with_path("https://lando.test/", "/D1/").unwrap(),
            "https://lando.test/D1/"
        );
        assert!(with_path("https://lando.test/x", "/D1/").is_err());
    }
}
