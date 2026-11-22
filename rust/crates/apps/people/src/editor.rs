//! `PhabricatorUserTransactionEditor`: what `user.edit` writes, and through
//! it `user.enable` and `user.disable`.
//!
//! An edit is planned first: the checks PHP's editor makes and the old and
//! new values of the transactions. Anything not ported is
//! [`MethodError::Unsupported`] there, before anything is written. Then one
//! MySQL transaction writes the rows PHP does:
//!
//! - the user row (`PhabricatorUser::save()`), saved even when no
//!   transaction has an effect;
//! - not the profile: `save()` saves it only once something called
//!   `loadUserProfile()` on that user object, which Conduit edits don't;
//! - the name tokens (`updateNameTokens()`);
//! - a `PhabricatorSearchWorker` task (`queueDocumentForIndexing()`);
//! - the transactions;
//! - the `PhabricatorApplicationTransactionPublishWorker` task, so feed
//!   stories still come from PHP's daemons.
//!
//! Users have no Herald adapter, no mail and no edges.
//!
//! Ported transactions: `user.disable`, and `user.approve` without effect.
//! Left to PHP: approvals (they send mail), configured custom fields, edits
//! the actor may not make (PHP raises policy exceptions), users without a
//! conduit certificate or account secret (`save()` makes them up) and
//! users whose names are not printable ASCII (PHP lowercases name tokens
//! with `mbstring`).

use phab_conduit::{ApiRequest, MethodError, pager};
use phab_core::{Phid, PhidType, php, random};
use phab_db::editor::{self as db_editor, CONDUIT_SOURCE, TransactionRow};
use phab_db::{Database, rows, worker};
use phab_policy::loader::{self, NoExtendedObjects};
use phab_policy::{Capability, ObjectKind, POLICY_ADMIN, PolicySubject, Viewer};
use serde_json::json;

/// `PhabricatorPeopleApplication`.
const APPLICATION_CLASS: &str = "PhabricatorPeopleApplication";
/// `PeopleDisableUsersCapability`, admin-only by default.
const DISABLE_USERS: Capability = Capability("people.disable.users");
/// `PhabricatorUserEditEngine::ENGINECONST`.
const ENGINE_KEY: &str = "people.user";
/// `PhabricatorWorker::PRIORITY_INDEX`.
const PRIORITY_INDEX: i64 = 3500;

/// One transaction of the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    /// `disabled`: `user.disable`.
    Disabled(bool),
    /// `approved`: `user.approve`.
    Approved(bool),
}

impl Change {
    fn kind(self) -> &'static str {
        match self {
            Self::Disabled(_) => "user.disable",
            Self::Approved(_) => "user.approve",
        }
    }
}

/// The edited user, as `PhabricatorPeopleQuery` loads it.
#[derive(Debug, Clone)]
pub(crate) struct EditedUser {
    id: i64,
    phid: String,
    user_name: String,
    real_name: String,
    is_disabled: bool,
    is_approved: bool,
    is_system_agent: bool,
    is_mailing_list: bool,
    has_certificate: bool,
    has_secret: bool,
}

/// How `newObjectFromIdentifier()` finds the user.
pub(crate) enum Identifier<'a> {
    Id(i64),
    Phid(&'a str),
}

/// Loads a user by ID or PHID. Users are public: anyone can see them.
pub(crate) async fn load_user(
    database: &Database,
    identifier: Identifier<'_>,
) -> Result<Option<EditedUser>, MethodError> {
    let (column, value) = match identifier {
        Identifier::Id(id) => ("id", id.to_string()),
        Identifier::Phid(phid) => ("phid", phid.to_owned()),
    };
    let sql = format!(
        "SELECT id, phid, userName, realName, isDisabled, isApproved, isSystemAgent, \
         isMailingList, conduitCertificate, accountSecret FROM {} WHERE {column} = ?",
        database.table("user", "user")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(value)
        .fetch_optional(database.pool())
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(EditedUser {
        id: rows::int(&row, "id")?,
        phid: rows::text(&row, "phid")?,
        user_name: rows::text(&row, "userName")?,
        real_name: rows::text(&row, "realName")?,
        is_disabled: rows::flag(&row, "isDisabled")?,
        is_approved: rows::flag(&row, "isApproved")?,
        is_system_agent: rows::flag(&row, "isSystemAgent")?,
        is_mailing_list: rows::flag(&row, "isMailingList")?,
        has_certificate: !rows::text(&row, "conduitCertificate")?.is_empty(),
        has_secret: !rows::text(&row, "accountSecret")?.is_empty(),
    }))
}

