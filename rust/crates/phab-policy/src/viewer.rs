//! The user a policy decision is made for.

use phab_core::Phid;

/// The user a policy decision is made for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Viewer {
    /// `None` for logged-out viewers.
    pub phid: Option<Phid>,
    pub is_admin: bool,
    /// `PhabricatorUser::getOmnipotentUser()`, used by internal code paths.
    pub is_omnipotent: bool,
}

impl Viewer {
    pub fn anonymous() -> Self {
        Self::default()
    }

    pub fn user(phid: Phid, is_admin: bool) -> Self {
        Self {
            phid: Some(phid),
            is_admin,
            is_omnipotent: false,
        }
    }

    pub fn omnipotent() -> Self {
        Self {
            phid: None,
            is_admin: false,
            is_omnipotent: true,
        }
    }

    pub fn is_logged_in(&self) -> bool {
        self.phid.is_some()
    }

    pub(crate) fn is(&self, phid: &Phid) -> bool {
        self.phid.as_ref() == Some(phid)
    }
}
