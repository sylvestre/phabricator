//! Creating an account from an external one, as
//! `PhabricatorAuthRegisterController` does through
//! `PhabricatorUserEditor::createNewUser()`: the rows, in PHP's order.
//!
//! What is written:
//!
//! - the `user` row (`PhabricatorUser::save()`, with a new conduit
//!   certificate and account secret), its name tokens and the
//!   `PhabricatorSearchWorker` task (`queueDocumentForIndexing()`);
//! - the primary `user_email` row, with a new verification code;
//! - the `PhabricatorRepositoryIdentityChangeWorker` task
//!   (`DiffusionRepositoryIdentityEngine::didUpdateEmailAddress()`);
//! - the external account, now linked to the user;
//! - for an account waiting for approval, the mail telling administrators
//!   (`sendWaitingForApprovalEmail()`): the `metamta_mail` row, its
//!   recipient edges and its `PhabricatorMetaMTAWorker` task.
//!
//! The caller checks what makes PHP write anything else (an unverified
//! address mails a verification link, event listeners, invites) and hands
//! those registrations to PHP before calling [`create()`].

use phab_core::{Config, Phid, PhidType, php, random};
use phab_db::{Database, editor as db_editor, worker};
use serde_json::{Map, Value, json};

use crate::external_account::{self, ExternalAccount};
use crate::{AuthError, User, user};

/// `PhabricatorUser::MAXIMUM_USERNAME_LENGTH`.
const MAXIMUM_USERNAME_LENGTH: usize = 64;
/// `PhabricatorUserEmail::MAX_ADDRESS_LENGTH`.
const MAX_ADDRESS_LENGTH: usize = 128;
/// `PhabricatorPeopleUserEmailPHIDType`.
const EMAIL_PHID_TYPE: PhidType = PhidType::new(*b"EADR");
/// `PhabricatorWorker::PRIORITY_INDEX`.
const PRIORITY_INDEX: i64 = 3500;
/// `PhabricatorMetaMTAMailHasRecipientEdgeType::EDGECONST`.
const MAIL_HAS_RECIPIENT: i64 = 57;
/// `PhabricatorMailOutboundStatus::STATUS_QUEUE`.
const MAIL_STATUS_QUEUE: &str = "queued";
/// `PlatformSymbols::getPlatformServerName()`.
const SERVER_NAME: &str = "Phabricator";

/// `PhabricatorUser::validateUsername()`.
pub fn is_valid_username(username: &str) -> bool {
    if username.len() > MAXIMUM_USERNAME_LENGTH {
        return false;
    }
    let allowed = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-');
    // `/^[a-zA-Z0-9._-]*[a-zA-Z0-9_-]\z/`.
    match username.as_bytes().split_last() {
        Some((last, rest)) => {
            allowed(*last) && rest.iter().all(|byte| allowed(*byte) || *byte == b'.')
        }
        None => false,
    }
}

/// `PhabricatorUser::describeValidUsername()`.
pub const VALID_USERNAME: &str = "Usernames must contain only numbers, letters, period, \
    underscore, and hyphen, and can not end with a period. They must have no more than 64 \
    characters.";

/// `PhabricatorUserEmail::isValidAddress()`:
/// `(^[a-z0-9_+.!-]+@[a-z0-9_+:.-]+\z)i`, at most 128 bytes.
pub fn is_valid_address(address: &str) -> bool {
    if address.len() > MAX_ADDRESS_LENGTH {
        return false;
    }
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    let local_ok = |byte: u8| byte.is_ascii_alphanumeric() || b"_+.!-".contains(&byte);
    let domain_ok = |byte: u8| byte.is_ascii_alphanumeric() || b"_+:.-".contains(&byte);
    !local.is_empty()
        && !domain.is_empty()
        && local.bytes().all(local_ok)
        && domain.bytes().all(domain_ok)
}

/// `PhabricatorUserEmail::describeValidAddresses()`.
pub const VALID_ADDRESSES: &str = "Email addresses should be in the form \"user@domain.com\". \
    The maximum length of an email address is 128 characters.";

/// The account to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUser {
    /// The username, valid ([`is_valid_username()`]) and printable ASCII.
    pub username: String,
    /// The real name, printable ASCII.
    pub real_name: String,
    /// The primary address, valid ([`is_valid_address()`]).
    pub email: String,
    /// Whether the address is verified, which also marks the user.
    pub email_verified: bool,
    /// Whether the account is approved; otherwise administrators get mail.
    pub approved: bool,
}

/// The mail administrators get about an account to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalMail {
    /// Every administrator, newest first (`withIsAdmin(true)`).
    pub admins: Vec<Phid>,
    /// `getProductionURI('/people/query/approval/')`.
    pub queue_uri: String,
    /// `getProductionURI('/config/edit/auth.require-approval/')`.
    pub config_uri: String,
}

