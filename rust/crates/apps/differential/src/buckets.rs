//! The review queue's groups: `DifferentialRevisionRequiredActionResultBucket`
//! ("Bucket by Required Action") and the two variants moz-extensions adds
//! (Bug 2085987): one which also lists revisions needing changes the viewer
//! hasn't acted on, and one which also leaves out what the viewer already
//! accepted.

use std::collections::HashSet;

use phab_core::Phid;

use crate::storage::Revision;

/// Which bucket groups the revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    /// `action`.
    RequiredAction,
    /// `action-with-needs-changes`.
    WithNeedsChanges,
    /// `action-not-accepted`.
    NotAccepted,
}

impl Bucket {
    /// From the `bucket` parameter.
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "action" => Some(Self::RequiredAction),
            "action-with-needs-changes" => Some(Self::WithNeedsChanges),
            "action-not-accepted" => Some(Self::NotAccepted),
            _ => None,
        }
    }

    /// The `bucket` parameter.
    pub fn key(self) -> &'static str {
        match self {
            Self::RequiredAction => "action",
            Self::WithNeedsChanges => "action-with-needs-changes",
            Self::NotAccepted => "action-not-accepted",
        }
    }

    /// `getResultBucketName()`.
    pub fn name(self) -> &'static str {
        match self {
            Self::RequiredAction => "Bucket by Required Action",
            Self::WithNeedsChanges => "Bucket by Required Action (Include Needs Changes)",
            Self::NotAccepted => "Bucket by Required Action (Exclude Accepted)",
        }
    }
}

/// A group of the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// Its name, like "Must Review".
    pub name: &'static str,
    /// What to say when it is empty; `None` for "Other Revisions", which
    /// only appears when not empty.
    pub no_data: Option<&'static str>,
    /// The revisions, in the order they were given.
    pub revisions: Vec<Revision>,
}

/// The revisions not grouped yet, keeping their order.
struct Remaining<'a> {
    revisions: Vec<Option<Revision>>,
    responsible: &'a HashSet<Phid>,
    viewer: &'a Phid,
    sticky_accept: bool,
}

const BLOCKING: &[&str] = &["blocking", "rejected", "rejected-older"];
const REVIEWING: &[&str] = &["added", "commented", "accepted"];
const INACTIVE: &[&str] = &["added", "commented"];

impl Remaining<'_> {
    fn is_authored(&self, revision: &Revision) -> bool {
        revision
            .author_phid
            .as_ref()
            .is_some_and(|author| self.responsible.contains(author))
    }

    /// `hasReviewersWithStatus()`. With `include_voided`, an accept counts
    /// only if it is voided (`Some(true)`) or not (`Some(false)`).
    fn has_reviewers_with_status(
        &self,
        revision: &Revision,
        statuses: &[&str],
        include_voided: Option<bool>,
    ) -> bool {
        revision.reviewers.iter().any(|reviewer| {
            if !self.responsible.contains(&reviewer.phid)
                || !statuses.contains(&reviewer.status.as_str())
            {
                return false;
            }
            match include_voided {
                Some(voided) if reviewer.status == "accepted" => {
                    reviewer.voided_phid.is_some() == voided
                }
                _ => true,
            }
        })
    }

    /// Takes out the revisions `keep` selects, in order.
    fn take(&mut self, mut keep: impl FnMut(&Self, &Revision) -> bool) -> Vec<Revision> {
        let mut taken = Vec::new();
        for index in 0..self.revisions.len() {
            let selected = self.revisions[index]
                .as_ref()
                .is_some_and(|revision| keep(self, revision));
            if selected && let Some(revision) = self.revisions[index].take() {
                taken.push(revision);
            }
        }
        taken
    }

    /// `getRevisionsUnderReview()` with a reviewer test: revisions needing
    /// review, not authored by the responsible users.
    fn under_review(&self, revision: &Revision) -> bool {
        !self.is_authored(revision) && revision.status == "needs-review"
    }

    fn must_or_should_review(
        &mut self,
        statuses: &'static [&'static str],
        include_voided: Option<bool>,
        bucket: Bucket,
    ) -> Vec<Revision> {
        let held = if bucket == Bucket::NotAccepted {
            self.hold_accepted_by_viewer()
        } else {
            Vec::new()
        };
        let taken = if bucket == Bucket::RequiredAction {
            self.take(|this, revision| {
                this.under_review(revision)
                    && this.has_reviewers_with_status(revision, statuses, include_voided)
            })
        } else {
            // `filterWithNeedsRevision()`: also revisions needing changes
            // the responsible reviewers haven't acted on.
            self.take(|this, revision| {
                if this.is_authored(revision) {
                    return false;
                }
                match revision.status.as_str() {
                    "needs-review" => {
                        this.has_reviewers_with_status(revision, statuses, Some(true))
                    }
                    "needs-revision" => this.has_reviewers_with_status(revision, INACTIVE, None),
                    _ => false,
                }
            })
        };
        self.restore(held);
        taken
    }

    /// `holdAcceptedByViewer()`: what the viewer accepted, on the current
    /// diff, is set aside while the review groups are built.
    fn hold_accepted_by_viewer(&mut self) -> Vec<(usize, Revision)> {
        let mut held = Vec::new();
        for index in 0..self.revisions.len() {
            let accepted = self.revisions[index]
                .as_ref()
                .is_some_and(|revision| self.is_accepted_by_viewer(revision));
            if accepted && let Some(revision) = self.revisions[index].take() {
                held.push((index, revision));
            }
        }
        held
    }

    /// `restoreHeld()`: back in their original places.
    fn restore(&mut self, held: Vec<(usize, Revision)>) {
        for (index, revision) in held {
            self.revisions[index] = Some(revision);
        }
    }

    /// `isAcceptedBy()`: the viewer's accept stands, and is of the current
    /// diff even with sticky accepts.
    fn is_accepted_by_viewer(&self, revision: &Revision) -> bool {
        let diff = revision.active_diff_phid.as_ref();
        revision
            .reviewers
            .iter()
            .find(|reviewer| reviewer.phid == *self.viewer)
            .is_some_and(|reviewer| {
                reviewer.is_accepted(diff, self.sticky_accept)
                    && match (diff, &reviewer.last_action_diff_phid) {
                        (Some(diff), Some(action)) => diff == action,
                        _ => true,
                    }
            })
    }
}