/// A transaction with an effect.
struct Planned {
    phid: String,
    kind: &'static str,
    old: bool,
    new: bool,
}

/// The planned edit of one user.
pub(crate) struct Plan {
    actor: String,
    user: EditedUser,
    /// The new value of `isDisabled`.
    is_disabled: bool,
    tokens: Vec<String>,
    transactions: Vec<Planned>,
    group_id: String,
    herald_header: Option<String>,
}

impl Plan {
    /// The ID of the edited user.
    pub(crate) fn user_id(&self) -> i64 {
        self.user.id
    }

    /// The PHID of the edited user.
    pub(crate) fn user_phid(&self) -> &str {
        &self.user.phid
    }

    /// The PHIDs of the applied transactions.
    pub(crate) fn transaction_phids(&self) -> Vec<String> {
        self.transactions
            .iter()
            .map(|planned| planned.phid.clone())
            .collect()
    }
}

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// Text PHP and Rust lowercase and split alike: printable ASCII.
fn is_plain(text: &str) -> bool {
    text.bytes().all(|byte| (b' '..=b'~').contains(&byte))
}

/// `PhabricatorTypeaheadDatasource::tokenizeString()` of plain text.
fn tokenize(text: &str) -> Vec<String> {
    let lowered = text.to_ascii_lowercase();
    let mut tokens: Vec<String> = Vec::new();
    for token in lowered
        .trim_matches([' ', '\t', '\n', '\r', '\0', '\x0b'])
        .split(|character: char| character.is_ascii_whitespace() || "[]()-\x0b".contains(character))
    {
        if !token.is_empty() && !tokens.iter().any(|known| known == token) {
            tokens.push(token.to_owned());
        }
    }
    tokens
}

/// What changes what the edit engine and the editor do in ways not
/// ported: configured custom fields, forms configured in the web UI.
pub(crate) async fn check_environment(request: &ApiRequest<'_>) -> Result<(), MethodError> {
    let context = request.context;
    if context
        .config
        .value("user.custom-field-definitions")
        .is_some_and(php::is_truthy)
    {
        return Err(unsupported("user custom fields"));
    }
    let database = &context.database;
    let sql = format!(
        "SELECT id FROM {} WHERE engineKey = ? LIMIT 1",
        database.table("search", "search_editengineconfiguration")
    );
    if phab_db::sqlx::query(&sql)
        .bind(ENGINE_KEY)
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Err(unsupported("configured user forms"));
    }
    Ok(())
}

/// `requireApplicationCapability(PeopleDisableUsersCapability)`.
async fn can_disable_users(request: &ApiRequest<'_>) -> Result<bool, MethodError> {
    let context = request.context;
    let applications = phab_policy::ApplicationPolicies::from_config(&context.config);
    let policy = applications.custom_policy(APPLICATION_CLASS, DISABLE_USERS, POLICY_ADMIN);
    let mut subject = PolicySubject::new(
        Some(Phid::new(format!("PHID-APPS-{APPLICATION_CLASS}"))),
        ObjectKind::Other,
    )
    .with_policy(DISABLE_USERS, policy);
    subject.capabilities.push(DISABLE_USERS);
    let facts = loader::load_facts(
        &context.database,
        &context.config,
        &request.viewer,
        &[&subject],
        &NoExtendedObjects,
        request.now,
    )
    .await?;
    Ok(phab_policy::has_capability(
        &request.viewer,
        &subject,
        DISABLE_USERS,
        &facts,
    )?)
}

/// `CAN_EDIT` on a user: the user themself, or administrators for bots and
/// mailing lists.
fn can_edit(viewer: &Viewer, user: &EditedUser) -> bool {
    let is_self = viewer
        .phid
        .as_ref()
        .is_some_and(|phid| phid.as_str() == user.phid);
    is_self || (viewer.is_admin && (user.is_system_agent || user.is_mailing_list))
}

/// `loadXHeraldRulesHeader()`.
async fn saved_header(database: &Database, phid: &str) -> Result<Option<String>, MethodError> {
    let sql = format!(
        "SELECT header FROM {} WHERE phid = ?",
        database.table("herald", "herald_savedheader")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await?
        .map(|row| rows::text(&row, "header"))
        .transpose()?)
}

