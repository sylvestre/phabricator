//! The audit requests of a commit (`PhabricatorRepositoryAuditRequest`)
//! and what the transactions of `DiffusionCommitAuditTransaction` and
//! `DiffusionCommitAuditorsTransaction` do to them: their checks
//! (`validateAction()`), the statuses they set (`applyAuditorEffect()`,
//! `generateNewValue()`) and the rows `updateAudits()` writes, then the
//! commit's status (`updateAuditStatus()`).

/// `PhabricatorAuditRequestStatus::AUDIT_REQUIRED`.
pub(super) const AUDIT_REQUIRED: &str = "audit-required";
/// `PhabricatorAuditRequestStatus::CONCERNED`.
pub(super) const CONCERNED: &str = "concerned";
/// `PhabricatorAuditRequestStatus::ACCEPTED`.
pub(super) const ACCEPTED: &str = "accepted";
/// `PhabricatorAuditRequestStatus::AUDIT_REQUESTED`.
pub(super) const AUDIT_REQUESTED: &str = "requested";
/// `PhabricatorAuditRequestStatus::RESIGNED`.
pub(super) const RESIGNED: &str = "resigned";

/// `DiffusionCommitAuditStatus::NONE`.
pub(super) const STATUS_NONE: &str = "none";
/// `DiffusionCommitAuditStatus::NEEDS_AUDIT`.
const STATUS_NEEDS_AUDIT: &str = "needs-audit";
/// `DiffusionCommitAuditStatus::CONCERN_RAISED`.
const STATUS_CONCERN_RAISED: &str = "concern-raised";
/// `DiffusionCommitAuditStatus::PARTIALLY_AUDITED`.
pub(super) const STATUS_PARTIALLY_AUDITED: &str = "partially-audited";
/// `DiffusionCommitAuditStatus::AUDITED`.
const STATUS_AUDITED: &str = "audited";
/// `DiffusionCommitAuditStatus::NEEDS_VERIFICATION`.
const STATUS_NEEDS_VERIFICATION: &str = "needs-verification";

/// A row of `repository_auditrequest`, in the order PHP loads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Audit {
    /// `id`.
    pub(super) id: i64,
    /// `auditorPHID`.
    pub(super) auditor: String,
    /// `auditStatus`.
    pub(super) status: String,
}

impl Audit {
    /// `isResigned()`.
    pub(super) fn is_resigned(&self) -> bool {
        self.status == RESIGNED
    }
}

/// An audit action of `DiffusionCommitEditEngine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    /// `DiffusionCommitAcceptTransaction`.
    Accept,
    /// `DiffusionCommitConcernTransaction`.
    Concern,
    /// `DiffusionCommitResignTransaction`.
    Resign,
    /// `DiffusionCommitVerifyTransaction`.
    Verify,
}

impl Action {
    /// The action of a Conduit transaction type (`ACTIONKEY`'s edit type).
    pub(super) fn of_conduit_type(kind: &str) -> Option<Self> {
        match kind {
            "accept" => Some(Self::Accept),
            "concern" => Some(Self::Concern),
            "resign" => Some(Self::Resign),
            "verify" => Some(Self::Verify),
            _ => None,
        }
    }

    /// `TRANSACTIONTYPE`.
    pub(super) fn transaction_type(self) -> &'static str {
        match self {
            Self::Accept => "diffusion.commit.accept",
            Self::Concern => "diffusion.commit.concern",
            Self::Resign => "diffusion.commit.resign",
            Self::Verify => "diffusion.commit.verify",
        }
    }

    /// The status `applyExternalEffects()` gives the actor's audits; "Request
    /// Verification" touches none.
    fn audit_status(self) -> Option<&'static str> {
        match self {
            Self::Accept => Some(ACCEPTED),
            Self::Concern => Some(CONCERNED),
            Self::Resign => Some(RESIGNED),
            Self::Verify => None,
        }
    }

    /// The commit status `applyInternalEffects()` sets, if any.
    pub(super) fn internal_status(self) -> Option<&'static str> {
        match self {
            Self::Concern => Some(STATUS_CONCERN_RAISED),
            Self::Verify => Some(STATUS_NEEDS_VERIFICATION),
            Self::Accept | Self::Resign => None,
        }
    }
}