/// `PhabricatorPeopleQuery::withIsAdmin(true)`, in the query's order.
pub async fn load_admins(database: &Database) -> Result<Vec<Phid>, AuthError> {
    let sql = format!(
        "SELECT phid FROM {} WHERE isAdmin = 1 ORDER BY id DESC",
        database.table("user", "user"),
    );
    let found = phab_db::sqlx::query(&sql)
        .fetch_all(database.pool())
        .await?;
    found
        .iter()
        .map(|row| Ok(Phid::new(phab_db::rows::text(row, "phid")?)))
        .collect()
}

/// `getProductionURI()`, `None` with a `phabricator.production-uri` (not
/// ported).
pub fn production_uri(config: &Config, path: &str) -> Option<String> {
    if config
        .nonempty_string("phabricator.production-uri")
        .is_some()
    {
        return None;
    }
    config.uri(path).ok()
}

/// The mail's body, as `PhabricatorMetaMTAMailBody::render()` makes it.
fn approval_body(username: &str, mail: &ApprovalMail) -> String {
    format!(
        "Newly registered user \"{username}\" is awaiting account approval by an \
         administrator.\n\nAPPROVAL QUEUE\n  {}\n\nDISABLE APPROVAL QUEUE\n  {}\n",
        mail.queue_uri, mail.config_uri,
    )
}

/// Whether a username or address is taken (`loadOneWhere()`, compared as
/// the columns' collation compares).
pub async fn username_exists(database: &Database, username: &str) -> Result<bool, AuthError> {
    Ok(!user::load_users_by_username(database, &[username])
        .await?
        .is_empty())
}

/// Whether an address belongs to an account already.
pub async fn email_exists(database: &Database, address: &str) -> Result<bool, AuthError> {
    let sql = format!(
        "SELECT 1 FROM {} WHERE address = ? LIMIT 1",
        database.table("user", "user_email"),
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(address)
        .fetch_optional(database.pool())
        .await?
        .is_some())
}

/// Writes the account, its address and the link to `account`, then the
/// approval mail if one is due. Returns the new user.
// The writes in PHP's order, in one transaction.
#[allow(clippy::too_many_lines)]
pub async fn create(
    database: &Database,
    new_user: &NewUser,
    account: &mut ExternalAccount,
    approval_mail: Option<&ApprovalMail>,
    now: i64,
) -> Result<User, AuthError> {
    let user_phid = Phid::generate(PhidType::USER, None);
    let mut transaction = database.pool().begin().await?;
    let connection = &mut *transaction;

    // `PhabricatorUser::save()`.
    let sql = format!(
        "INSERT INTO {} (phid, userName, realName, profileImagePHID, conduitCertificate, \
           isSystemAgent, isMailingList, isAdmin, isDisabled, isEmailVerified, isApproved, \
           isEnrolledInMultiFactor, accountSecret, availabilityCache, availabilityCacheTTL, \
           defaultProfileImagePHID, defaultProfileImageVersion, dateCreated, dateModified) \
         VALUES (?, ?, ?, NULL, ?, 0, 0, 0, 0, ?, ?, 0, ?, NULL, NULL, NULL, NULL, ?, ?)",
        database.table("user", "user"),
    );
    let user_id = phab_db::sqlx::query(&sql)
        .bind(user_phid.as_str())
        .bind(&new_user.username)
        .bind(&new_user.real_name)
        .bind(random::random_characters(255))
        .bind(i32::from(new_user.email_verified))
        .bind(i32::from(new_user.approved))
        .bind(random::random_characters(64))
        .bind(now)
        .bind(now)
        .execute(&mut *connection)
        .await?
        .last_insert_id();
    // `updateNameTokens()`.
    let name_tokens = database.table("user", "user_nametoken");
    phab_db::sqlx::query(&format!("DELETE FROM {name_tokens} WHERE userID = ?"))
        .bind(user_id)
        .execute(&mut *connection)
        .await?;
    let tokens = user::name_tokens(&format!("{} {}", new_user.username, new_user.real_name));
    if !tokens.is_empty() {
        let sql = format!(
            "INSERT INTO {name_tokens} (userID, token) VALUES {}",
            vec!["(?, ?)"; tokens.len()].join(", ")
        );
        let mut query = phab_db::sqlx::query(&sql);
        for token in &tokens {
            query = query.bind(user_id).bind(token.as_str());
        }
        query.execute(&mut *connection).await?;
    }
    let index_data = php::json_encode(&json!({
        "documentPHID": user_phid.as_str(),
        "parameters": [],
        "strict": false,
    }));
    worker::schedule_task(
        connection,
        database,
        worker::Task {
            class: "PhabricatorSearchWorker",
            data: &index_data,
            priority: PRIORITY_INDEX,
            object_phid: Some(user_phid.as_str()),
        },
        now,
    )
    .await?;

    // `PhabricatorUserEmail::save()`: always primary.
    let sql = format!(
        "INSERT INTO {} (phid, userPHID, address, isVerified, isPrimary, verificationCode, \
           dateCreated, dateModified) VALUES (?, ?, ?, ?, 1, ?, ?, ?)",
        database.table("user", "user_email"),
    );
    phab_db::sqlx::query(&sql)
        .bind(Phid::generate(EMAIL_PHID_TYPE, None).as_str())
        .bind(user_phid.as_str())
        .bind(&new_user.email)
        .bind(i32::from(new_user.email_verified))
        .bind(random::random_characters(24))
        .bind(now)
        .bind(now)
        .execute(&mut *connection)
        .await?;
    worker::schedule_task(
        connection,
        database,
        worker::Task {
            class: "PhabricatorRepositoryIdentityChangeWorker",
            data: &php::json_encode(&json!({"emailAddresses": [new_user.email]})),
            priority: worker::PRIORITY_DEFAULT,
            object_phid: None,
        },
        now,
    )
    .await?;

    account.user_phid = Some(user_phid.clone());
    external_account::save_on(connection, database, account, now).await?;

    if let Some(mail) = approval_mail.filter(|mail| !mail.admins.is_empty()) {
        write_approval_mail(connection, database, &new_user.username, mail, now).await?;
    }
    transaction.commit().await?;

    Ok(User {
        id: i64::try_from(user_id).unwrap_or(i64::MAX),
        phid: user_phid,
        user_name: new_user.username.clone(),
        real_name: new_user.real_name.clone(),
        profile_image_phid: None,
        is_system_agent: false,
        is_disabled: false,
        is_admin: false,
        is_email_verified: new_user.email_verified,
        is_approved: new_user.approved,
        is_mailing_list: false,
    })
}

