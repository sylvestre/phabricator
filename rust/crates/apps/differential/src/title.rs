//! Feed titles of Differential transactions: `getTitleForFeed()` of
//! `DifferentialTransaction`, its modular transaction types, the core types
//! of `PhabricatorApplicationTransaction` and the custom fields Mozilla
//! adds, with the English plurals of `PhabricatorUSEnglishTranslation`.
//!
//! A title is a list of [`Part`]s: text, and handles which the caller
//! resolves under the viewer's policy and renders as links or text. Titles
//! which depend on something not ported come back as [`Unsupported`].

use phab_core::Phid;
use phab_db::edges;
use serde_json::Value;

use crate::storage::Transaction;

/// A piece of a title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// Plain text.
    Text(String),
    /// `renderHandleLink()`: an object, user or project.
    Handle(Phid),
    /// `renderHandleList()`: handles separated by commas.
    HandleList(Vec<Phid>),
    /// `renderValue()` of a modular type: a quoted value in text.
    Value(String),
}

/// A title, as `pht()` builds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Title(pub Vec<Part>);

impl Title {
    /// Every handle the title shows (`getRequiredHandlePHIDs()`).
    pub fn handles(&self) -> Vec<Phid> {
        let mut phids = Vec::new();
        for part in &self.0 {
            match part {
                Part::Handle(phid) => phids.push(phid.clone()),
                Part::HandleList(list) => phids.extend(list.iter().cloned()),
                Part::Text(_) | Part::Value(_) => {}
            }
        }
        phids
    }

    /// The title as text, naming handles with `name`: what PHP renders with
    /// `TARGET_TEXT`.
    pub fn to_text(&self, name: impl Fn(&Phid) -> String) -> String {
        let mut text = String::new();
        for part in &self.0 {
            match part {
                Part::Text(piece) => text.push_str(piece),
                Part::Handle(phid) => text.push_str(&name(phid)),
                Part::HandleList(list) => {
                    text.push_str(&list.iter().map(&name).collect::<Vec<_>>().join(", "));
                }
                Part::Value(value) => {
                    text.push('"');
                    text.push_str(value);
                    text.push('"');
                }
            }
        }
        text
    }
}

/// A title PHP renders with code that is not ported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unsupported(pub &'static str);

/// What `%s` stands for in a format.
enum Argument {
    Part(Part),
    /// `phutil_count()` or `PhutilNumber`: rendered as the number.
    Count(usize),
}

/// `pht($format, ...)`: `%s` and positional `%N$s` arguments.
fn pht(format: &str, arguments: &[Argument]) -> Title {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut next = 0;
    let mut rest = format;
    while let Some(at) = rest.find('%') {
        text.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let index = if digits > 0 && rest[digits..].starts_with("$s") {
            let index: usize = rest[..digits].parse().expect("digits");
            rest = &rest[digits + 2..];
            index - 1
        } else if let Some(after) = rest.strip_prefix('s') {
            rest = after;
            next += 1;
            next - 1
        } else {
            text.push('%');
            continue;
        };
        match &arguments[index] {
            Argument::Count(count) => text.push_str(&count.to_string()),
            Argument::Part(Part::Text(piece)) => text.push_str(piece),
            Argument::Part(part) => {
                if !text.is_empty() {
                    parts.push(Part::Text(std::mem::take(&mut text)));
                }
                parts.push(part.clone());
            }
        }
    }
    text.push_str(rest);
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    Title(parts)
}

/// Picks the singular or plural form for `count`, as the English
/// translation does.
fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn handle(phid: &Phid) -> Argument {
    Argument::Part(Part::Handle(phid.clone()))
}

fn handle_list(phids: Vec<Phid>) -> Argument {
    Argument::Part(Part::HandleList(phids))
}

fn text(value: &str) -> Argument {
    Argument::Part(Part::Text(value.to_owned()))
}

/// A PHID value of a transaction, which PHP renders as a handle.
fn phid_value(value: &Value) -> Option<Phid> {
    value
        .as_str()
        .filter(|phid| !phid.is_empty())
        .map(Phid::new)
}

/// The keys of a dictionary value, in order (`array_keys()`).
fn keys(value: &Value) -> Result<Vec<String>, Unsupported> {
    match value {
        Value::Object(entries) => Ok(entries.keys().cloned().collect()),
        Value::Array(items) if items.is_empty() => Ok(Vec::new()),
        Value::Null => Ok(Vec::new()),
        _ => Err(Unsupported("reviewer values are not dictionaries")),
    }
}

