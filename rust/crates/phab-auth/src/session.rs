//! Session lookup, as done by
//! `PhabricatorAuthSessionEngine::loadUserForSession()`.
//!
//! Sessions are shared with PHP: a user keeps their session when a request
//! moves between the two stacks, so keys are looked up with the same digests
//! and extended with the same rules.

use std::fmt;

use phab_core::{Config, Phid, PhidType, hash, random};
use phab_db::{Database, rows};
use serde_json::{Map, json};

use crate::csrf::CsrfSecret;
use crate::user::{self, User as UserRecord};
use crate::user_log::{self, LogEntry, LogType, RequestInfo};
use crate::{AuthError, Outcome, User, Writes};

/// The cookie holding web session tokens (`PhabricatorCookies`).
pub const SESSION_COOKIE: &str = "phsid";
const SESSION_DIGEST_KEY: &str = "session.digest";

/// The `type` of a session (`PhabricatorAuthSession::TYPE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionType {
    /// A browser session, from the [`SESSION_COOKIE`] cookie.
    Web,
    /// A session opened by `conduit.connect`.
    Conduit,
}

impl SessionType {
    /// The value stored in the `type` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Conduit => "conduit",
        }
    }

    /// `PhabricatorAuthSession::getSessionTypeTTL()` of a full session.
    pub fn ttl(self) -> i64 {
        match self {
            Self::Web => 30 * 24 * 60 * 60,
            Self::Conduit => 24 * 60 * 60,
        }
    }

    /// `PhabricatorAuthSession::getSessionTypeTTL()` of a partial session,
    /// which has not passed multi-factor authentication yet.
    pub fn partial_ttl(self) -> i64 {
        match self {
            Self::Web => 30 * 60,
            Self::Conduit => self.ttl(),
        }
    }
}

/// The kind of a session token, from its prefix
/// (`PhabricatorAuthSessionEngine::KIND_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A logged-in user: no prefix, or `U/`.
    User,
    /// A logged-out user, with no identity (`A/`).
    Anonymous,
    /// A user without a full account (`X/`); PHP does not issue these.
    External,
    /// Any other prefix.
    Unknown,
}

/// `PhabricatorAuthSessionEngine::getSessionKindFromToken()`.
pub fn session_kind(token: &str) -> SessionKind {
    match token.split_once('/') {
        None | Some(("U", _)) => SessionKind::User,
        Some(("A", _)) => SessionKind::Anonymous,
        Some(("X", _)) => SessionKind::External,
        Some(_) => SessionKind::Unknown,
    }
}

/// A row of `phabricator_user.phabricator_session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The row ID.
    pub id: i64,
    /// The session PHID.
    pub phid: Phid,
    /// The session type, which the lookup matched on.
    pub session_type: SessionType,
    /// When the session started (epoch seconds).
    pub session_start: i64,
    /// When the session expires (epoch seconds).
    pub session_expires: i64,
    /// Until when the session is in high security mode (epoch seconds).
    pub high_security_until: Option<i64>,
    /// Whether multi-factor authentication is still pending.
    pub is_partial: bool,
    /// The digest of the session token the row is stored under
    /// (`PhabricatorAuthSession::newSessionDigest()`). Not a secret: PHP
    /// writes it to the activity log.
    pub key_digest: String,
}

/// The HMAC key PHP keeps in `phabricator_auth.auth_hmackey`, decoded from
/// hex. `None` if PHP has not generated it yet.
pub(crate) async fn named_hmac_key(
    database: &Database,
    name: &str,
) -> Result<Option<Vec<u8>>, AuthError> {
    let sql = format!(
        "SELECT keyValue FROM {} WHERE keyName = ?",
        database.table("auth", "auth_hmackey")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(name)
        .fetch_optional(database.pool())
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let value = rows::text(&row, "keyValue")?;
    Ok(hex::decode(value).ok().filter(|key| !key.is_empty()))
}

/// A session and the user it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUser {
    /// The user owning the session.
    pub user: User,
    /// The session itself.
    pub session: Session,
    /// What the user's CSRF tokens are made from.
    pub csrf_secret: CsrfSecret,
}

