//! Port of the Phabricator policy system (`PhabricatorPolicyFilter` and the
//! policy rules it evaluates).
//!
//! **This crate is a security boundary.** A divergence from PHP either hides
//! objects users should see or, worse, leaks private revisions. Changes need
//! a dedicated review.
//!
//! The decision logic in [`filter`] is pure: it works on [`PolicySubject`]
//! descriptions of objects and on [`PolicyFacts`], the data PHP would load
//! while filtering (project memberships, custom policies, spaces...).
//! [`loader`] fills `PolicyFacts` from the database. Missing facts always
//! count against the viewer, so an incomplete load fails closed.

pub mod application;
mod capability;
mod facts;
pub mod filter;
pub mod loader;
mod policies;
pub mod rules;
mod subject;
mod viewer;

pub use application::ApplicationPolicies;
pub use capability::Capability;
pub use facts::{CustomPolicy, PolicyFacts, Space};
pub use filter::{PolicyError, filter, has_capability};
pub use policies::{OBJECT_POLICY_PREFIX, POLICY_ADMIN, POLICY_NOONE, POLICY_PUBLIC, POLICY_USER};
pub use subject::{AutomaticCapability, ExtendedPolicy, ObjectKind, PolicySubject};
pub use viewer::Viewer;