/// What the checks of an action read.
pub(super) struct Actor<'a> {
    /// The acting user.
    pub(super) phid: &'a str,
    /// `getEffectiveAuthorPHID()` of the commit.
    pub(super) effective_author: Option<&'a str>,
    /// The auditors the actor has authority over (`hasAuditAuthority()`).
    pub(super) authority: &'a [String],
    /// `audit.can-author-close-audit`.
    pub(super) can_author_close: bool,
}

impl Actor<'_> {
    /// `isViewerCommitAuthor()`.
    fn is_author(&self) -> bool {
        self.effective_author == Some(self.phid)
    }

    /// `getViewerAuditStatus()`: the status of the actor's own audit.
    fn own_status<'a>(&self, audits: &'a [Audit]) -> Option<&'a str> {
        audits
            .iter()
            .find(|audit| audit.auditor == self.phid)
            .map(|audit| audit.status.as_str())
    }

    /// `isViewerAuditStatusFullyAmong()`.
    fn is_fully_among(&self, audits: &[Audit], statuses: &[&str]) -> bool {
        if self.own_status(audits).is_none() {
            return false;
        }
        audits
            .iter()
            .filter(|audit| self.authority.contains(&audit.auditor))
            .all(|audit| statuses.contains(&audit.status.as_str()))
    }

    /// `isViewerAnyActiveAuditor()`.
    fn is_active_auditor(&self, audits: &[Audit]) -> bool {
        self.own_status(audits).is_some_and(|status| {
            [AUDIT_REQUIRED, CONCERNED, ACCEPTED, AUDIT_REQUESTED].contains(&status)
        })
    }
}

/// `validateAction()`: whether PHP lets the actor take the action on a
/// commit with these audits and this status. PHP's errors are left to it.
pub(super) fn is_allowed(
    action: Action,
    actor: &Actor<'_>,
    audits: &[Audit],
    status: &str,
) -> bool {
    match action {
        Action::Accept => {
            (actor.can_author_close || !actor.is_author())
                && !actor.is_fully_among(audits, &[ACCEPTED])
        }
        Action::Concern => {
            // Raising a concern again is fine once verification is asked.
            !actor.is_author()
                && (!actor.is_fully_among(audits, &[CONCERNED])
                    || status == STATUS_NEEDS_VERIFICATION)
        }
        Action::Resign => actor.is_active_auditor(audits),
        Action::Verify => actor.is_author() && status == STATUS_CONCERN_RAISED,
    }
}

/// `applyAuditorEffect()`: the audits the actor has authority over take
/// the action's status (resigning only affects oneself), and so does the
/// actor's own, which may be new.
pub(super) fn action_statuses(
    action: Action,
    actor: &Actor<'_>,
    audits: &[Audit],
) -> Vec<(String, String)> {
    let Some(status) = action.audit_status() else {
        return Vec::new();
    };
    let mut map: Vec<(String, String)> = Vec::new();
    if status != RESIGNED {
        for audit in audits {
            if actor.authority.contains(&audit.auditor) {
                put(&mut map, &audit.auditor, status);
            }
        }
    }
    put(&mut map, actor.phid, status);
    map
}

/// `$map[$key] = $value`: a new key goes last.
fn put(map: &mut Vec<(String, String)>, key: &str, value: &str) {
    match map.iter_mut().find(|(known, _)| known == key) {
        Some(entry) => value.clone_into(&mut entry.1),
        None => map.push((key.to_owned(), value.to_owned())),
    }
}

/// What an auditor list transaction does (`+`, `-` or `=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Operation {
    Add,
    Remove,
    Set,
}

/// `DiffusionCommitAuditorsTransaction::generateOldValue()`: each
/// auditor's status.
pub(super) fn auditor_map(audits: &[Audit]) -> Vec<(String, String)> {
    let mut map = Vec::new();
    for audit in audits {
        put(&mut map, &audit.auditor, &audit.status);
    }
    map
}