/// `buildResultGroups()`: groups `revisions` for the `responsible` users
/// and projects, as seen by `viewer`.
pub fn group(
    revisions: Vec<Revision>,
    responsible: &[Phid],
    viewer: &Phid,
    bucket: Bucket,
    sticky_accept: bool,
) -> Vec<Group> {
    let responsible: HashSet<Phid> = responsible.iter().cloned().collect();
    let mut remaining = Remaining {
        revisions: revisions.into_iter().map(Some).collect(),
        responsible: &responsible,
        viewer,
        sticky_accept,
    };
    // Revisions the responsible reviewers resigned from, and other people's
    // drafts, are not shown.
    remaining.take(|this, revision| {
        !this.is_authored(revision) && this.has_reviewers_with_status(revision, &["resigned"], None)
    });
    remaining.take(|this, revision| !this.is_authored(revision) && revision.status == "draft");

    let group = |name, no_data, revisions| Group {
        name,
        no_data: Some(no_data),
        revisions,
    };
    let mut groups = vec![
        group(
            "Must Review",
            "No revisions are blocked on your review.",
            remaining.must_or_should_review(BLOCKING, None, bucket),
        ),
        group(
            "Ready to Review",
            "No revisions are waiting on you to review them.",
            remaining.must_or_should_review(REVIEWING, Some(true), bucket),
        ),
        group(
            "Ready to Land",
            "No revisions are ready to land.",
            remaining
                .take(|this, revision| this.is_authored(revision) && revision.status == "accepted"),
        ),
        group(
            "Ready to Update",
            "No revisions are waiting for updates.",
            remaining.take(|this, revision| {
                this.is_authored(revision)
                    && matches!(
                        revision.status.as_str(),
                        "needs-revision" | "changes-planned"
                    )
            }),
        ),
        group(
            "Drafts",
            "You have no draft revisions.",
            remaining
                .take(|this, revision| this.is_authored(revision) && revision.status == "draft"),
        ),
        group(
            "Waiting on Review",
            "None of your revisions are waiting on review.",
            remaining.take(|this, revision| {
                this.is_authored(revision) && revision.status == "needs-review"
            }),
        ),
        group(
            "Waiting on Authors",
            "No revisions are waiting on author action.",
            remaining.take(|this, revision| {
                !this.is_authored(revision)
                    && matches!(
                        revision.status.as_str(),
                        "accepted" | "needs-revision" | "changes-planned"
                    )
            }),
        ),
        group(
            "Waiting on Other Reviewers",
            "No revisions are waiting for other reviewers.",
            remaining.take(|this, revision| {
                !this.is_authored(revision) && revision.status == "needs-review"
            }),
        ),
    ];
    let others: Vec<Revision> = remaining.revisions.into_iter().flatten().collect();
    if !others.is_empty() {
        groups.push(Group {
            name: "Other Revisions",
            no_data: None,
            revisions: others,
        });
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Reviewer;

    // The cases of DifferentialRevisionRequiredActionNotAcceptedResultBucketTestCase.
    const VIEWER: &str = "PHID-USER-viewer";
    const AUTHOR: &str = "PHID-USER-author";
    const OTHER: &str = "PHID-USER-other";
    const GROUP: &str = "PHID-PROJ-group";
    const DIFF_OLD: &str = "PHID-DIFF-old";
    const DIFF_NEW: &str = "PHID-DIFF-new";

    fn revision(id: i64, reviewers: Vec<Reviewer>) -> Revision {
        Revision {
            id,
            phid: Phid::new(format!("PHID-DREV-{id}")),
            title: String::new(),
            status: "needs-review".to_owned(),
            author_phid: Some(Phid::new(AUTHOR)),
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            repository_phid: None,
            active_diff_phid: Some(Phid::new(DIFF_NEW)),
            date_modified: 0,
            reviewers,
        }
    }

    fn reviewer(phid: &str, status: &str) -> Reviewer {
        Reviewer::new(Phid::new(phid), status)
    }

    fn accept(phid: &str, diff: &str) -> Reviewer {
        let mut reviewer = reviewer(phid, "accepted");
        reviewer.last_action_diff_phid = Some(Phid::new(diff));
        reviewer
    }

    fn ids(revisions: Vec<Revision>, bucket: Bucket) -> Vec<(&'static str, Vec<i64>)> {
        group(
            revisions,
            &[Phid::new(VIEWER), Phid::new(GROUP)],
            &Phid::new(VIEWER),
            bucket,
            true,
        )
        .into_iter()
        .map(|group| {
            (
                group.name,
                group.revisions.iter().map(|revision| revision.id).collect(),
            )
        })
        .collect()
    }

    fn in_group(groups: &[(&'static str, Vec<i64>)], name: &str) -> Vec<i64> {
        groups
            .iter()
            .find(|(found, _)| *found == name)
            .map(|(_, ids)| ids.clone())
            .unwrap_or_default()
    }

    #[test]
    fn accepted_with_pending_group_is_held_back() {
        let groups = ids(
            vec![revision(
                1,
                vec![accept(VIEWER, DIFF_NEW), reviewer(GROUP, "blocking")],
            )],
            Bucket::NotAccepted,
        );
        assert_eq!(in_group(&groups, "Must Review"), Vec::<i64>::new());
        assert_eq!(in_group(&groups, "Ready to Review"), Vec::<i64>::new());
        assert_eq!(in_group(&groups, "Waiting on Other Reviewers"), [1]);
        // The stock bucket puts it in "Must Review".
        let stock = ids(
            vec![revision(
                1,
                vec![accept(VIEWER, DIFF_NEW), reviewer(GROUP, "blocking")],
            )],
            Bucket::RequiredAction,
        );
        assert_eq!(in_group(&stock, "Must Review"), [1]);
    }

    #[test]
    fn voided_accept_is_reviewable() {
        let mut voided = accept(VIEWER, DIFF_NEW);
        voided.voided_phid = Some(Phid::new("PHID-XACT-void"));
        let groups = ids(vec![revision(1, vec![voided])], Bucket::NotAccepted);
        assert_eq!(in_group(&groups, "Ready to Review"), [1]);
    }

    #[test]
    fn sticky_accept_of_older_diff_is_reviewable() {
        let groups = ids(
            vec![revision(
                1,
                vec![accept(VIEWER, DIFF_OLD), reviewer(GROUP, "blocking")],
            )],
            Bucket::NotAccepted,
        );
        assert_eq!(in_group(&groups, "Must Review"), [1]);
    }

    #[test]
    fn unreviewed_revision_is_reviewable() {
        let groups = ids(
            vec![revision(1, vec![reviewer(VIEWER, "added")])],
            Bucket::NotAccepted,
        );
        assert_eq!(in_group(&groups, "Ready to Review"), [1]);
    }

    #[test]
    fn held_revisions_keep_their_order() {
        let groups = ids(
            vec![
                revision(1, vec![accept(VIEWER, DIFF_NEW), reviewer(GROUP, "added")]),
                revision(2, vec![reviewer(OTHER, "added")]),
                revision(
                    3,
                    vec![accept(VIEWER, DIFF_NEW), reviewer(GROUP, "blocking")],
                ),
            ],
            Bucket::NotAccepted,
        );
        assert_eq!(in_group(&groups, "Must Review"), Vec::<i64>::new());
        assert_eq!(in_group(&groups, "Ready to Review"), Vec::<i64>::new());
        assert_eq!(in_group(&groups, "Waiting on Other Reviewers"), [1, 2, 3]);
    }

    #[test]
    fn needs_changes_variant_lists_revisions_not_acted_on() {
        let mut needs_revision = revision(1, vec![reviewer(VIEWER, "added")]);
        needs_revision.status = "needs-revision".to_owned();
        // PHP checks these against the inactive statuses in both review
        // groups, so they land in the first one, "Must Review".
        let groups = ids(vec![needs_revision.clone()], Bucket::WithNeedsChanges);
        assert_eq!(in_group(&groups, "Must Review"), [1]);
        let stock = ids(vec![needs_revision], Bucket::RequiredAction);
        assert_eq!(in_group(&stock, "Waiting on Authors"), [1]);
    }

    #[test]
    fn authors_see_their_revisions_by_state() {
        let mut mine = revision(1, vec![reviewer(OTHER, "accepted")]);
        mine.author_phid = Some(Phid::new(VIEWER));
        mine.status = "accepted".to_owned();
        let groups = ids(vec![mine], Bucket::RequiredAction);
        assert_eq!(in_group(&groups, "Ready to Land"), [1]);
        assert!(groups.iter().all(|(name, _)| *name != "Other Revisions"));
    }
}
