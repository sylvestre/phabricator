//! What the publish worker asks of each Differential transaction
//! (`DifferentialTransaction`, its modular types and
//! `PhabricatorApplicationTransaction`): whether mail and feed hide it, its
//! strength and action name, its mail tags, and its mail title and body.

use phab_app_differential::feed::{self as visibility, FeedVisibility};
use phab_app_differential::storage::{Comment, Transaction};
use phab_core::php;
use phab_db::edges;
use serde_json::Value;

use crate::Error;

/// `DifferentialTransaction::MAILTAG_*`.
pub(crate) mod tag {
    pub(crate) const REVIEWERS: &str = "differential-reviewers";
    pub(crate) const CLOSED: &str = "differential-committed";
    pub(crate) const CC: &str = "differential-cc";
    pub(crate) const COMMENT: &str = "differential-comment";
    pub(crate) const UPDATED: &str = "differential-updated";
    pub(crate) const REVIEW_REQUEST: &str = "differential-review-request";
    pub(crate) const OTHER: &str = "differential-other";
}

/// The revision actions (`DifferentialRevisionActionTransaction`), with
/// their action names; they all have strength 300.
const ACTIONS: &[(&str, &str)] = &[
    ("differential.revision.accept", "Accepted"),
    ("differential.revision.reject", "Requested Changes"),
    ("differential.revision.abandon", "Abandoned"),
    ("differential.revision.reclaim", "Reclaimed"),
    ("differential.revision.reopen", "Reopened"),
    ("differential.revision.resign", "Resigned"),
    ("differential.revision.commandeer", "Commandeered"),
    ("differential.revision.plan", "Planned Changes"),
    ("differential.revision.request", "Requested Review"),
    ("differential.revision.close", "Closed"),
    // Mozilla's (Bug 2091563).
    ("mozilla.ai-change.approve", "Approved AI Change"),
];

/// A loaded transaction, with its comment.
#[derive(Debug, Clone)]
pub(crate) struct Xaction {
    /// The row.
    pub(crate) row: Transaction,
    /// The comment, when it has one.
    pub(crate) comment: Option<Comment>,
}

/// A mail title: plain text, or HTML (when `pht()` had a safe HTML
/// argument, everything in it is escaped, even in text mail).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Title {
    /// The title, escaped when `is_html`.
    pub(crate) text: String,
    /// Whether it is HTML.
    pub(crate) is_html: bool,
}

/// `pht()`: `%s` and `%d` take the next argument, `%N$s` the Nth one.
fn pht(format: &str, arguments: &[String]) -> String {
    let mut result = String::new();
    let mut next = 0;
    let mut rest = format;
    while let Some(at) = rest.find('%') {
        result.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        if let Some(after) = rest.strip_prefix('%') {
            result.push('%');
            rest = after;
            continue;
        }
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let index = if digits > 0
            && (rest[digits..].starts_with("$s") || rest[digits..].starts_with("$d"))
        {
            let index: usize = rest[..digits].parse().expect("digits");
            rest = &rest[digits + 2..];
            index - 1
        } else if let Some(after) = rest.strip_prefix('s').or_else(|| rest.strip_prefix('d')) {
            rest = after;
            next += 1;
            next - 1
        } else {
            result.push('%');
            continue;
        };
        result.push_str(arguments.get(index).map_or("", String::as_str));
    }
    result.push_str(rest);
    result
}

/// The singular or plural English form.
fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

/// A list of strings of a transaction value (`array_diff()` compares
/// them as strings).
fn values(value: &Value) -> Result<Vec<String>, Error> {
    let bad = || Error::unsupported("transaction values which are not lists of strings");
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(bad))
            .collect(),
        Value::Object(entries) => entries
            .values()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(bad))
            .collect(),
        _ => Err(bad()),
    }
}

/// The keys of a dictionary value, in order.
fn keys(value: &Value) -> Result<Vec<String>, Error> {
    match value {
        Value::Object(entries) => Ok(entries.keys().cloned().collect()),
        Value::Array(items) if items.is_empty() => Ok(Vec::new()),
        Value::Null => Ok(Vec::new()),
        _ => Err(Error::unsupported(
            "reviewer values which are not dictionaries",
        )),
    }
}

/// `array_diff($a, $b)`: the values of `a` not in `b`, in order.
fn minus(a: &[String], b: &[String]) -> Vec<String> {
    a.iter()
        .filter(|value| !b.contains(value))
        .cloned()
        .collect()
}

