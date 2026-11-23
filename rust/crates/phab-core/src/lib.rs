//! Building blocks shared by every Phabricator Rust crate: PHIDs, the digests
//! from `PhabricatorHash` and the layered configuration from `PhabricatorEnv`.

pub mod config;
pub mod hash;
pub mod phid;
pub mod php;
pub mod random;
pub mod text;

pub use config::Config;
pub use phid::{Phid, PhidType};