/// `PhabricatorEdgeChangeRecord`: edge values map destination PHIDs to
/// edge data; the added and removed PHIDs are the keys only on one side.
fn edge_changes(transaction: &Transaction) -> Result<(Vec<Phid>, Vec<Phid>), Unsupported> {
    let edge_keys = |value: &Value| -> Result<Vec<String>, Unsupported> {
        match value {
            Value::Object(entries) => Ok(entries.keys().cloned().collect()),
            Value::Array(items) if items.is_empty() => Ok(Vec::new()),
            Value::Null => Ok(Vec::new()),
            _ => Err(Unsupported("edge values are not dictionaries")),
        }
    };
    let old = edge_keys(&transaction.old_value)?;
    let new = edge_keys(&transaction.new_value)?;
    let added = new
        .iter()
        .filter(|phid| !old.contains(phid))
        .map(Phid::new)
        .collect();
    let removed = old
        .iter()
        .filter(|phid| !new.contains(phid))
        .map(Phid::new)
        .collect();
    Ok((added, removed))
}

/// The edge type of a `core:edge` transaction (`head()` of the metadata).
fn edge_type(transaction: &Transaction) -> Option<i64> {
    let value = transaction.metadata_value("edge:type")?;
    let value = match value {
        Value::Array(items) => items.first()?,
        other => other,
    };
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn truthy(value: Option<&Value>) -> bool {
    value.is_some_and(phab_core::php::is_truthy)
}

/// `DifferentialRevisionStatus` keys whose status transaction has a title.
fn status_title(status: &str) -> Option<&'static str> {
    match status {
        "accepted" => Some("%s is now accepted and ready to land."),
        "needs-revision" => Some("%s now requires changes to proceed."),
        "needs-review" => Some("%s now requires review to proceed."),
        _ => None,
    }
}

/// `getTitleForFeed()` of a Differential transaction. `Ok(None)` where
/// PHP's title is `null`.
pub fn feed_title(transaction: &Transaction) -> Result<Option<Title>, Unsupported> {
    let author = handle(&transaction.author_phid);
    let object = handle(&transaction.object_phid);
    let new = &transaction.new_value;
    let old = &transaction.old_value;
    let simple = |format: &str| {
        Ok(Some(pht(
            format,
            &[
                handle(&transaction.author_phid),
                handle(&transaction.object_phid),
            ],
        )))
    };

    match transaction.transaction_type.as_str() {
        // DifferentialTransaction's own types.
        "differential:inline" => simple("%s added inline comments to %s."),
        "differential:action" => legacy_action(transaction),
        "differential:update" => simple("%s updated the diff for %s."),

        // Modular transaction types.
        "differential.revision.abandon" => simple("%s abandoned %s."),
        "differential.revision.accept" => simple("%s accepted %s."),
        "differential.revision.close" => simple("%s closed %s."),
        "differential.revision.commandeer" => simple("%s commandeered %s."),
        "differential.revision.plan" => simple("%s planned changes to %s."),
        "differential.revision.reclaim" => simple("%s reclaimed %s."),
        "differential.revision.reject" => simple("%s requested changes to %s."),
        "differential.revision.reopen" => simple("%s reopened %s."),
        "differential.revision.resign" => simple("%s resigned from %s."),
        "differential.revision.summary" => simple("%s updated the summary of %s."),
        "differential.revision.testplan" => simple("%s updated the test plan for %s."),
        "differential.revision.request" => {
            let source = transaction
                .metadata_value("promotion.source")
                .and_then(Value::as_str);
            if matches!(source, Some("harbormaster" | "viewer" | "author")) {
                simple("%s published %s for review.")
            } else {
                simple("%s requested review of %s.")
            }
        }
        "draft" => {
            if phab_core::php::is_truthy(new) {
                simple("%s held %s as a draft.")
            } else {
                simple("%s set %s to automatically submit once builds complete.")
            }
        }
        "differential.revision.title" => Ok(Some(pht(
            "%s retitled %s from %s to %s.",
            &[
                author,
                object,
                Argument::Part(Part::Value(phab_core::php::string_cast(old).into_owned())),
                Argument::Part(Part::Value(phab_core::php::string_cast(new).into_owned())),
            ],
        ))),
        "differential.revision.author" => Ok(Some(author_title(transaction))),
        "differential.revision.repository" => Ok(Some(repository_title(transaction))),
        "differential.revision.status" => Ok(new
            .as_str()
            .and_then(status_title)
            .map(|format| pht(format, &[object]))),
        "differential.revision.reviewers" => reviewers_title(transaction).map(Some),
        "harbormaster:buildable" => Err(Unsupported("buildable titles need Harbormaster handles")),

        _ => core_title(transaction),
    }
}

