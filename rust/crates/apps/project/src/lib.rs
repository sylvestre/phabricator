//! Projects (`PhabricatorProject`): loading them by PHID or hashtag, and
//! deciding which ones a viewer can see, which depends on their parents
//! and on membership.

pub mod policy;
pub mod storage;

pub use storage::Project;
