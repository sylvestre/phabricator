//! Mozilla's AI change on the revision page (Bug 2091563): the "Approve AI
//! Change" comment action, offered as PHP's comment form does
//! (`DifferentialRevisionAIChangeApproveTransaction::newEditField()`): one
//! checkbox for the pending diff, which starts unticked; and the box above
//! the revision's details which shows the state (`DifferentialAIChangeView`,
//! one of `DifferentialRevisionWarning`'s warnings), with the same texts.
//! The requester's details and the AI summary are folded away under "Show
//! details", the confidence is a tag coloured by how sure the AI is.

use phab_app_differential::ai_change::{self, AiChange, Status};
use phab_app_differential::storage::Revision;
use phab_app_feed::handles::{self, Handle};
use phab_auth::User;
use phab_core::Phid;
use phab_db::{Database, rows};
use phab_policy::Viewer;
use phab_remarkup::Mode;
use serde_json::Value;

use crate::error::PageError;
use crate::remarkup;
use crate::state::WebState;
use crate::viewer::WebUser;

/// The AI change of `revision`, from its `properties`.
pub(crate) async fn load(database: &Database, revision: &Revision) -> Result<AiChange, PageError> {
    let sql = format!(
        "SELECT properties FROM {} WHERE id = ?",
        database.table("differential", "differential_revision")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(revision.id)
        .fetch_optional(database.pool())
        .await
        .map_err(crate::harbormaster::internal)?
        .ok_or(PageError::NotFound)?;
    let properties = rows::json(&row, "properties").map_err(crate::harbormaster::internal)?;
    AiChange::from_properties(&properties).map_err(crate::harbormaster::method_error)
}

/// `isActionAvailable()` for `viewer`: a pending change on an open
/// revision, and a viewer who isn't a bot or a mailing list.
pub(crate) fn can_approve(change: &AiChange, revision: &Revision, viewer: &User) -> bool {
    let active = revision
        .active_diff_phid
        .as_ref()
        .map(phab_core::Phid::as_str);
    !viewer.is_system_agent
        && !viewer.is_mailing_list
        && !revision.is_closed()
        && change.status(active) == Status::Pending
}

/// The diff the checkbox approves, when the viewer may approve: the stored
/// `changeDiffID`, an integer.
pub(crate) fn approvable_diff(
    change: &AiChange,
    revision: &Revision,
    viewer: &User,
) -> Option<i64> {
    if !can_approve(change, revision, viewer) {
        return None;
    }
    change
        .get(ai_change::KEY_CHANGE_DIFF_ID)
        .and_then(Value::as_i64)
}

/// How the box looks (`PHUIInfoView::SEVERITY_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Warning,
    Notice,
    Success,
}

impl Severity {
    /// The CSS class suffix.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Notice => "notice",
            Self::Success => "success",
        }
    }
}

/// The box above the revision's details (`DifferentialAIChangeView`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiChangeBox {
    pub(crate) severity: Severity,
    pub(crate) title: String,
    /// The paragraphs, as HTML.
    pub(crate) paragraphs_html: Vec<String>,
    /// `Confidence: <value>`, and the colour of its tag.
    pub(crate) confidence: Option<(String, &'static str)>,
    /// "About this revision": the details, rendered.
    pub(crate) details_html: Option<String>,
    /// "AI summary", plain text.
    pub(crate) summary: Option<String>,
    /// The comparison of the base and change diffs: its URI and text.
    pub(crate) compare: Option<(String, String)>,
}

impl AiChangeBox {
    /// Whether "Show details" has anything to show.
    pub(crate) fn has_more(&self) -> bool {
        self.details_html.is_some() || self.summary.is_some()
    }
}

/// A stored text, `None` when empty (`strlen($text ?? '')`); other types
/// are left to PHP.
fn text(change: &AiChange, key: &str) -> Result<Option<String>, PageError> {
    match change.get(key) {
        None => Ok(None),
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(PageError::Fallback("an AI change text of an unusual type")),
    }
}

