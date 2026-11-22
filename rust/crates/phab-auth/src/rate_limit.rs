//! Rate limits of `PhabricatorSystemActionEngine`, as the password login
//! uses them.

use phab_core::hash;
use phab_db::Database;

use crate::AuthError;

/// Scores are averaged over this window (`getWindow()`).
const WINDOW: f64 = 3600.0;

/// A `PhabricatorSystemAction`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SystemAction {
    /// `getActionConstant()`.
    pub constant: &'static str,
    /// `getScoreThreshold()`, per second.
    pub threshold: f64,
}

/// `PhabricatorAuthTryPasswordAction`: 100 attempts an hour.
pub const TRY_PASSWORD: SystemAction = SystemAction {
    constant: "auth.password",
    threshold: 100.0 / WINDOW,
};

/// `loadBlockedActors()` for one actor, without writing: whether `actor`
/// is over the limit of `action`, so `willTakeAction()` would throw.
pub async fn is_blocked(
    database: &Database,
    actor: &str,
    action: SystemAction,
    now: i64,
) -> Result<bool, AuthError> {
    let sql = format!(
        "SELECT SUM(score) FROM {} WHERE action = ? AND actorHash IN (?) AND epoch >= ? \
         GROUP BY actorHash",
        database.table("system", "system_actionlog")
    );
    let total: Option<f64> = phab_db::sqlx::query_scalar(&sql)
        .bind(action.constant)
        .bind(hash::digest_for_index(actor.as_bytes()))
        .bind(now - 3600)
        .fetch_optional(database.pool())
        .await?;
    // `shouldBlockActor()`: `$score > $threshold`, with the score averaged.
    Ok(total.unwrap_or(0.0) / WINDOW > action.threshold)
}

/// `recordAction()`: adds `score` for `actor`.
pub async fn record(
    database: &Database,
    actor: &str,
    action: SystemAction,
    score: f64,
    now: i64,
) -> Result<(), AuthError> {
    let sql = format!(
        "INSERT INTO {} (actorHash, actorIdentity, action, score, epoch) VALUES (?, ?, ?, ?, ?)",
        database.table("system", "system_actionlog")
    );
    phab_db::sqlx::query(&sql)
        .bind(hash::digest_for_index(actor.as_bytes()))
        .bind(actor)
        .bind(action.constant)
        .bind(score)
        .bind(now)
        .execute(database.pool())
        .await?;
    Ok(())
}
