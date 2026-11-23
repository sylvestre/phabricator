//! A user's email addresses, as the Email Addresses settings panel edits
//! them through `PhabricatorUserEditor` (`addEmail()`, `removeEmail()`,
//! `changePrimaryEmail()`) and mails them through `PhabricatorUserEmail`
//! (`sendVerificationEmail()`, `sendOldPrimaryEmail()`,
//! `sendNewPrimaryEmail()`): the rows PHP writes, in its order.
//!
//! The caller checks what makes PHP write anything else before calling
//! these: [`removal_has_side_effects()`] for the destruction engine's
//! extensions, a production URI for the links, the user's verified flag
//! for `changePrimaryEmail()`.

use std::fmt;

use phab_core::{Config, Phid, PhidType, php, random};
use phab_db::sqlx::MySqlConnection;
use phab_db::{Database, rows, worker};
use serde_json::{Map, Value, json};

use crate::rate_limit::{self, ADD_EMAIL};
use crate::user_log::{self, LogEntry, LogType, RequestInfo};
use crate::{AuthError, Outcome};

/// `PhabricatorPeopleUserEmailPHIDType`.
const EMAIL_PHID_TYPE: PhidType = PhidType::new(*b"EADR");
/// Length of a new verification code (`PhabricatorUserEmail::save()`).
const CODE_LENGTH: usize = 24;
/// `PhabricatorMailOutboundStatus::STATUS_QUEUE`.
const MAIL_STATUS_QUEUE: &str = "queued";
/// `PlatformSymbols::getPlatformServerName()`.
const SERVER_NAME: &str = "Phabricator";
/// The temporary tokens `revokePasswordResetLinks()` revokes:
/// `PhabricatorAuthOneTimeLoginTemporaryTokenType` and
/// `PhabricatorAuthPasswordResetTemporaryTokenType`.
const RESET_TOKEN_TYPES: [&str; 2] = ["login:onetime", "login:password"];

/// A verification code, which only the mail to the address carries.
/// Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct VerificationCode(String);

impl fmt::Debug for VerificationCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VerificationCode(<redacted>)")
    }
}

/// A `user_email` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEmail {
    /// `id`.
    pub id: i64,
    /// `phid`.
    pub phid: Phid,
    /// `address`.
    pub address: String,
    /// `isVerified`.
    pub is_verified: bool,
    /// `isPrimary`.
    pub is_primary: bool,
    /// `verificationCode`, if set.
    code: Option<VerificationCode>,
}

/// Which of a user's addresses a panel action may load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// `isPrimary = 0`: addresses one may delete.
    NotPrimary,
    /// `isVerified = 0`: addresses one may send another verification to.
    Unverified,
    /// `isVerified = 1 AND isPrimary = 0`: addresses one may make primary.
    VerifiedNotPrimary,
}

/// Who edits the addresses: the logged-in user and their session, which
/// the activity log records.
#[derive(Debug, Clone, Copy)]
pub struct Actor<'a> {
    /// The actor's PHID.
    pub phid: &'a Phid,
    /// The digest of the actor's session key.
    pub session_digest: &'a str,
}

/// The user whose addresses change, as the mail greets them.
#[derive(Debug, Clone, Copy)]
pub struct Owner<'a> {
    /// The user's PHID.
    pub phid: &'a Phid,
    /// `getUsername()`.
    pub username: &'a str,
}

const COLUMNS: &str = "id, phid, address, isVerified, isPrimary, verificationCode";

fn user_email(row: &phab_db::sqlx::mysql::MySqlRow) -> Result<UserEmail, AuthError> {
    Ok(UserEmail {
        id: rows::int(row, "id")?,
        phid: Phid::new(rows::text(row, "phid")?),
        address: rows::text(row, "address")?,
        is_verified: rows::flag(row, "isVerified")?,
        is_primary: rows::flag(row, "isPrimary")?,
        code: rows::opt_text(row, "verificationCode")?
            .filter(|code| php::is_truthy_str(code))
            .map(VerificationCode),
    })
}

