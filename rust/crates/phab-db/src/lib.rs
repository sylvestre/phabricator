//! Access to the `phabricator_<application>` databases.
//!
//! PHP owns the schema: `bin/storage upgrade` stays the only migration tool
//! and the tables the Rust code relies on are listed in [`schema`], which CI
//! checks against `resources/sql/quickstart.sql`.
//!
//! All databases live on one `MySQL` server, so a single pool is used and
//! tables are always addressed with their database name
//! (`phabricator_user`.`user`).

mod database;
pub mod edges;
mod error;
pub mod phpser;
pub mod rows;
pub mod schema;
#[cfg(feature = "test-support")]
pub mod testing;
pub mod worker;

pub use database::{Database, bind_all, like_escape, placeholders};
pub use error::DbError;
pub use sqlx;