/// Loads the user for a session token, extending the session like PHP.
pub async fn load_user_for_session(
    database: &Database,
    config: &Config,
    session_type: SessionType,
    token: &str,
    now: i64,
    writes: Writes,
) -> Result<Outcome<Option<SessionUser>>, AuthError> {
    if session_kind(token) != SessionKind::User {
        return Ok(Outcome::Done(None));
    }

    let Some(digest_key) = named_hmac_key(database, SESSION_DIGEST_KEY).await? else {
        return Ok(Outcome::Unsupported(
            "the session digest key does not exist yet",
        ));
    };
    let session_key = hash::hmac_sha256(token.as_bytes(), &digest_key)?;
    let weak_key = hash::weak_digest(token.as_bytes(), config.hmac_key()?.as_bytes())?;

    let sql = format!(
        "SELECT s.id AS s_id, s.phid AS s_phid, s.sessionExpires AS s_sessionExpires, \
           s.sessionStart AS s_sessionStart, s.highSecurityUntil AS s_highSecurityUntil, \
           s.isPartial AS s_isPartial, IF(s.sessionKey = ?, 1, 0) AS s_weak, \
           u.accountSecret AS u_accountSecret, {} \
         FROM {} u JOIN {} s ON u.phid = s.userPHID \
           AND s.type = ? AND s.sessionKey IN (?, ?)",
        user::select_columns("u"),
        database.table("user", "user"),
        database.table("user", "phabricator_session"),
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(&weak_key)
        .bind(session_type.as_str())
        .bind(&session_key)
        .bind(&weak_key)
        .fetch_optional(database.pool())
        .await?;
    let Some(row) = row else {
        return Ok(Outcome::Done(None));
    };

    let user = UserRecord::from_row(&row)?;
    let session = Session {
        id: rows::int(&row, "s_id")?,
        phid: Phid::new(rows::text(&row, "s_phid")?),
        session_type,
        session_start: rows::int(&row, "s_sessionStart")?,
        session_expires: rows::int(&row, "s_sessionExpires")?,
        high_security_until: rows::opt_int(&row, "s_highSecurityUntil")?,
        is_partial: rows::flag(&row, "s_isPartial")?,
        key_digest: session_key.clone(),
    };
    let is_weak = rows::flag(&row, "s_weak")?;
    // PHP keeps the new digest on the session, even before upgrading a weak
    // key, and builds CSRF tokens from it.
    let csrf_secret = CsrfSecret::for_session(
        &user.phid,
        &rows::text(&row, "u_accountSecret")?,
        &session_key,
    );

    if session_type == SessionType::Web && !user.can_establish_web_sessions() {
        return Ok(Outcome::Done(None));
    }

    if writes == Writes::Allowed {
        extend_session(database, &session, now).await?;
        if is_weak {
            let sql = format!(
                "UPDATE {} SET sessionKey = ? WHERE id = ?",
                database.table("user", "phabricator_session")
            );
            phab_db::sqlx::query(&sql)
                .bind(&session_key)
                .bind(session.id)
                .execute(database.pool())
                .await?;
        }
    }

    Ok(Outcome::Done(Some(SessionUser {
        user,
        session,
        csrf_secret,
    })))
}

/// Whether a session should be pushed back to its full lifetime: sessions
/// are refreshed once 20% of their lifetime is used.
pub fn needs_extension(session: &Session, now: i64) -> bool {
    if session.is_partial {
        return false;
    }
    let ttl = session.session_type.ttl();
    // `$now + (0.80 * $ttl) <= $expires` means no refresh; keep the
    // comparison in integers (ttl is a multiple of 5).
    now + (ttl / 5) * 4 > session.session_expires
}

async fn extend_session(database: &Database, session: &Session, now: i64) -> Result<(), AuthError> {
    if !needs_extension(session, now) {
        return Ok(());
    }
    let sql = format!(
        "UPDATE {} SET sessionExpires = UNIX_TIMESTAMP() + ? WHERE id = ?",
        database.table("user", "phabricator_session")
    );
    phab_db::sqlx::query(&sql)
        .bind(session.session_type.ttl())
        .bind(session.id)
        .execute(database.pool())
        .await?;
    Ok(())
}

/// The `session.digest` HMAC key, loaded before a login writes anything so
/// that no later step has to hand the request to PHP.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionDigest(Vec<u8>);

impl fmt::Debug for SessionDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionDigest(<redacted>)")
    }
}