/// The validation errors of `validateTransactions()`, in the order of the
/// transaction types (`user.approve` before `user.disable`).
async fn validate(
    request: &ApiRequest<'_>,
    (actor, user): (&str, &EditedUser),
    changes: &[Change],
) -> Result<Vec<&'static str>, MethodError> {
    let mut errors = Vec::new();
    for change in changes {
        if let Change::Approved(approved) = *change
            && approved != user.is_approved
            && !request.viewer.is_admin
        {
            errors.push("You must be an administrator to approve users.");
        }
    }
    for change in changes {
        if let Change::Disabled(disabled) = *change
            && disabled != user.is_disabled
        {
            if !can_disable_users(request).await? {
                // A policy exception, whose message PHP builds.
                return Err(unsupported("users the actor may not disable"));
            }
            if actor == user.phid {
                errors.push("You can not enable or disable your own account.");
            }
        }
    }
    Ok(errors)
}

/// Plans the edit of `user` by `changes`, the transactions of one
/// `user.edit` call.
pub(crate) async fn plan(
    request: &ApiRequest<'_>,
    user: EditedUser,
    changes: &[Change],
) -> Result<Plan, MethodError> {
    let database = &request.context.database;
    let actor = request
        .viewer
        .phid
        .as_ref()
        .map(|phid| phid.as_str().to_owned())
        .ok_or_else(|| unsupported("edits by the omnipotent user"))?;
    // At most one transaction of each type: PHP applies them one after the
    // other.
    for (index, change) in changes.iter().enumerate() {
        if changes[..index]
            .iter()
            .any(|other| other.kind() == change.kind())
        {
            return Err(unsupported("several transactions of one type"));
        }
    }

    let errors = validate(request, (&actor, &user), changes).await?;
    match errors.as_slice() {
        [] => {}
        [error] => {
            return Err(pager::core_error(format!(
                "Validation errors:\n  - {error}"
            )));
        }
        _ => return Err(unsupported("several validation errors")),
    }

    // applyCapabilityChecks(): without transactions, the edit needs
    // CAN_EDIT; the user transactions need nothing.
    if changes.is_empty() && !can_edit(&request.viewer, &user) {
        return Err(unsupported("edits the actor may not make"));
    }

    let mut transactions = Vec::new();
    let mut is_disabled = user.is_disabled;
    for change in changes {
        let (old, new) = match *change {
            Change::Disabled(disabled) => (user.is_disabled, disabled),
            Change::Approved(approved) => (user.is_approved, approved),
        };
        if old == new {
            continue;
        }
        match change {
            Change::Disabled(_) => is_disabled = new,
            Change::Approved(_) => return Err(unsupported("approvals, which send mail")),
        }
        transactions.push(Planned {
            phid: Phid::generate(PhidType::TRANSACTION, Some(PhidType::USER)).into_string(),
            kind: change.kind(),
            old,
            new,
        });
    }

    if !user.has_certificate || !user.has_secret {
        return Err(unsupported(
            "users without a conduit certificate or account secret",
        ));
    }
    if !is_plain(&user.user_name) || !is_plain(&user.real_name) {
        return Err(unsupported("users whose names aren't printable ASCII"));
    }
    let tokens = tokenize(&format!("{} {}", user.user_name, user.real_name));
    let herald_header = saved_header(database, &user.phid).await?;
    Ok(Plan {
        actor,
        user,
        is_disabled,
        tokens,
        transactions,
        group_id: random::random_characters(32),
        herald_header,
    })
}

/// The publish task's data (`getWorkerState()`): users publish feed
/// stories but send no mail.
fn worker_data(plan: &Plan) -> String {
    let user = &plan.user.phid;
    let actor = &plan.actor;
    php::json_encode(&json!({
        "objectPHID": user,
        "actorPHID": actor,
        "xactionPHIDs": plan.transaction_phids(),
        "state": {
            "parentMessageID": null,
            "isNewObject": false,
            "heraldEmailPHIDs": [],
            "heraldForcedEmailPHIDs": [],
            "heraldHeader": plan.herald_header,
            "mailToPHIDs": [],
            "mailCCPHIDs": [],
            "feedNotifyPHIDs": [],
            "feedRelatedPHIDs": [user, actor],
            "feedShouldPublish": true,
            "mailShouldSend": false,
            "mustEncrypt": [],
            "mailStamps": [],
            "mailUnexpandablePHIDs": [],
            "mailMutedPHIDs": [],
            "webhookMap": [],
            "silent": null,
            "sendHistory": false,
            "excludeMailRecipientPHIDs": [],
            "custom": [],
            "custom.encoding": [],
        },
    }))
}