/// What resolves handles while rendering titles: `getLinkName()` of each.
pub(crate) type Names<'a> = dyn Fn(&str) -> Result<String, Error> + 'a;

impl Xaction {
    fn kind(&self) -> &str {
        &self.row.transaction_type
    }

    /// `isInlineCommentTransaction()`.
    pub(crate) fn is_inline(&self) -> bool {
        self.kind() == "differential:inline"
    }

    fn is_self_subscription(&self) -> Result<bool, Error> {
        visibility::is_self_subscription(&self.row)
            .ok_or_else(|| Error::unsupported("subscriber values which are not lists"))
    }

    /// `getActionStrength()`.
    pub(crate) fn strength(&self) -> Result<i64, Error> {
        let kind = self.kind();
        if self.is_inline() {
            return Ok(25);
        }
        if ACTIONS.iter().any(|(action, _)| *action == kind) || kind == "differential:action" {
            return Ok(300);
        }
        Ok(match kind {
            "differential:update" => 200,
            "differential.revision.wrong" | "differential.builds.wrong" => 400,
            "core:comment" => 50,
            "core:subscribers" if self.is_self_subscription()? => 25,
            "core:subscribers" => 75,
            "core:mfa" => 1000,
            _ => 100,
        })
    }

    /// `getActionName()`.
    pub(crate) fn action_name(&self) -> Result<&'static str, Error> {
        let kind = self.kind();
        if let Some((_, name)) = ACTIONS.iter().find(|(action, _)| *action == kind) {
            return Ok(name);
        }
        Ok(match kind {
            "differential:inline" | "core:comment" => "Commented On",
            "differential:update" => {
                if php::is_truthy(
                    self.row
                        .metadata_value("core.create")
                        .unwrap_or(&Value::Null),
                ) {
                    "Request"
                } else {
                    "Updated"
                }
            }
            "differential:action" | "harbormaster:buildable" => {
                return Err(Error::unsupported(
                    "the action name of this transaction type",
                ));
            }
            "core:view-policy"
            | "core:edit-policy"
            | "core:join-policy"
            | "core:interact-policy" => "Changed Policy",
            "core:subscribers" => "Changed Subscribers",
            _ => "Updated",
        })
    }

    /// `DifferentialTransaction::getMailTags()`.
    pub(crate) fn mail_tags(&self) -> &'static str {
        match self.kind() {
            "core:subscribers" => tag::CC,
            "differential:action" => {
                if self.row.new_value.as_str() == Some("commit") {
                    tag::CLOSED
                } else {
                    tag::OTHER
                }
            }
            "differential:update" => {
                if self.row.old_value.is_null() {
                    tag::REVIEW_REQUEST
                } else {
                    tag::UPDATED
                }
            }
            "core:comment" | "differential:inline" => tag::COMMENT,
            "differential.revision.reviewers" => tag::REVIEWERS,
            "differential.revision.close" => tag::CLOSED,
            _ => tag::OTHER,
        }
    }

    /// `shouldHideForFeed()`, which `shouldHideForNotifications()` follows
    /// for every Differential type.
    pub(crate) fn hidden_for_feed(&self) -> Result<bool, Error> {
        match visibility::feed_visibility(&self.row, self.comment.as_ref()) {
            FeedVisibility::Visible => Ok(false),
            FeedVisibility::Hidden => Ok(true),
            FeedVisibility::Unknown => Err(Error::unsupported(
                "the feed visibility of this transaction",
            )),
        }
    }

    /// `shouldHideForMail()`.
    pub(crate) fn hidden_for_mail(&self) -> Result<bool, Error> {
        let kind = self.kind();
        match kind {
            // DifferentialTransaction shows reviewer edits, even on creation.
            "differential.revision.reviewers" => return Ok(false),
            "harbormaster:buildable" => {
                return Err(Error::unsupported("mail about a buildable"));
            }
            _ => {}
        }
        if self.is_self_subscription()? {
            return Ok(true);
        }
        match kind {
            "token:give" => return Ok(true),
            "core:edge" => match visibility::transaction_edge_type(&self.row) {
                Some(
                    edges::OBJECT_MENTIONS_OBJECT
                    | edges::OBJECT_MENTIONED_BY_OBJECT
                    | edges::REVISION_DEPENDS_ON_REVISION
                    | edges::REVISION_DEPENDED_ON_BY_REVISION
                    | edges::COMMIT_HAS_REVISION
                    | edges::REVISION_HAS_COMMIT,
                ) => return Ok(true),
                Some(edges::OBJECT_HAS_PROJECT) => return Ok(false),
                _ => {}
            },
            _ => {}
        }
        if self.is_inline() {
            return Err(Error::unsupported("mail about inline comments"));
        }
        // DifferentialTransaction::shouldHide(), then the modular type's.
        match kind {
            "differential.revision.request" => Ok(false),
            "differential.revision.void" => Ok(true),
            _ => match visibility::should_hide(&self.row, self.comment.as_ref()) {
                FeedVisibility::Visible => Ok(false),
                FeedVisibility::Hidden => Ok(true),
                FeedVisibility::Unknown => Err(Error::unsupported(
                    "the mail visibility of this transaction",
                )),
            },
        }
    }

    /// `getBodyForMail()`: the comment, unless it is empty.
    pub(crate) fn mail_body(&self) -> Option<&str> {
        if self.is_inline() {
            return None;
        }
        self.comment
            .as_ref()
            .map(|comment| comment.content.as_str())
            .filter(|content| !content.is_empty())
    }

    /// `getTitleForTextMail()`, which `getTitleForHTMLMail()` shares for
    /// every type ported here (none has change details): `None` where
    /// PHP's title is `null`.
    pub(crate) fn mail_title(&self, name: &Names<'_>) -> Result<Option<Title>, Error> {
        let plain = |text: String| {
            Ok(Some(Title {
                text,
                is_html: false,
            }))
        };
        let author = || name(self.row.author_phid.as_str());
        let simple = |format: &str| plain(pht(format, &[author()?]));
        match self.kind() {
            "core:comment" => simple("%s added a comment."),
            "core:subscribers" => self.subscribers_title(name),
            "differential.revision.accept" => match &self.row.new_value {
                Value::Array(items) if !items.is_empty() => {
                    let phids = values(&self.row.new_value)?;
                    let list = handle_list(&phids, name)?;
                    Ok(Some(html_title(&pht(
                        "%s accepted this revision as %s reviewer(s): %s.",
                        &[author()?, phids.len().to_string(), list],
                    ))))
                }
                Value::Object(entries) if !entries.is_empty() => Err(Error::unsupported(
                    "an accept on behalf of reviewers keyed by something",
                )),
                _ => simple("%s accepted this revision."),
            },
            "differential.revision.reject" => simple("%s requested changes to this revision."),
            "differential.revision.abandon" => simple("%s abandoned this revision."),
            "differential.revision.reclaim" => simple("%s reclaimed this revision."),
            "differential.revision.reopen" => simple("%s reopened this revision."),
            "differential.revision.resign" => simple("%s resigned from this revision."),
            "differential.revision.commandeer" => simple("%s commandeered this revision."),
            "differential.revision.plan" => {
                if php::is_truthy(
                    self.row
                        .metadata_value("draft.demote")
                        .unwrap_or(&Value::Null),
                ) {
                    simple(
                        "%s returned this revision to the author for changes because remote \
                         builds failed.",
                    )
                } else {
                    simple("%s planned changes to this revision.")
                }
            }
            "differential.revision.status" => Ok(match self.row.new_value.as_str() {
                Some("accepted") => Some("This revision is now accepted and ready to land."),
                Some("needs-revision") => Some("This revision now requires changes to proceed."),
                Some("needs-review") => Some("This revision now requires review to proceed."),
                _ => None,
            }
            .map(|text| Title {
                text: text.to_owned(),
                is_html: false,
            })),
            "differential.revision.reviewers" => self.reviewers_title(name).map(Some),
            _ => Err(Error::unsupported(format!(
                "the mail title of {} transactions",
                self.kind()
            ))),
        }
    }

    /// The `TYPE_SUBSCRIBERS` title of `PhabricatorApplicationTransaction`.
    fn subscribers_title(&self, name: &Names<'_>) -> Result<Option<Title>, Error> {
        let old = values(&self.row.old_value)?;
        let new = values(&self.row.new_value)?;
        let added = minus(&new, &old);
        let removed = minus(&old, &new);
        let list = |phids: &[String]| handle_list(phids, name);
        let author = name(self.row.author_phid.as_str())?;
        let text = match (added.is_empty(), removed.is_empty()) {
            (false, false) => pht(
                "%s edited subscribers, added: %3$s; removed: %5$s.",
                &[
                    author,
                    added.len().to_string(),
                    list(&added)?,
                    removed.len().to_string(),
                    list(&removed)?,
                ],
            ),
            (false, true) => pht(
                plural(
                    added.len(),
                    "%s added a subscriber: %3$s.",
                    "%s added subscribers: %3$s.",
                ),
                &[author, added.len().to_string(), list(&added)?],
            ),
            (true, false) => pht(
                plural(
                    removed.len(),
                    "%s removed a subscriber: %3$s.",
                    "%s removed subscribers: %3$s.",
                ),
                &[author, removed.len().to_string(), list(&removed)?],
            ),
            (true, true) => pht("%s updated subscribers...", &[author]),
        };
        Ok(Some(Title {
            text,
            is_html: false,
        }))
    }

    /// `DifferentialRevisionReviewersTransaction::renderReviewerEditTitle()`
    /// for mail: reviewers added and removed, then those which became
    /// blocking or stopped being, joined by a space.
    fn reviewers_title(&self, name: &Names<'_>) -> Result<Title, Error> {
        let old = &self.row.old_value;
        let new = &self.row.new_value;
        let old_keys = keys(old)?;
        let new_keys = keys(new)?;
        let removed = minus(&old_keys, &new_keys);
        let added = minus(&new_keys, &old_keys);
        let author = name(self.row.author_phid.as_str())?;
        let mut parts = Vec::new();
        let list = |phids: &[String]| handle_list(phids, name);
        match (added.is_empty(), removed.is_empty()) {
            (false, false) => parts.push(pht(
                "%s edited reviewers, added: %4$s; removed: %6$s.",
                &[
                    author.clone(),
                    (added.len() + removed.len()).to_string(),
                    added.len().to_string(),
                    list(&added)?,
                    removed.len().to_string(),
                    list(&removed)?,
                ],
            )),
            (false, true) => parts.push(pht(
                plural(
                    added.len(),
                    "%s added a reviewer: %3$s.",
                    "%s added reviewers: %3$s.",
                ),
                &[author.clone(), added.len().to_string(), list(&added)?],
            )),
            (true, false) => parts.push(pht(
                plural(
                    removed.len(),
                    "%s removed a reviewer: %3$s.",
                    "%s removed reviewers: %3$s.",
                ),
                &[author.clone(), removed.len().to_string(), list(&removed)?],
            )),
            (true, true) => {}
        }

        let status = |value: &Value, phid: &str| {
            value
                .get(phid)
                .map(|status| php::string_cast(status).into_owned())
        };
        let mut blocks = Vec::new();
        let mut unblocks = Vec::new();
        for phid in &new_keys {
            let Some(old_status) = status(old, phid).filter(|status| php::is_truthy_str(status))
            else {
                continue;
            };
            let was_blocking = old_status == "blocking";
            let now_blocking = status(new, phid).as_deref() == Some("blocking");
            if now_blocking && !was_blocking {
                blocks.push(phid.clone());
            }
            if !now_blocking && was_blocking {
                unblocks.push(phid.clone());
            }
        }
        // These have no English translation, so the "(s)" stays.
        match (blocks.is_empty(), unblocks.is_empty()) {
            (false, false) => parts.push(pht(
                "%s changed %s blocking reviewer(s), added %s: %s; removed %s: %s.",
                &[
                    author,
                    (blocks.len() + unblocks.len()).to_string(),
                    blocks.len().to_string(),
                    list(&blocks)?,
                    unblocks.len().to_string(),
                    list(&unblocks)?,
                ],
            )),
            (false, true) => parts.push(pht(
                "%s added %s blocking reviewer(s): %s.",
                &[author, blocks.len().to_string(), list(&blocks)?],
            )),
            (true, false) => parts.push(pht(
                "%s removed %s blocking reviewer(s): %s.",
                &[author, unblocks.len().to_string(), list(&unblocks)?],
            )),
            (true, true) => {}
        }
        // Every sentence has a handle list, so they are all HTML.
        Ok(html_title(&parts.join(" ")))
    }
}