/// `DifferentialRevisionAuthorTransaction::getTitleForFeed()`:
/// commandeering, foisting, or changing the author.
fn author_title(transaction: &Transaction) -> Title {
    let (old, new) = (&transaction.old_value, &transaction.new_value);
    let (author, object) = (
        handle(&transaction.author_phid),
        handle(&transaction.object_phid),
    );

    let old_phid = phid_value(old);
    let new_phid = phid_value(new);
    let author_phid = Some(transaction.author_phid.clone());
    let old_handle = old_phid.as_ref().map_or(text(""), handle);
    let new_handle = new_phid.as_ref().map_or(text(""), handle);
    if author_phid == new_phid {
        pht("%s commandeered %s from %s.", &[author, object, old_handle])
    } else if author_phid == old_phid {
        pht("%s foisted %s upon %s.", &[author, object, new_handle])
    } else {
        pht(
            "%s changed the author of %s from %s to %s.",
            &[author, object, old_handle, new_handle],
        )
    }
}

/// `DifferentialRevisionRepositoryTransaction::getTitleForFeed()`.
fn repository_title(transaction: &Transaction) -> Title {
    let (old, new) = (&transaction.old_value, &transaction.new_value);
    let (author, object) = (
        handle(&transaction.author_phid),
        handle(&transaction.object_phid),
    );

    let old_phid = phid_value(old);
    let new_phid = phid_value(new);
    match (old_phid, new_phid) {
        (Some(old), Some(new)) => pht(
            "%s changed the repository for %s from %s to %s.",
            &[author, object, handle(&old), handle(&new)],
        ),
        (_, Some(new)) => pht(
            "%s set the repository for %s to %s.",
            &[author, object, handle(&new)],
        ),
        (old, None) => pht(
            "%s removed %s as the repository for %s.",
            &[author, old.as_ref().map_or(text(""), handle), object],
        ),
    }
}

/// The core types of `PhabricatorApplicationTransaction::getTitleForFeed()`.
fn core_title(transaction: &Transaction) -> Result<Option<Title>, Unsupported> {
    let simple = |format: &str| {
        Ok(Some(pht(
            format,
            &[
                handle(&transaction.author_phid),
                handle(&transaction.object_phid),
            ],
        )))
    };
    match transaction.transaction_type.as_str() {
        "core:create" => simple("%s created %s."),
        "core:comment" => simple("%s added a comment to %s."),
        "core:view-policy" => simple("%s changed the visibility for %s."),
        "core:edit-policy" => simple("%s changed the edit policy for %s."),
        "core:join-policy" => simple("%s changed the join policy for %s."),
        "core:interact-policy" => simple("%s changed the interact policy for %s."),
        "core:subscribers" => simple("%s updated subscribers of %s."),
        "core:edge" => edge_title(transaction).map(Some),
        "core:customfield" => custom_field_title(transaction).map(Some),
        "core:mfa" => Ok(None),
        _ => Err(Unsupported(
            "feed title of this transaction type is not ported",
        )),
    }
}

/// `DifferentialTransaction::TYPE_ACTION`, from before modular
/// transactions: old stories still have them. Actions without a feed title
/// get the timeline title of `DifferentialAction::getBasicStoryText()`.
fn legacy_action(transaction: &Transaction) -> Result<Option<Title>, Unsupported> {
    let action = transaction.new_value.as_str().unwrap_or("");
    let feed = match action {
        "accept" => Some("%s accepted %s."),
        "reject" => Some("%s requested changes to %s."),
        "rethink" => Some("%s planned changes to %s."),
        "abandon" => Some("%s abandoned %s."),
        "commit" if truthy(transaction.metadata_value("isCommitClose")) => {
            return Err(Unsupported("closing by commit needs commit handles"));
        }
        "commit" => Some("%s closed %s."),
        "request_review" => Some("%s requested review of %s."),
        "reclaim" => Some("%s reclaimed %s."),
        "resign" => Some("%s resigned from %s."),
        "claim" => Some("%s commandeered %s."),
        "reopen" => Some("%s reopened %s."),
        _ => None,
    };
    if let Some(format) = feed {
        return Ok(Some(pht(
            format,
            &[
                handle(&transaction.author_phid),
                handle(&transaction.object_phid),
            ],
        )));
    }
    let basic = match action {
        "none" => "%s commented on this revision.",
        "update" => "%s updated this revision.",
        "summarize" => "%s summarized this revision.",
        "testplan" => "%s explained the test plan for this revision.",
        "create" => "%s created this revision.",
        "add_reviewers" => "%s added reviewers to this revision.",
        "add_ccs" => "%s added CCs to this revision.",
        "differential:inline" => "%s added an inline comment.",
        _ => "Ghosts happened to this revision.",
    };
    Ok(Some(pht(basic, &[handle(&transaction.author_phid)])))
}