/// A stored diff ID, as PHP prints it.
fn diff_id(change: &AiChange, key: &str) -> Result<Option<i64>, PageError> {
    match change.get(key) {
        None => Ok(None),
        Some(Value::Number(number)) if number.is_i64() => Ok(number.as_i64()),
        Some(_) => Err(PageError::Fallback("an AI change diff of an unusual type")),
    }
}

/// `renderConfidence()`'s colour of a confidence.
fn confidence_color(confidence: &str) -> &'static str {
    match confidence.to_lowercase().as_str() {
        "high" => "green",
        "medium" => "orange",
        "low" => "red",
        _ => "grey",
    }
}

/// The parts of the box which don't need the page: its severity and title,
/// and whether it says who approved the change; `None` without an AI
/// change.
pub(crate) fn heading(change: &AiChange, revision: &Revision) -> Option<(Severity, String, bool)> {
    let active = revision
        .active_diff_phid
        .as_ref()
        .map(phab_core::Phid::as_str);
    let status = change.status(active);
    if status == Status::None {
        return None;
    }
    let kind = ai_change::kind_name(change.get(ai_change::KEY_KIND));
    let approved = change
        .get(ai_change::KEY_APPROVER_PHID)
        .is_some_and(phab_core::php::is_truthy);
    Some(if revision.is_closed() {
        // Landing attaches a new diff, so a closed revision's change looks
        // lapsed: say whether it was approved instead.
        if approved {
            (
                Severity::Success,
                format!("{} approved", ucfirst(kind)),
                true,
            )
        } else {
            (
                Severity::Notice,
                format!("{} not approved", ucfirst(kind)),
                false,
            )
        }
    } else {
        match status {
            Status::Lapsed => (
                Severity::Notice,
                "AI change: approval no longer required".to_owned(),
                approved,
            ),
            Status::Approved => (
                Severity::Success,
                format!("{} approved", ucfirst(kind)),
                true,
            ),
            _ => (
                Severity::Warning,
                format!("This revision has an {kind} to approve"),
                false,
            ),
        }
    })
}

/// `ucfirst()`.
fn ucfirst(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_ascii_uppercase().to_string() + chars.as_str()
    })
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
}