/// The link names of handles joined by commas, as both
/// `renderHandleList()` render them in text mode.
fn handle_list(phids: &[String], name: &Names<'_>) -> Result<String, Error> {
    let names = phids
        .iter()
        .map(|phid| name(phid))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names.join(", "))
}

/// A title `pht()` made HTML: its format and arguments escaped.
fn html_title(text: &str) -> Title {
    Title {
        text: crate::html::escape(text),
        is_html: true,
    }
}

/// `getStrongestAction()`: the first transaction of the highest strength.
pub(crate) fn strongest(xactions: &[Xaction]) -> Result<Option<&Xaction>, Error> {
    let mut best: Option<(&Xaction, i64)> = None;
    for xaction in xactions {
        let strength = xaction.strength()?;
        if best.is_none_or(|(_, top)| strength > top) {
            best = Some((xaction, strength));
        }
    }
    Ok(best.map(|(xaction, _)| xaction))
}

/// `msortv($xactions, 'newActionStrengthSortVector')`: strongest first,
/// stable.
pub(crate) fn by_strength(xactions: &[&Xaction]) -> Result<Vec<usize>, Error> {
    let mut keyed = Vec::new();
    for (index, xaction) in xactions.iter().enumerate() {
        keyed.push((-xaction.strength()?, index));
    }
    keyed.sort_by_key(|(strength, index)| (*strength, *index));
    Ok(keyed.into_iter().map(|(_, index)| index).collect())
}