/// `generateNewValue()`: removed auditors go, new ones are requested,
/// the others keep their status.
pub(super) fn new_auditors(
    old: &[(String, String)],
    operation: Operation,
    phids: &[String],
) -> Vec<(String, String)> {
    let mut auditors: Vec<(String, String)> = old.to_vec();
    if operation == Operation::Remove {
        auditors.retain(|(phid, _)| !phids.contains(phid));
    }
    let added: &[String] = match operation {
        Operation::Remove => &[],
        Operation::Add | Operation::Set => phids,
    };
    if operation == Operation::Set {
        auditors.clear();
    }
    for phid in added {
        let status = old
            .iter()
            .find(|(known, _)| known == phid)
            .map_or(AUDIT_REQUESTED, |(_, status)| status.as_str());
        put(&mut auditors, phid, status);
    }
    auditors
}

/// `getTransactionHasEffect()`: the maps differ once sorted by key.
pub(super) fn auditors_differ(old: &[(String, String)], new: &[(String, String)]) -> bool {
    let sorted = |map: &[(String, String)]| {
        let mut map = map.to_vec();
        map.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        map
    };
    sorted(old) != sorted(new)
}

/// A write of `updateAudits()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Write {
    /// A new audit request.
    Insert { auditor: String, status: String },
    /// A new status for an existing one.
    Update { id: i64, status: String },
}

/// `updateAudits()`: an audit for each auditor of `map` which has none, a
/// new status for those whose status changes.
pub(super) fn updates(audits: &[Audit], map: &[(String, String)]) -> Vec<Write> {
    map.iter()
        .filter_map(|(auditor, status)| {
            match audits.iter().find(|audit| &audit.auditor == auditor) {
                None => Some(Write::Insert {
                    auditor: auditor.clone(),
                    status: status.clone(),
                }),
                Some(audit) if &audit.status == status => None,
                Some(audit) => Some(Write::Update {
                    id: audit.id,
                    status: status.clone(),
                }),
            }
        })
        .collect()
}