/// `loadOneWhere('id = %d AND userPHID = %s AND ...')` of the panel's
/// actions.
pub async fn load(
    database: &Database,
    id: i64,
    user: &Phid,
    which: Which,
) -> Result<Option<UserEmail>, AuthError> {
    let condition = match which {
        Which::NotPrimary => "isPrimary = 0",
        Which::Unverified => "isVerified = 0",
        Which::VerifiedNotPrimary => "isVerified = 1 AND isPrimary = 0",
    };
    let sql = format!(
        "SELECT {COLUMNS} FROM {} WHERE id = ? AND userPHID = ? AND {condition}",
        database.table("user", "user_email")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(id)
        .bind(user.as_str())
        .fetch_optional(database.pool())
        .await?;
    found.as_ref().map(user_email).transpose()
}

/// Whether an address belongs to any account (the unique key PHP's
/// insert would hit).
pub async fn address_exists(database: &Database, address: &str) -> Result<bool, AuthError> {
    crate::register::email_exists(database, address).await
}

/// `getProductionURI($email->getVerificationURI())`, `None` where PHP
/// builds it in a way Rust doesn't (`phabricator.production-uri`).
pub fn verification_link(config: &Config, code: &str) -> Option<String> {
    crate::register::production_uri(config, &format!("/emailverify/{code}/"))
}

/// Whether `PhabricatorDestructionEngine::destroyObject()` of the address
/// would write more than its destruction log row and the deletion: rows
/// of the extensions which look for the address's PHID (edges, worker
/// tasks, flags it owns, mail properties, notifications, index versions,
/// Herald transcripts, passwords).
pub async fn removal_has_side_effects(
    database: &Database,
    email: &UserEmail,
) -> Result<bool, AuthError> {
    let checks = [
        (database.table("user", "edge"), "src"),
        (database.table("worker", "worker_activetask"), "objectPHID"),
        (database.table("flag", "flag"), "ownerPHID"),
        (
            database.table("metamta", "metamta_mailproperties"),
            "objectPHID",
        ),
        (
            database.table("feed", "feed_storynotification"),
            "primaryObjectPHID",
        ),
        (
            database.table("search", "search_indexversion"),
            "objectPHID",
        ),
        (database.table("herald", "herald_transcript"), "objectPHID"),
        (database.table("auth", "auth_password"), "objectPHID"),
    ];
    for (table, column) in checks {
        let sql = format!("SELECT 1 FROM {table} WHERE {column} = ? LIMIT 1");
        let found = phab_db::sqlx::query(&sql)
            .bind(email.phid.as_str())
            .fetch_optional(database.pool())
            .await?;
        if found.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `$user->reload()` after `beginWriteLocking()`: locks the user's row
/// for the transaction.
async fn lock_user(
    connection: &mut MySqlConnection,
    database: &Database,
    user: &Phid,
) -> Result<(), AuthError> {
    let sql = format!(
        "SELECT id FROM {} WHERE phid = ? FOR UPDATE",
        database.table("user", "user")
    );
    phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .fetch_optional(connection)
        .await?;
    Ok(())
}

/// `$email->reload()` inside the transaction.
async fn reload(
    connection: &mut MySqlConnection,
    database: &Database,
    email: &UserEmail,
) -> Result<Option<(UserEmail, Phid)>, AuthError> {
    let sql = format!(
        "SELECT {COLUMNS}, userPHID FROM {} WHERE id = ?",
        database.table("user", "user_email")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(email.id)
        .fetch_optional(connection)
        .await?;
    found
        .map(|row| Ok((user_email(&row)?, Phid::new(rows::text(&row, "userPHID")?))))
        .transpose()
}

fn log_entry<'a>(actor: Actor<'a>, user: &'a Phid, action: LogType) -> LogEntry<'a> {
    LogEntry {
        actor: Some(actor.phid),
        session: Some(actor.session_digest),
        user_phid: user.as_str(),
        action,
        details: Map::new(),
    }
}

/// `DiffusionRepositoryIdentityEngine::didUpdateEmailAddress()` and the
/// identity destruction extension: the identity change task.
async fn queue_identity_change(
    database: &Database,
    data: &Value,
    now: i64,
) -> Result<(), AuthError> {
    let mut connection = database.pool().acquire().await?;
    worker::schedule_task(
        &mut connection,
        database,
        worker::Task {
            class: "PhabricatorRepositoryIdentityChangeWorker",
            data: &php::json_encode(data),
            priority: worker::PRIORITY_DEFAULT,
            object_phid: None,
        },
        now,
    )
    .await?;
    Ok(())
}

/// The outcome of [`add()`].
#[derive(Debug)]
pub enum Added {
    /// The address was added; it needs a verification mail.
    Added(UserEmail),
    /// Another account took the address meanwhile: nothing was written.
    Duplicate,
}

/// `PhabricatorSystemActionEngine::willTakeAction()` of
/// `PhabricatorSettingsAddEmailAction` without writing: whether PHP would
/// answer with its rate limit page.
pub async fn is_add_rate_limited(
    database: &Database,
    actor: &Phid,
    now: i64,
) -> Result<bool, AuthError> {
    rate_limit::is_blocked(database, actor.as_str(), ADD_EMAIL, now).await
}

/// The settings panel adding `address` (valid and allowed) to `user`:
/// the action's rate limit score, then `PhabricatorUserEditor::addEmail()`
/// (an unverified, non-primary address, the `email-add` log row and the
/// identity change task).
pub async fn add(
    database: &Database,
    actor: Actor<'_>,
    user: &Phid,
    address: &str,
    info: &RequestInfo,
    now: i64,
) -> Result<Added, AuthError> {
    let code = random::random_characters(CODE_LENGTH);
    let phid = Phid::generate(EMAIL_PHID_TYPE, None);
    let mut transaction = database.pool().begin().await?;
    // `willTakeAction(..., 1)`. In the transaction, so that a duplicate
    // address, which Rust leaves to PHP, writes nothing.
    rate_limit::record_on(
        &mut transaction,
        database,
        actor.phid.as_str(),
        ADD_EMAIL,
        1.0,
        now,
    )
    .await?;
    lock_user(&mut transaction, database, user).await?;
    let sql = format!(
        "INSERT INTO {} (phid, userPHID, address, isVerified, isPrimary, verificationCode, \
           dateCreated, dateModified) VALUES (?, ?, ?, 0, 0, ?, ?, ?)",
        database.table("user", "user_email")
    );
    let inserted = phab_db::sqlx::query(&sql)
        .bind(phid.as_str())
        .bind(user.as_str())
        .bind(address)
        .bind(&code)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await;
    let id = match inserted {
        Ok(result) => i64::try_from(result.last_insert_id()).unwrap_or(i64::MAX),
        Err(phab_db::sqlx::Error::Database(error)) if error.is_unique_violation() => {
            transaction.rollback().await?;
            return Ok(Added::Duplicate);
        }
        Err(error) => return Err(error.into()),
    };
    user_log::insert_on(
        &mut transaction,
        database,
        log_entry(actor, user, LogType::AddEmail),
        (&Value::Null, &json!(address)),
        info,
        now,
    )
    .await?;
    transaction.commit().await?;
    queue_identity_change(database, &json!({"emailAddresses": [address]}), now).await?;
    Ok(Added::Added(UserEmail {
        id,
        phid,
        address: address.to_owned(),
        is_verified: false,
        is_primary: false,
        code: Some(VerificationCode(code)),
    }))
}

/// `PhabricatorUserEditor::removeEmail()`: the destruction log row, the
/// deletion and the `email-remove` log row, then the revoked password
/// reset links and the identity change task. `Unsupported` where PHP
/// throws (the address became primary or another user's meanwhile).
pub async fn remove(
    database: &Database,
    actor: Actor<'_>,
    user: &Phid,
    email: &UserEmail,
    info: &RequestInfo,
    now: i64,
) -> Result<Outcome<()>, AuthError> {
    let mut transaction = database.pool().begin().await?;
    lock_user(&mut transaction, database, user).await?;
    let Some((email, owner)) = reload(&mut transaction, database, email).await? else {
        return Ok(Outcome::Unsupported("an address deleted meanwhile"));
    };
    if email.is_primary || owner != *user {
        return Ok(Outcome::Unsupported("an address PHP refuses to remove"));
    }
    // `PhabricatorDestructionEngine::destroyObject()`.
    let sql = format!(
        "INSERT INTO {} (objectClass, rootLogID, objectPHID, objectMonogram, epoch) \
         VALUES ('PhabricatorUserEmail', NULL, ?, NULL, ?)",
        database.table("system", "system_destructionlog")
    );
    phab_db::sqlx::query(&sql)
        .bind(email.phid.as_str())
        .bind(now)
        .execute(&mut *transaction)
        .await?;
    let sql = format!(
        "DELETE FROM {} WHERE id = ?",
        database.table("user", "user_email")
    );
    phab_db::sqlx::query(&sql)
        .bind(email.id)
        .execute(&mut *transaction)
        .await?;
    user_log::insert_on(
        &mut transaction,
        database,
        log_entry(actor, user, LogType::RemoveEmail),
        (&json!(email.address), &Value::Null),
        info,
        now,
    )
    .await?;
    transaction.commit().await?;
    revoke_password_reset_links(database, user, now).await?;
    // `finalizeDestruction()`.
    queue_identity_change(
        database,
        &json!({"relatedPHIDs": [], "emailAddresses": [email.address]}),
        now,
    )
    .await?;
    Ok(Outcome::Done(()))
}

/// `PhabricatorUserEmail::save()` of a loaded address with a new
/// `isPrimary`: a code is made up for an address which has none.
async fn save_primary_flag(
    connection: &mut MySqlConnection,
    database: &Database,
    email: &UserEmail,
    is_primary: bool,
    now: i64,
) -> Result<(), AuthError> {
    let code = match &email.code {
        Some(code) => code.0.clone(),
        None => random::random_characters(CODE_LENGTH),
    };
    let sql = format!(
        "UPDATE {} SET isPrimary = ?, verificationCode = ?, dateModified = ? WHERE id = ?",
        database.table("user", "user_email")
    );
    phab_db::sqlx::query(&sql)
        .bind(i32::from(is_primary))
        .bind(code)
        .bind(now)
        .bind(email.id)
        .execute(connection)
        .await?;
    Ok(())
}

/// `PhabricatorUserEditor::changePrimaryEmail()` for a user whose account
/// is email verified (PHP saves the user otherwise): the old primary
/// address loses the flag, `email` gets it, the `email-primary` log row;
/// then the mail to both addresses and the revoked password reset links.
/// `Unsupported` where PHP throws.
pub async fn make_primary(
    database: &Database,
    actor: Actor<'_>,
    owner: Owner<'_>,
    email: &UserEmail,
    info: &RequestInfo,
    now: i64,
) -> Result<Outcome<()>, AuthError> {
    let user = owner.phid;
    let mut transaction = database.pool().begin().await?;
    lock_user(&mut transaction, database, user).await?;
    let Some((email, email_owner)) = reload(&mut transaction, database, email).await? else {
        return Ok(Outcome::Unsupported("an address deleted meanwhile"));
    };
    if email_owner != *user || email.is_primary || !email.is_verified {
        return Ok(Outcome::Unsupported(
            "an address PHP refuses to make primary",
        ));
    }
    // `loadPrimaryEmail()`.
    let sql = format!(
        "SELECT {COLUMNS} FROM {} WHERE userPHID = ? AND isPrimary = 1 LIMIT 1",
        database.table("user", "user_email")
    );
    let old_primary = phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .fetch_optional(&mut *transaction)
        .await?
        .as_ref()
        .map(user_email)
        .transpose()?;
    if let Some(old) = &old_primary {
        save_primary_flag(&mut transaction, database, old, false, now).await?;
    }
    save_primary_flag(&mut transaction, database, &email, true, now).await?;
    let old_value = old_primary
        .as_ref()
        .map_or(Value::Null, |old| json!(old.address));
    user_log::insert_on(
        &mut transaction,
        database,
        log_entry(actor, user, LogType::PrimaryEmail),
        (&old_value, &json!(email.address)),
        info,
        now,
    )
    .await?;
    transaction.commit().await?;

    let subject = format!("[{SERVER_NAME}] Primary Address Changed");
    if let Some(old) = &old_primary {
        let body = format!(
            "Hi {}\n\nThis email address ({}) is no longer your primary email address. Going \
             forward, all email will be sent to your new primary email address ({}).\n",
            owner.username, old.address, email.address,
        );
        queue_mail(
            database,
            &RawMail {
                to: &old.address,
                subject: &subject,
                body: &body,
                from: Some(user),
                related: user,
            },
            now,
        )
        .await?;
    }
    let body = format!(
        "Hi {}\n\nThis is now your primary email address ({}). Going forward, all email will \
         be sent here.\n",
        owner.username, email.address,
    );
    queue_mail(
        database,
        &RawMail {
            to: &email.address,
            subject: &subject,
            body: &body,
            from: Some(user),
            related: user,
        },
        now,
    )
    .await?;
    revoke_password_reset_links(database, user, now).await?;
    Ok(Outcome::Done(()))
}

/// `PhabricatorUserEmail::sendVerificationEmail()`. `Unsupported` without
/// a code, or with a link Rust doesn't make as PHP does
/// ([`verification_link()`]); nothing is written then.
pub async fn send_verification(
    database: &Database,
    config: &Config,
    owner: Owner<'_>,
    email: &UserEmail,
    now: i64,
) -> Result<Outcome<()>, AuthError> {
    let Some(link) = email
        .code
        .as_ref()
        .and_then(|code| verification_link(config, &code.0))
    else {
        return Ok(Outcome::Unsupported("a verification link PHP makes"));
    };
    let signature = if config.truthy("phabricator.serious-business") {
        String::new()
    } else {
        format!("Get Well Soon,\n{SERVER_NAME}")
    };
    let body = format!(
        "Hi {}\n\nPlease verify that you own this email address ({}) by clicking this \
         link:\n\n  {link}\n\n{signature}",
        owner.username, email.address,
    );
    queue_mail(
        database,
        &RawMail {
            to: &email.address,
            subject: &format!("[{SERVER_NAME}] Email Verification"),
            body: &body,
            from: None,
            related: owner.phid,
        },
        now,
    )
    .await?;
    Ok(Outcome::Done(()))
}

/// `revokePasswordResetLinks()`: `revokeToken()` of the user's live
/// one-time login and password reset tokens.
async fn revoke_password_reset_links(
    database: &Database,
    user: &Phid,
    now: i64,
) -> Result<(), AuthError> {
    let sql = format!(
        "UPDATE {} SET tokenExpires = ? WHERE tokenResource = ? AND tokenType IN (?, ?) \
         AND tokenExpires > ?",
        database.table("auth", "auth_temporarytoken")
    );
    phab_db::sqlx::query(&sql)
        .bind(now - 1)
        .bind(user.as_str())
        .bind(RESET_TOKEN_TYPES[0])
        .bind(RESET_TOKEN_TYPES[1])
        .bind(now)
        .execute(database.pool())
        .await?;
    Ok(())
}

/// A mail to one raw address (`addRawTos()`), always delivered
/// (`setForceDelivery(true)`).
struct RawMail<'a> {
    to: &'a str,
    subject: &'a str,
    body: &'a str,
    /// `setFrom()`, which also sets the actor.
    from: Option<&'a Phid>,
    /// `setRelatedPHID()`.
    related: &'a Phid,
}

/// `PhabricatorMetaMTAMail::saveAndSend()`: the row (no recipient edges:
/// raw addresses aren't PHIDs) and its `PhabricatorMetaMTAWorker` task.
async fn queue_mail(database: &Database, mail: &RawMail<'_>, now: i64) -> Result<(), AuthError> {
    // The constructor's parameters, then the setters in PHP's order.
    let mut parameters = Map::new();
    parameters.insert("sensitive".to_owned(), Value::Bool(true));
    parameters.insert("mustEncrypt".to_owned(), Value::Bool(false));
    parameters.insert("raw-to".to_owned(), json!([mail.to]));
    parameters.insert("force".to_owned(), Value::Bool(true));
    parameters.insert("subject".to_owned(), json!(mail.subject));
    parameters.insert("body".to_owned(), json!(mail.body));
    if let Some(from) = mail.from {
        parameters.insert("from".to_owned(), json!(from.as_str()));
    }
    let phid = Phid::generate(PhidType::MAIL, None);
    let mut transaction = database.pool().begin().await?;
    let sql = format!(
        "INSERT INTO {} (phid, actorPHID, parameters, status, message, relatedPHID, \
           dateCreated, dateModified) VALUES (?, ?, ?, ?, NULL, ?, ?, ?)",
        database.table("metamta", "metamta_mail"),
    );
    let mail_id = phab_db::sqlx::query(&sql)
        .bind(phid.as_str())
        .bind(mail.from.map(Phid::as_str))
        .bind(php::json_encode(&php::round_trip(&Value::Object(
            parameters,
        ))))
        .bind(MAIL_STATUS_QUEUE)
        .bind(mail.related.as_str())
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?
        .last_insert_id();
    transaction.commit().await?;
    let mut connection = database.pool().acquire().await?;
    worker::schedule_task(
        &mut connection,
        database,
        worker::Task {
            class: "PhabricatorMetaMTAWorker",
            data: &mail_id.to_string(),
            priority: worker::PRIORITY_ALERTS,
            object_phid: None,
        },
        now,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_codes_are_never_printed() {
        let code = VerificationCode("abc".to_owned());
        assert_eq!(format!("{code:?}"), "VerificationCode(<redacted>)");
    }

    #[test]
    fn verification_links_use_the_base_uri() {
        let mut config = Config::default();
        let mut values = Map::new();
        values.insert(
            "phabricator.base-uri".to_owned(),
            json!("https://phab.example/"),
        );
        config.push_layer("test", values.clone());
        assert_eq!(
            verification_link(&config, "abc").as_deref(),
            Some("https://phab.example/emailverify/abc/")
        );
        values.insert(
            "phabricator.production-uri".to_owned(),
            json!("https://prod.example/"),
        );
        config.push_layer("production", values);
        assert_eq!(verification_link(&config, "abc"), None);
    }
}