impl SessionDigest {
    /// Loads the key. Until PHP has created it, Rust can't issue sessions
    /// PHP accepts.
    pub async fn load(database: &Database) -> Result<Outcome<Self>, AuthError> {
        Ok(match named_hmac_key(database, SESSION_DIGEST_KEY).await? {
            Some(key) => Outcome::Done(Self(key)),
            None => Outcome::Unsupported("the session digest key does not exist yet"),
        })
    }

    /// A key from its raw bytes.
    pub fn new(key: Vec<u8>) -> Self {
        Self(key)
    }

    /// `PhabricatorAuthSession::newSessionDigest()`.
    pub fn digest(&self, token: &str) -> Result<String, AuthError> {
        Ok(hash::hmac_sha256(token.as_bytes(), &self.0)?)
    }
}

/// A session token just issued, for the `phsid` cookie or a Conduit
/// client. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuedSession {
    token: String,
    /// The digest the session is stored under.
    pub digest: String,
}

impl IssuedSession {
    /// The token to hand to the client.
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl fmt::Debug for IssuedSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IssuedSession(<redacted>)")
    }
}

/// Length of the random part of session tokens
/// (`Filesystem::readRandomCharacters(40)`).
const SESSION_TOKEN_LENGTH: usize = 40;

/// `establishSession()` without an identity: a logged-out session token,
/// which only protects logged-out forms from CSRF and is not stored.
pub fn anonymous_token() -> String {
    format!("A/{}", random::random_characters(SESSION_TOKEN_LENGTH))
}

/// `PhabricatorAuthSessionEngine::establishSession()` for a user: stores a
/// new session and logs the login. No session engine extension exists in
/// this install, so none runs.
pub async fn establish_session(
    database: &Database,
    keys: &SessionDigest,
    session_type: SessionType,
    user_phid: &Phid,
    partial: bool,
    now: i64,
    request: &RequestInfo,
) -> Result<IssuedSession, AuthError> {
    let token = random::random_characters(SESSION_TOKEN_LENGTH);
    let digest = keys.digest(&token)?;
    let ttl = if partial {
        session_type.partial_ttl()
    } else {
        session_type.ttl()
    };
    let sql = format!(
        "INSERT INTO {} (userPHID, type, sessionKey, sessionStart, sessionExpires, \
           highSecurityUntil, isPartial, signedLegalpadDocuments, phid) \
         VALUES (?, ?, ?, ?, ?, NULL, ?, 0, ?)",
        database.table("user", "phabricator_session")
    );
    phab_db::sqlx::query(&sql)
        .bind(user_phid.as_str())
        .bind(session_type.as_str())
        .bind(&digest)
        .bind(now)
        .bind(now + ttl)
        .bind(i32::from(partial))
        .bind(Phid::generate(PhidType::SESSION, None).as_str())
        .execute(database.pool())
        .await?;

    let details = Map::from_iter([("session_type".to_owned(), json!(session_type.as_str()))]);
    user_log::insert(
        database,
        LogEntry {
            actor: None,
            session: Some(&digest),
            user_phid: user_phid.as_str(),
            action: if partial {
                LogType::PartialLogin
            } else {
                LogType::Login
            },
            details,
        },
        request,
        now,
    )
    .await?;
    Ok(IssuedSession { token, digest })
}