/// The strings of a list change: edited (both added and removed), then
/// added and removed, singular and plural.
struct ChangeStrings {
    edit: &'static str,
    add: (&'static str, &'static str),
    remove: (&'static str, &'static str),
}

/// `%s` (author), count, object, then the added and removed lists: the
/// arguments of the edge and reviewer feed strings.
fn change_title(
    transaction: &Transaction,
    added: Vec<Phid>,
    removed: Vec<Phid>,
    strings: &ChangeStrings,
) -> Option<Title> {
    let author = handle(&transaction.author_phid);
    let object = handle(&transaction.object_phid);
    Some(match (added.is_empty(), removed.is_empty()) {
        (false, false) => pht(
            strings.edit,
            &[
                author,
                Argument::Count(added.len() + removed.len()),
                object,
                Argument::Count(added.len()),
                handle_list(added),
                Argument::Count(removed.len()),
                handle_list(removed),
            ],
        ),
        (false, true) => {
            let format = plural(added.len(), strings.add.0, strings.add.1);
            pht(
                format,
                &[
                    author,
                    Argument::Count(added.len()),
                    object,
                    handle_list(added),
                ],
            )
        }
        (true, false) => {
            let format = plural(removed.len(), strings.remove.0, strings.remove.1);
            pht(
                format,
                &[
                    author,
                    Argument::Count(removed.len()),
                    object,
                    handle_list(removed),
                ],
            )
        }
        (true, true) => return None,
    })
}

/// `DifferentialRevisionHasReviewerEdgeType` and the reviewers
/// transaction share these.
const REVIEWER_STRINGS: ChangeStrings = ChangeStrings {
    edit: "%s edited reviewers for %3$s, added: %5$s; removed: %7$s.",
    add: (
        "%s added a reviewer for %3$s: %4$s.",
        "%s added reviewers for %3$s: %4$s.",
    ),
    remove: (
        "%s removed a reviewer for %3$s: %4$s.",
        "%s removed reviewers for %3$s: %4$s.",
    ),
};

/// These have no English translation, so the "(s)" stays.
const BLOCKING_STRINGS: ChangeStrings = ChangeStrings {
    edit: "%s changed %s blocking reviewer(s) for %s, added %s: %s; removed %s: %s.",
    add: (
        "%s added %s blocking reviewer(s) for %s: %s.",
        "%s added %s blocking reviewer(s) for %s: %s.",
    ),
    remove: (
        "%s removed %s blocking reviewer(s) for %s: %s.",
        "%s removed %s blocking reviewer(s) for %s: %s.",
    ),
};

/// `PhabricatorProjectObjectHasProjectEdgeType`.
const PROJECT_STRINGS: ChangeStrings = ChangeStrings {
    edit: "%s edited projects for %3$s, added: %5$s; removed %7$s.",
    add: (
        "%s added a project to %3$s: %4$s.",
        "%s added projects to %3$s: %4$s.",
    ),
    remove: (
        "%s removed a project from %3$s: %4$s.",
        "%s removed projects from %3$s: %4$s.",
    ),
};

/// `DifferentialRevisionReviewersTransaction::renderReviewerEditTitle()`
/// for feed: reviewers added and removed, then those which became blocking
/// or stopped being.
fn reviewers_title(transaction: &Transaction) -> Result<Title, Unsupported> {
    let old_keys = keys(&transaction.old_value)?;
    let new_keys = keys(&transaction.new_value)?;
    let only_in = |these: &[String], those: &[String]| -> Vec<Phid> {
        these
            .iter()
            .filter(|phid| !those.contains(phid))
            .map(Phid::new)
            .collect()
    };
    let changes = change_title(
        transaction,
        only_in(&new_keys, &old_keys),
        only_in(&old_keys, &new_keys),
        &REVIEWER_STRINGS,
    );

    let status = |value: &Value, phid: &str| {
        value
            .get(phid)
            .map(|status| phab_core::php::string_cast(status).into_owned())
    };
    let mut blocks = Vec::new();
    let mut unblocks = Vec::new();
    for phid in &new_keys {
        let Some(old_status) = status(&transaction.old_value, phid)
            .filter(|status| phab_core::php::is_truthy_str(status))
        else {
            continue;
        };
        let was_blocking = old_status == "blocking";
        let now_blocking = status(&transaction.new_value, phid).as_deref() == Some("blocking");
        if now_blocking && !was_blocking {
            blocks.push(Phid::new(phid));
        }
        if !now_blocking && was_blocking {
            unblocks.push(Phid::new(phid));
        }
    }
    let blocking = change_title(transaction, blocks, unblocks, &BLOCKING_STRINGS);

    let mut parts = Vec::new();
    for (index, sentence) in changes.into_iter().chain(blocking).enumerate() {
        if index > 0 {
            parts.push(Part::Text(" ".to_owned()));
        }
        parts.extend(sentence.0);
    }
    Ok(Title(parts))
}

