//! The user a policy decision is made for.

use phab_core::Phid;

/// The user a policy decision is made for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Viewer {
    /// `None` for logged-out viewers.
    pub phid: Option<Phid>,
    /// Whether the user is an administrator.
    pub is_admin: bool,
    /// `PhabricatorUser::getOmnipotentUser()`, used by internal code paths.
    pub is_omnipotent: bool,
}

impl Viewer {
    /// A logged-out viewer.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// A logged-in user.
    pub fn user(phid: Phid, is_admin: bool) -> Self {
        Self {
            phid: Some(phid),
            is_admin,
            is_omnipotent: false,
        }
    }

    /// The omnipotent viewer, which passes every policy check.
    pub fn omnipotent() -> Self {
        Self {
            phid: None,
            is_admin: false,
            is_omnipotent: true,
        }
    }

    /// Whether the viewer has a PHID.
    pub fn is_logged_in(&self) -> bool {
        self.phid.is_some()
    }

    pub(crate) fn is(&self, phid: &Phid) -> bool {
        self.phid.as_ref() == Some(phid)
    }
}