/// `PhabricatorAuthSessionEngine::upgradePartialSession()`: the user
/// passed multi-factor authentication, or has no factor to pass. The
/// expiry is left alone; the next request extends it.
pub async fn upgrade_partial_session(
    database: &Database,
    found: &SessionUser,
    now: i64,
    request: &RequestInfo,
) -> Result<(), AuthError> {
    let sql = format!(
        "UPDATE {} SET isPartial = 0 WHERE id = ?",
        database.table("user", "phabricator_session")
    );
    phab_db::sqlx::query(&sql)
        .bind(found.session.id)
        .execute(database.pool())
        .await?;
    user_log::insert(
        database,
        LogEntry {
            actor: Some(&found.user.phid),
            session: Some(&found.session.key_digest),
            user_phid: found.user.phid.as_str(),
            action: LogType::FullLogin,
            details: Map::new(),
        },
        request,
        now,
    )
    .await
}

/// `PhabricatorAuthSessionEngine::logoutSession()`: logs the logout and
/// deletes the session.
pub async fn logout_session(
    database: &Database,
    found: &SessionUser,
    now: i64,
    request: &RequestInfo,
) -> Result<(), AuthError> {
    user_log::insert(
        database,
        LogEntry {
            actor: Some(&found.user.phid),
            session: Some(&found.session.key_digest),
            user_phid: found.user.phid.as_str(),
            action: LogType::Logout,
            details: Map::new(),
        },
        request,
        now,
    )
    .await?;
    let sql = format!(
        "DELETE FROM {} WHERE id = ?",
        database.table("user", "phabricator_session")
    );
    phab_db::sqlx::query(&sql)
        .bind(found.session.id)
        .execute(database.pool())
        .await?;
    Ok(())
}

/// `PhabricatorAuthSessionEngine::terminateLoginSessions()` without a
/// session to keep: deletes every session of the user, of any type.
pub async fn terminate_login_sessions(
    database: &Database,
    user_phid: &Phid,
) -> Result<(), AuthError> {
    let sql = format!(
        "DELETE FROM {} WHERE userPHID = ?",
        database.table("user", "phabricator_session")
    );
    phab_db::sqlx::query(&sql)
        .bind(user_phid.as_str())
        .execute(database.pool())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("abcdef", SessionKind::User)]
    #[case("U/abcdef", SessionKind::User)]
    #[case("A/abcdef", SessionKind::Anonymous)]
    #[case("X/abcdef", SessionKind::External)]
    #[case("Q/abcdef", SessionKind::Unknown)]
    #[case("/abcdef", SessionKind::Unknown)]
    fn kinds(#[case] token: &str, #[case] expected: SessionKind) {
        assert_eq!(session_kind(token), expected);
    }

    fn session(session_type: SessionType, expires: i64, is_partial: bool) -> Session {
        Session {
            id: 1,
            phid: Phid::new("PHID-SSSN-1"),
            session_type,
            session_start: 0,
            session_expires: expires,
            high_security_until: None,
            is_partial,
            key_digest: "digest".to_owned(),
        }
    }

    #[rstest]
    // Web sessions last 30 days: refresh once fewer than 24 days remain.
    #[case(SessionType::Web, 24 * 86_400, false, false)]
    #[case(SessionType::Web, 24 * 86_400 - 1, false, true)]
    #[case(SessionType::Web, 0, true, false)]
    #[case(SessionType::Conduit, 69_120, false, false)]
    #[case(SessionType::Conduit, 69_119, false, true)]
    fn extension(
        #[case] session_type: SessionType,
        #[case] remaining: i64,
        #[case] is_partial: bool,
        #[case] expected: bool,
    ) {
        let now = 1_000_000_000;
        let session = session(session_type, now + remaining, is_partial);
        assert_eq!(needs_extension(&session, now), expected);
    }
}