/// Writes the planned edit in one transaction, in PHP's order.
pub(crate) async fn write(request: &ApiRequest<'_>, plan: &Plan) -> Result<(), MethodError> {
    let database = &request.context.database;
    let now = request.now;
    let mut transaction = database.pool().begin().await?;
    let connection = &mut *transaction;
    let user = &plan.user;

    // PhabricatorUser::save(): Lisk saves every column; only these change.
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET isDisabled = ?, dateModified = ? WHERE id = ?",
        database.table("user", "user")
    ))
    .bind(plan.is_disabled)
    .bind(now)
    .bind(user.id)
    .execute(&mut *connection)
    .await?;
    let name_tokens = database.table("user", "user_nametoken");
    phab_db::sqlx::query(&format!("DELETE FROM {name_tokens} WHERE userID = ?"))
        .bind(user.id)
        .execute(&mut *connection)
        .await?;
    if !plan.tokens.is_empty() {
        let sql = format!(
            "INSERT INTO {name_tokens} (userID, token) VALUES {}",
            vec!["(?, ?)"; plan.tokens.len()].join(", ")
        );
        let mut query = phab_db::sqlx::query(&sql);
        for token in &plan.tokens {
            query = query.bind(user.id).bind(token.as_str());
        }
        query.execute(&mut *connection).await?;
    }
    let index_data = php::json_encode(&json!({
        "documentPHID": user.phid,
        "parameters": [],
        "strict": false,
    }));
    let index_task = worker::Task {
        class: "PhabricatorSearchWorker",
        data: &index_data,
        priority: PRIORITY_INDEX,
        object_phid: Some(&user.phid),
    };
    worker::schedule_task(connection, database, index_task, now).await?;

    let table = database.table("user", "user_transaction");
    let metadata = php::json_encode(&json!({"core.groupID": plan.group_id}));
    for planned in &plan.transactions {
        let (old, new) = (
            json!(planned.old).to_string(),
            json!(planned.new).to_string(),
        );
        let row = TransactionRow {
            phid: &planned.phid,
            author: &plan.actor,
            object: &user.phid,
            kind: planned.kind,
            old: &old,
            new: &new,
            metadata: &metadata,
            content_source: CONDUIT_SOURCE,
        };
        db_editor::insert_transaction(connection, &table, row, now).await?;
    }

    let data = worker_data(plan);
    let publish_task = worker::Task {
        class: "PhabricatorApplicationTransactionPublishWorker",
        data: &data,
        priority: worker::PRIORITY_ALERTS,
        object_phid: Some(&user.phid),
    };
    worker::schedule_task(connection, database, publish_task, now).await?;
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(phid: &str, bot: bool) -> EditedUser {
        EditedUser {
            id: 1,
            phid: phid.to_owned(),
            user_name: "bob".to_owned(),
            real_name: "Bob".to_owned(),
            is_disabled: false,
            is_approved: true,
            is_system_agent: bot,
            is_mailing_list: false,
            has_certificate: true,
            has_secret: true,
        }
    }

    #[test]
    fn tokenizes_names_like_php() {
        assert_eq!(
            tokenize("bob.smith Bob (Smith) bob"),
            vec!["bob.smith", "bob", "smith"]
        );
        assert_eq!(tokenize("x-y "), vec!["x", "y"]);
    }

    #[test]
    fn users_edit_themselves_and_admins_edit_bots() {
        let bob = Viewer::user(Phid::new("PHID-USER-bob"), false);
        let admin = Viewer::user(Phid::new("PHID-USER-admin"), true);
        assert!(can_edit(&bob, &user("PHID-USER-bob", false)));
        assert!(!can_edit(&admin, &user("PHID-USER-bob", false)));
        assert!(can_edit(&admin, &user("PHID-USER-bot", true)));
        assert!(!can_edit(&bob, &user("PHID-USER-bot", true)));
    }
}