/// `PhabricatorRepositoryCommit::updateAuditStatus()`: the status the
/// audits give the commit, whose status is `current`.
pub(super) fn commit_status(audits: &[Audit], current: &str) -> &'static str {
    let any = |statuses: &[&str]| {
        audits
            .iter()
            .any(|audit| statuses.contains(&audit.status.as_str()))
    };
    let need = any(&[AUDIT_REQUIRED, AUDIT_REQUESTED]);
    if any(&[CONCERNED]) {
        if current == STATUS_NEEDS_VERIFICATION {
            STATUS_NEEDS_VERIFICATION
        } else {
            STATUS_CONCERN_RAISED
        }
    } else if any(&[ACCEPTED]) {
        if need {
            STATUS_PARTIALLY_AUDITED
        } else {
            STATUS_AUDITED
        }
    } else if need {
        STATUS_NEEDS_AUDIT
    } else {
        STATUS_NONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audit(id: i64, auditor: &str, status: &str) -> Audit {
        Audit {
            id,
            auditor: auditor.to_owned(),
            status: status.to_owned(),
        }
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn computes_commit_statuses_like_php() {
        assert_eq!(commit_status(&[], "audited"), STATUS_NONE);
        let requested = audit(1, "a", AUDIT_REQUESTED);
        let accepted = audit(2, "b", ACCEPTED);
        let concerned = audit(3, "c", CONCERNED);
        assert_eq!(
            commit_status(std::slice::from_ref(&requested), "none"),
            "needs-audit"
        );
        assert_eq!(
            commit_status(&[requested.clone(), accepted.clone()], "none"),
            "partially-audited"
        );
        assert_eq!(
            commit_status(std::slice::from_ref(&accepted), "none"),
            "audited"
        );
        assert_eq!(
            commit_status(&[accepted.clone(), concerned.clone()], "none"),
            "concern-raised"
        );
        assert_eq!(
            commit_status(&[concerned], "needs-verification"),
            "needs-verification"
        );
        assert_eq!(commit_status(&[audit(4, "d", RESIGNED)], "audited"), "none");
    }

    #[test]
    fn edits_auditors_like_php() {
        let old = pairs(&[("a", ACCEPTED), ("b", AUDIT_REQUESTED)]);
        assert_eq!(
            new_auditors(&old, Operation::Add, &strings(&["c", "a"])),
            pairs(&[
                ("a", ACCEPTED),
                ("b", AUDIT_REQUESTED),
                ("c", AUDIT_REQUESTED)
            ])
        );
        assert_eq!(
            new_auditors(&old, Operation::Remove, &strings(&["a", "x"])),
            pairs(&[("b", AUDIT_REQUESTED)])
        );
        assert_eq!(
            new_auditors(&old, Operation::Set, &strings(&["c", "a"])),
            pairs(&[("c", AUDIT_REQUESTED), ("a", ACCEPTED)])
        );
        assert!(!auditors_differ(
            &old,
            &pairs(&[("b", AUDIT_REQUESTED), ("a", ACCEPTED)])
        ));
        assert!(auditors_differ(&old, &pairs(&[("a", ACCEPTED)])));
    }

    #[test]
    fn applies_actions_like_php() {
        let audits = [
            audit(1, "proj", AUDIT_REQUESTED),
            audit(2, "other", AUDIT_REQUESTED),
        ];
        let authority = strings(&["me", "proj"]);
        let actor = Actor {
            phid: "me",
            effective_author: Some("author"),
            authority: &authority,
            can_author_close: false,
        };
        let map = action_statuses(Action::Accept, &actor, &audits);
        assert_eq!(map, pairs(&[("proj", ACCEPTED), ("me", ACCEPTED)]));
        assert_eq!(
            updates(&audits, &map),
            vec![
                Write::Update {
                    id: 1,
                    status: ACCEPTED.to_owned()
                },
                Write::Insert {
                    auditor: "me".to_owned(),
                    status: ACCEPTED.to_owned()
                },
            ]
        );
        assert_eq!(
            action_statuses(Action::Resign, &actor, &audits),
            pairs(&[("me", RESIGNED)])
        );
        assert_eq!(action_statuses(Action::Verify, &actor, &audits), pairs(&[]));
    }

    #[test]
    fn checks_actions_like_php() {
        let authority = strings(&["me"]);
        let actor = |author| Actor {
            phid: "me",
            effective_author: author,
            authority: &authority,
            can_author_close: false,
        };
        let none: [Audit; 0] = [];
        let mine = [audit(1, "me", ACCEPTED)];
        let concerned = [audit(1, "me", CONCERNED)];
        assert!(is_allowed(Action::Accept, &actor(None), &none, "none"));
        assert!(!is_allowed(
            Action::Accept,
            &actor(Some("me")),
            &none,
            "none"
        ));
        assert!(!is_allowed(Action::Accept, &actor(None), &mine, "audited"));
        assert!(is_allowed(Action::Concern, &actor(None), &mine, "audited"));
        assert!(!is_allowed(
            Action::Concern,
            &actor(None),
            &concerned,
            "concern-raised"
        ));
        assert!(is_allowed(
            Action::Concern,
            &actor(None),
            &concerned,
            "needs-verification"
        ));
        assert!(!is_allowed(Action::Resign, &actor(None), &none, "none"));
        assert!(is_allowed(Action::Resign, &actor(None), &mine, "audited"));
        assert!(!is_allowed(
            Action::Resign,
            &actor(None),
            &[audit(1, "me", RESIGNED)],
            "none"
        ));
        assert!(is_allowed(
            Action::Verify,
            &actor(Some("me")),
            &none,
            "concern-raised"
        ));
        assert!(!is_allowed(
            Action::Verify,
            &actor(None),
            &none,
            "concern-raised"
        ));
        assert!(!is_allowed(
            Action::Verify,
            &actor(Some("me")),
            &none,
            "audited"
        ));
    }
}