/// The `core:edge` title: the edge type's feed strings.
fn edge_title(transaction: &Transaction) -> Result<Title, Unsupported> {
    let (added, removed) = edge_changes(transaction)?;
    let strings = match edge_type(transaction) {
        Some(edges::OBJECT_HAS_PROJECT) => &PROJECT_STRINGS,
        Some(REVISION_HAS_REVIEWER) => &REVIEWER_STRINGS,
        _ => return Err(Unsupported("feed title of this edge type is not ported")),
    };
    Ok(
        change_title(transaction, added, removed, strings).unwrap_or_else(|| {
            pht(
                "%s edited edge metadata for %s.",
                &[
                    handle(&transaction.author_phid),
                    handle(&transaction.object_phid),
                ],
            )
        }),
    )
}

/// `DifferentialRevisionHasReviewerEdgeType`: reviewers before the
/// reviewer transaction existed.
const REVISION_HAS_REVIEWER: i64 = 35;

/// The custom fields of revisions whose title is known: the Mozilla ones,
/// which can't be disabled.
fn custom_field_title(transaction: &Transaction) -> Result<Title, Unsupported> {
    let author = handle(&transaction.author_phid);
    let object = handle(&transaction.object_phid);
    match transaction
        .metadata_value("customfield:key")
        .and_then(Value::as_str)
    {
        // DifferentialBugzillaBugIDField keeps the default feed title.
        Some("differential:bugzilla-bug-id") => Ok(pht("%s updated %s.", &[author, object])),
        Some("differential:uplift-request") => Ok(pht(
            "%s updated the uplift request field for %s.",
            &[author, object],
        )),
        // DifferentialMergeConflictStatusField doesn't appear in
        // transactions, so PHP finds no field.
        Some("differential:merge-conflict-status") | None => {
            Ok(pht("%s edited a custom field on %s.", &[author, object]))
        }
        Some(_) => Err(Unsupported(
            "this custom field depends on differential.fields",
        )),
    }
}

/// `PhabricatorApplicationTransactionFeedStory::getPrimaryTransactionPHID()`:
/// the first transaction not hidden from feed, or the first one.
pub fn primary_transaction(transactions: &[(Transaction, bool)]) -> Option<&Transaction> {
    transactions
        .iter()
        .find(|(_, hidden)| !hidden)
        .or_else(|| transactions.first())
        .map(|(transaction, _)| transaction)
}

/// `customfield:key` values whose visibility is known (see
/// [`custom_field_title()`]).
pub fn is_known_custom_field(key: Option<&str>) -> bool {
    matches!(
        key,
        None | Some(
            "differential:bugzilla-bug-id"
                | "differential:uplift-request"
                | "differential:merge-conflict-status"
        )
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn formats_take_positional_arguments() {
        let title = pht(
            "%s added a reviewer for %3$s: %4$s.",
            &[text("A"), Argument::Count(1), text("D1"), text("b")],
        );
        assert_eq!(
            title,
            Title(vec![Part::Text("A added a reviewer for D1: b.".to_owned())])
        );
    }

    #[test]
    fn handles_are_collected() {
        let transaction = Transaction {
            phid: Phid::new("PHID-XACT-DREV-1"),
            author_phid: Phid::new("PHID-USER-alice"),
            object_phid: Phid::new("PHID-DREV-1"),
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            comment_phid: None,
            transaction_type: "differential.revision.reviewers".to_owned(),
            old_value: json!({}),
            new_value: json!({"PHID-USER-bob": "added", "PHID-PROJ-x": "added"}),
            metadata: json!({}),
        };
        let title = feed_title(&transaction).unwrap().unwrap();
        assert_eq!(
            title.handles(),
            [
                "PHID-USER-alice",
                "PHID-DREV-1",
                "PHID-USER-bob",
                "PHID-PROJ-x"
            ]
            .map(Phid::new)
        );
    }
}