/// `renderApproval()`: who approved the change, `None` if nobody has.
async fn approval_html<S: WebState>(
    state: &S,
    viewer: &Viewer,
    change: &AiChange,
) -> Result<Option<String>, PageError> {
    let Some(approver) = change
        .get(ai_change::KEY_APPROVER_PHID)
        .filter(|value| phab_core::php::is_truthy(value))
    else {
        return Ok(None);
    };
    let approver = approver
        .as_str()
        .filter(|phid| phid.starts_with("PHID-USER-"))
        .ok_or(PageError::Fallback(
            "an AI change approver who isn't a user",
        ))?;
    let phid = Phid::new(approver);
    let handles = handles::load_handles(
        state.database(),
        state.config(),
        viewer,
        std::slice::from_ref(&phid),
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    let handle = match handles.get(&phid) {
        Some(Handle {
            name,
            uri: Some(uri),
        }) => format!("<a href=\"{}\">{}</a>", escape(uri), escape(name)),
        Some(Handle { name, uri: None }) => escape(name),
        None => "Unknown Object".to_owned(),
    };
    let diff = diff_id(change, ai_change::KEY_CHANGE_DIFF_ID)?
        .map(|id| id.to_string())
        .unwrap_or_default();
    Ok(Some(format!(
        "{handle} approved the change in diff {diff}."
    )))
}

/// `DifferentialAIChangeView::render()` for `viewer`, `None` without an
/// AI change.
pub(crate) async fn render_box<S: WebState>(
    state: &S,
    viewer: &WebUser,
    revision: &Revision,
    change: &AiChange,
) -> Result<Option<AiChangeBox>, PageError> {
    let Some((severity, title, shows_approval)) = heading(change, revision) else {
        return Ok(None);
    };
    let as_viewer = viewer.user.viewer();
    let mut paragraphs_html = Vec::new();
    let approval = approval_html(state, &as_viewer, change).await?;
    if revision.is_closed() {
        if approval.is_none() {
            paragraphs_html
                .push("This revision was closed before anyone approved the AI change.".to_owned());
        }
    } else if severity == Severity::Notice {
        paragraphs_html.push(
            "This revision was updated after the AI change, so normal review covers it.".to_owned(),
        );
    } else if severity == Severity::Warning {
        let text = if can_approve(change, revision, &viewer.user) {
            "Check the change, then use \"Approve AI Change\" in the actions below. Lando won't \
             land this revision until someone approves it."
        } else {
            "Lando won't land this revision until someone approves the AI change."
        };
        paragraphs_html.push(escape(text));
    }
    if shows_approval || severity == Severity::Notice {
        paragraphs_html.extend(approval);
    }
    let confidence = text(change, ai_change::KEY_CONFIDENCE)?.map(|confidence| {
        let color = confidence_color(&confidence);
        (format!("Confidence: {confidence}"), color)
    });
    let details_html = match text(change, ai_change::KEY_DETAILS)? {
        Some(details) => {
            let markup = remarkup::render(state, &as_viewer, &details, Mode::Html).await?;
            Some(String::from_utf8_lossy(markup.as_bytes()).into_owned())
        }
        None => None,
    };
    let compare = match (
        diff_id(change, ai_change::KEY_BASE_DIFF_ID)?.filter(|id| *id != 0),
        diff_id(change, ai_change::KEY_CHANGE_DIFF_ID)?,
    ) {
        (Some(base), change_id) => {
            let change_id = change_id.map(|id| id.to_string()).unwrap_or_default();
            Some((
                format!("/D{}?vs={base}&id={change_id}#toc", revision.id),
                format!("View AI changes (diff {base} to {change_id})"),
            ))
        }
        (None, _) => None,
    };
    Ok(Some(AiChangeBox {
        severity,
        title,
        paragraphs_html,
        confidence,
        details_html,
        summary: text(change, ai_change::KEY_SUMMARY)?,
        compare,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn revision(status: &str, diff: &str) -> Revision {
        Revision {
            id: 3,
            phid: Phid::new("PHID-DREV-3"),
            title: String::new(),
            status: status.to_owned(),
            author_phid: None,
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            repository_phid: None,
            active_diff_phid: Some(Phid::new(diff)),
            date_modified: 0,
            reviewers: Vec::new(),
        }
    }

    fn change(kind: &str, approver: &Value) -> AiChange {
        AiChange::from_properties(&json!({"mozilla.ai-change": {
            "kind": kind, "changeDiffID": 2, "changeDiffPHID": "PHID-DIFF-2",
            "approverPHID": approver,
        }}))
        .unwrap()
    }

    #[test]
    fn headings_follow_php() {
        let open = revision("needs-review", "PHID-DIFF-2");
        let updated = revision("needs-review", "PHID-DIFF-9");
        let closed = revision("published", "PHID-DIFF-9");
        let pending = change("requested-edit", &Value::Null);
        let approved = change("merge-conflict", &json!("PHID-USER-a"));
        assert_eq!(heading(&AiChange::default(), &open), None);
        assert_eq!(
            heading(&pending, &open),
            Some((
                Severity::Warning,
                "This revision has an AI edit to approve".to_owned(),
                false
            ))
        );
        assert_eq!(
            heading(&approved, &open),
            Some((
                Severity::Success,
                "AI merge conflict resolution approved".to_owned(),
                true
            ))
        );
        assert_eq!(
            heading(&approved, &updated),
            Some((
                Severity::Notice,
                "AI change: approval no longer required".to_owned(),
                true
            ))
        );
        assert_eq!(
            heading(&approved, &closed),
            Some((
                Severity::Success,
                "AI merge conflict resolution approved".to_owned(),
                true
            ))
        );
        assert_eq!(
            heading(&pending, &closed),
            Some((Severity::Notice, "AI edit not approved".to_owned(), false))
        );
        assert_eq!(
            heading(&change("vibes", &Value::Null), &open).map(|(_, title, _)| title),
            Some("This revision has an AI change to approve".to_owned())
        );
    }

    #[test]
    fn confidence_colors_follow_php() {
        assert_eq!(confidence_color("High"), "green");
        assert_eq!(confidence_color("medium"), "orange");
        assert_eq!(confidence_color("LOW"), "red");
        assert_eq!(confidence_color("sure"), "grey");
    }
}