#[cfg(test)]
mod tests {
    use phab_core::Phid;
    use serde_json::json;

    use super::*;

    fn xaction(kind: &str, author: &str, old: Value, new: Value) -> Xaction {
        Xaction {
            row: Transaction {
                phid: Phid::new("PHID-XACT-DREV-1"),
                author_phid: Phid::new(author),
                object_phid: Phid::new("PHID-DREV-1"),
                view_policy: "public".to_owned(),
                edit_policy: author.to_owned(),
                comment_phid: None,
                transaction_type: kind.to_owned(),
                old_value: old,
                new_value: new,
                metadata: json!({}),
            },
            comment: None,
        }
    }

    #[expect(clippy::unnecessary_wraps, reason = "the signature of Names")]
    fn name(phid: &str) -> Result<String, Error> {
        Ok(phid.trim_start_matches("PHID-USER-").to_owned())
    }

    #[test]
    fn formats_take_positional_arguments() {
        assert_eq!(
            pht(
                "%s added a reviewer: %3$s.",
                &["a".into(), "1".into(), "b".into()]
            ),
            "a added a reviewer: b."
        );
        assert_eq!(pht("%s and %d%%", &["a".into(), "2".into()]), "a and 2%");
    }

    #[test]
    fn titles_reviewer_edits() {
        let edit = xaction(
            "differential.revision.reviewers",
            "PHID-USER-alice",
            json!({"PHID-USER-bob": "added", "PHID-USER-carol": "added"}),
            json!({"PHID-USER-bob": "blocking", "PHID-USER-dave": "added"}),
        );
        assert_eq!(
            edit.mail_title(&name).unwrap().unwrap(),
            Title {
                text: "alice edited reviewers, added: dave; removed: carol. alice added 1 \
                       blocking reviewer(s): bob."
                    .to_owned(),
                is_html: true,
            }
        );
    }

