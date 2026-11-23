//! What the Sessions settings panel does to sessions:
//! `PhabricatorAuthTerminateSessionController` deletes them (PHP writes no
//! log row for it), and `PhabricatorAuthDowngradeSessionController` takes
//! the viewer's own out of high security
//! (`PhabricatorAuthSessionEngine::exitHighSecurity()`).

use phab_db::Database;
use serde_json::Map;

use crate::AuthError;
use crate::session::SessionUser;
use crate::user_log::{self, LogEntry, LogType, RequestInfo};

/// `$session->delete()` of each session, by row ID. The caller has checked
/// that they belong to the viewer and that none is the viewer's current
/// session.
pub async fn terminate(database: &Database, ids: &[i64]) -> Result<(), AuthError> {
    let sql = format!(
        "DELETE FROM {} WHERE id = ?",
        database.table("user", "phabricator_session")
    );
    for id in ids {
        phab_db::sqlx::query(&sql)
            .bind(id)
            .execute(database.pool())
            .await?;
    }
    Ok(())
}

/// `exitHighSecurity()`: clears the session's high security and logs it.
/// Nothing happens to a session which never entered it.
pub async fn exit_high_security(
    database: &Database,
    viewer: &SessionUser,
    info: &RequestInfo,
    now: i64,
) -> Result<(), AuthError> {
    // `if (!$session->getHighSecurityUntil())`.
    if viewer.session.high_security_until.unwrap_or(0) == 0 {
        return Ok(());
    }
    let sql = format!(
        "UPDATE {} SET highSecurityUntil = NULL WHERE id = ?",
        database.table("user", "phabricator_session")
    );
    phab_db::sqlx::query(&sql)
        .bind(viewer.session.id)
        .execute(database.pool())
        .await?;
    let entry = LogEntry {
        actor: Some(&viewer.user.phid),
        session: Some(&viewer.session.key_digest),
        user_phid: viewer.user.phid.as_str(),
        action: LogType::ExitHisec,
        details: Map::new(),
    };
    user_log::insert(database, entry, info, now).await
}