/// `sendWaitingForApprovalEmail()` through `PhabricatorMetaMTAMail::save()`.
async fn write_approval_mail(
    connection: &mut phab_db::sqlx::MySqlConnection,
    database: &Database,
    username: &str,
    mail: &ApprovalMail,
    now: i64,
) -> Result<(), AuthError> {
    let mut admins: Vec<&str> = Vec::new();
    for admin in &mail.admins {
        if !admins.contains(&admin.as_str()) {
            admins.push(admin.as_str());
        }
    }
    // The constructor's parameters, then `addTos()`, `setSubject()` and
    // `setBody()`.
    let mut parameters = Map::new();
    parameters.insert("sensitive".to_owned(), Value::Bool(true));
    parameters.insert("mustEncrypt".to_owned(), Value::Bool(false));
    parameters.insert("to".to_owned(), json!(admins));
    parameters.insert(
        "subject".to_owned(),
        Value::String(format!(
            "[{SERVER_NAME}] New User \"{username}\" Awaiting Approval"
        )),
    );
    parameters.insert(
        "body".to_owned(),
        Value::String(approval_body(username, mail)),
    );
    let phid = Phid::generate(PhidType::MAIL, None);
    let sql = format!(
        "INSERT INTO {} (phid, actorPHID, parameters, status, message, relatedPHID, \
           dateCreated, dateModified) VALUES (?, NULL, ?, ?, NULL, NULL, ?, ?)",
        database.table("metamta", "metamta_mail"),
    );
    let mail_id = phab_db::sqlx::query(&sql)
        .bind(phid.as_str())
        .bind(php::json_encode(&php::round_trip(&Value::Object(
            parameters,
        ))))
        .bind(MAIL_STATUS_QUEUE)
        .bind(now)
        .bind(now)
        .execute(&mut *connection)
        .await?
        .last_insert_id();
    // Users expand to themselves.
    for (seq, admin) in (0..).zip(&admins) {
        db_editor::insert_edge(
            connection,
            database,
            (phid.as_str(), MAIL_HAS_RECIPIENT, admin),
            seq,
            now,
        )
        .await?;
    }
    worker::schedule_task(
        connection,
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
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("alice", true)]
    #[case("a.b-c_d", true)]
    #[case("a.", false)]
    #[case(".a", true)]
    #[case("", false)]
    #[case("a b", false)]
    #[case("é", false)]
    #[case(&"a".repeat(64), true)]
    #[case(&"a".repeat(65), false)]
    fn usernames_like_php(#[case] username: &str, #[case] expected: bool) {
        assert_eq!(is_valid_username(username), expected);
    }

    #[rstest]
    #[case("a@example.com", true)]
    #[case("A.B+c!d@EXAMPLE.com:25", true)]
    #[case("a@b@c", false)]
    #[case("@example.com", false)]
    #[case("a@", false)]
    #[case("a b@example.com", false)]
    #[case("\"a\"@example.com", false)]
    #[case(&format!("{}@b", "a".repeat(126)), true)]
    #[case(&format!("{}@b", "a".repeat(127)), false)]
    fn addresses_like_php(#[case] address: &str, #[case] expected: bool) {
        assert_eq!(is_valid_address(address), expected);
    }

    #[test]
    fn approval_mail_bodies_like_php() {
        let mail = ApprovalMail {
            admins: Vec::new(),
            queue_uri: "https://phab.example/people/query/approval/".to_owned(),
            config_uri: "https://phab.example/config/edit/auth.require-approval/".to_owned(),
        };
        assert_eq!(
            approval_body("bob", &mail),
            "Newly registered user \"bob\" is awaiting account approval by an administrator.\
             \n\nAPPROVAL QUEUE\n  https://phab.example/people/query/approval/\n\n\
             DISABLE APPROVAL QUEUE\n  https://phab.example/config/edit/auth.require-approval/\n"
        );
    }
}