    #[test]
    fn titles_subscriber_edits() {
        let added = xaction(
            "core:subscribers",
            "PHID-USER-alice",
            json!(["PHID-USER-bob"]),
            json!(["PHID-USER-bob", "PHID-USER-carol", "PHID-USER-dave"]),
        );
        assert_eq!(
            added.mail_title(&name).unwrap().unwrap().text,
            "alice added subscribers: carol, dave."
        );
        assert_eq!(added.strength().unwrap(), 75);
        let own = xaction(
            "core:subscribers",
            "PHID-USER-alice",
            json!([]),
            json!(["PHID-USER-alice"]),
        );
        assert!(own.hidden_for_mail().unwrap());
        assert_eq!(own.strength().unwrap(), 25);
    }

    #[test]
    fn picks_the_first_strongest_action() {
        let xactions = [
            xaction("core:comment", "PHID-USER-a", json!(null), json!(null)),
            xaction(
                "differential.revision.status",
                "PHID-USER-a",
                json!("x"),
                json!("accepted"),
            ),
            xaction(
                "differential.revision.accept",
                "PHID-USER-a",
                json!(false),
                json!(true),
            ),
            xaction(
                "differential.revision.void",
                "PHID-USER-a",
                json!(null),
                json!(true),
            ),
        ];
        let best = strongest(&xactions).unwrap().unwrap();
        assert_eq!(best.action_name().unwrap(), "Accepted");
        let refs: Vec<&Xaction> = xactions.iter().collect();
        assert_eq!(by_strength(&refs).unwrap(), [2, 1, 3, 0]);
    }
}
