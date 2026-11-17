//! `user.search`: `PhabricatorPeopleSearchEngine` through
//! `buildConduitResponse()`.
//!
//! Ported: every constraint but the fulltext `query`, the `newest` and
//! `oldest` orders, paging, the fields and the `external-accounts`
//! attachment (moz-extensions). Left to PHP: `query`, the `relevance`
//! order, saved queries and the `availability` attachment, which needs
//! the calendar.

use std::collections::HashMap;
use std::fmt::Write;

use phab_conduit::pager::{self, Direction, Paging, PolicyAwareQuery};
use phab_conduit::search::{self, Clauses, Constraints, IdOrder};
use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::Phid;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::loader::NoExtendedObjects;
use phab_policy::{
    Capability, ObjectKind, POLICY_ADMIN, POLICY_NOONE, POLICY_PUBLIC, PolicySubject,
};
use serde_json::{Value, json};

use crate::external_account::{self, ExternalAccount};

const QUERY_CLASS: &str = "PhabricatorPeopleQuery";

/// `getValidConstraintKeys()` of the engine's fields.
const VALID_CONSTRAINTS: &[&str] = &[
    "usernames",
    "nameLike",
    "isAdmin",
    "isDisabled",
    "isBot",
    "isMailingList",
    "needsApproval",
    "createdStart",
    "createdEnd",
    "query",
    "ids",
    "phids",
];

const ORDERS: &[&str] = &["newest", "created", "oldest", "relevance"];

const ATTACHMENTS: &[&str] = &["availability", "external-accounts"];

/// `PeopleBrowseUserDirectoryCapability`.
const BROWSE: Capability = Capability("people.browse");

/// The ported constraints, in the engine's field order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Filters {
    usernames: Option<Vec<String>>,
    name_like: Option<String>,
    flags: Vec<(&'static str, bool)>,
    created: (Option<i64>, Option<i64>),
    ids: Option<Vec<i64>>,
    phids: Option<Vec<String>>,
}

impl Filters {
    /// `buildWhereClauseParts()`.
    fn add_conditions(&self, clauses: &mut Clauses) {
        if let Some(usernames) = &self.usernames {
            clauses.in_list("userName", search::strings(usernames));
        }
        if let Some(phids) = &self.phids {
            clauses.in_list("phid", search::strings(phids));
        }
        if let Some(ids) = &self.ids {
            clauses.in_list("id", ids.iter().map(|id| json!(id)));
        }
        if let Some(start) = self.created.0 {
            clauses.condition("dateCreated >= ?", [json!(start)]);
        }
        if let Some(end) = self.created.1 {
            clauses.condition("dateCreated <= ?", [json!(end)]);
        }
        for (column, flag) in &self.flags {
            clauses.condition(format!("{column} = ?"), [json!(i64::from(*flag))]);
        }
        if let Some(name) = &self.name_like {
            let pattern = json!(format!("%{}%", phab_db::like_escape(name)));
            clauses.condition(
                "userName LIKE ? OR realName LIKE ?",
                [pattern.clone(), pattern],
            );
        }
    }

    /// `buildQueryFromParameters()`: empty values are no constraint.
    fn parse(constraints: &Constraints, strict: bool) -> Result<Self, MethodError> {
        let mut filters = Self::default();
        if let Some((key, value)) = constraints.get(&["usernames"]) {
            // An empty list is no constraint, as in PHP.
            let usernames = search::string_list(key, value, true)?;
            filters.usernames = Some(usernames).filter(|names| !names.is_empty());
        }
        if let Some((key, value)) = constraints.get(&["nameLike"]) {
            filters.name_like = Some(search::string(key, value)?).filter(|name| !name.is_empty());
        }
        for (constraint, column, inverted) in [
            ("isAdmin", "isAdmin", false),
            ("isDisabled", "isDisabled", false),
            ("isBot", "isSystemAgent", false),
            ("isMailingList", "isMailingList", false),
            ("needsApproval", "isApproved", true),
        ] {
            if let Some((key, value)) = constraints.get(&[constraint]) {
                let flag = search::bool(key, value, strict)?;
                filters.flags.push((column, flag != inverted));
            }
        }
        if let Some((key, value)) = constraints.get(&["createdStart"]) {
            filters.created.0 = Some(search::epoch(key, value, strict)?);
        }
        if let Some((key, value)) = constraints.get(&["createdEnd"]) {
            filters.created.1 = Some(search::epoch(key, value, strict)?);
        }
        if let Some((key, value)) = constraints.get(&["query"])
            && !search::string(key, value)?.is_empty()
        {
            return Err(MethodError::Unsupported("fulltext user search"));
        }
        if let Some((key, value)) = constraints.get(&["ids"]) {
            filters.ids = Some(search::int_list(key, value, strict)?);
        }
        if let Some((key, value)) = constraints.get(&["phids"]) {
            filters.phids = Some(search::string_list(key, value, false)?);
        }
        Ok(filters)
    }
}

/// A `user` row, as the search shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UserRow {
    id: i64,
    phid: Phid,
    user_name: String,
    real_name: String,
    date_created: i64,
    date_modified: i64,
    is_system_agent: bool,
    is_mailing_list: bool,
    is_disabled: bool,
    is_admin: bool,
    is_email_verified: bool,
    is_approved: bool,
}

const COLUMNS: &str = "id, phid, userName, realName, dateCreated, dateModified, isSystemAgent, \
     isMailingList, isDisabled, isAdmin, isEmailVerified, isApproved";

impl UserRow {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            user_name: rows::text(row, "userName")?,
            real_name: rows::text(row, "realName")?,
            date_created: rows::int(row, "dateCreated")?,
            date_modified: rows::int(row, "dateModified")?,
            is_system_agent: rows::flag(row, "isSystemAgent")?,
            is_mailing_list: rows::flag(row, "isMailingList")?,
            is_disabled: rows::flag(row, "isDisabled")?,
            is_admin: rows::flag(row, "isAdmin")?,
            is_email_verified: rows::flag(row, "isEmailVerified")?,
            is_approved: rows::flag(row, "isApproved")?,
        })
    }

    /// `getPolicy()`: everyone sees users; bots and lists are edited by
    /// administrators, people by nobody.
    fn edit_policy(&self) -> &'static str {
        if self.is_system_agent || self.is_mailing_list {
            POLICY_ADMIN
        } else {
            POLICY_NOONE
        }
    }

    /// `getFieldValuesForConduit()`'s roles.
    fn roles(&self, verification_required: bool) -> Vec<&'static str> {
        let activated = !self.is_disabled
            && self.is_approved
            && (!verification_required || self.is_email_verified);
        [
            (self.is_disabled, "disabled"),
            (self.is_system_agent, "bot"),
            (self.is_mailing_list, "list"),
            (self.is_admin, "admin"),
            (self.is_email_verified, "verified"),
            (self.is_approved, "approved"),
            (activated, "activated"),
        ]
        .into_iter()
        .filter_map(|(has, role)| has.then_some(role))
        .collect()
    }
}

/// `PhabricatorPeopleQuery` with the ported constraints.
struct Users<'a> {
    request: &'a ApiRequest<'a>,
    filters: Filters,
    order: IdOrder,
}

impl PolicyAwareQuery for Users<'_> {
    type Row = UserRow;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &UserRow) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<UserRow>, MethodError> {
        let database = &self.request.context.database;
        let mut clauses = Clauses::default();
        // A cursor's own query has none of the constraints.
        if let Some(ids) = ids {
            clauses.in_list("id", ids.iter().map(|id| json!(id)));
        } else {
            self.filters.add_conditions(&mut clauses);
        }
        let (comparison, order) = search::id_paging(self.order, direction);
        if let Some(after) = after {
            clauses.condition(format!("id {comparison} ?"), [json!(after)]);
        }
        let mut tail = format!(" ORDER BY id {order}");
        if let Some(limit) = limit {
            let _ = write!(tail, " LIMIT {limit}");
        }
        let sql = clauses.sql(
            &format!("SELECT {COLUMNS} FROM {}", database.table("user", "user")),
            &tail,
        );
        Ok(clauses
            .bind(phab_db::sqlx::query(&sql))
            .fetch_all(database.pool())
            .await?
            .iter()
            .map(UserRow::from_row)
            .collect::<Result<_, _>>()?)
    }

    async fn filter(&self, rows: Vec<UserRow>) -> Result<Vec<UserRow>, MethodError> {
        let context = self.request.context;
        let subjects: Vec<PolicySubject> = rows
            .iter()
            .map(|user| {
                PolicySubject::new(Some(user.phid.clone()), ObjectKind::Other)
                    .with_policy(Capability::VIEW, POLICY_PUBLIC)
                    .with_policy(Capability::EDIT, user.edit_policy())
            })
            .collect();
        let refs: Vec<&PolicySubject> = subjects.iter().collect();
        let facts = phab_policy::loader::load_facts(
            &context.database,
            &context.config,
            &self.request.viewer,
            &refs,
            &NoExtendedObjects,
            self.request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        let passed = phab_policy::filter(&self.request.viewer, &refs, &[Capability::VIEW], &facts)
            .map_err(MethodError::internal)?;
        Ok(rows
            .into_iter()
            .zip(passed)
            .filter_map(|(row, visible)| visible.then_some(row))
            .collect())
    }
}

/// Whether the viewer may browse the user directory; if not, the search
/// only finds them (`buildQueryFromParameters()`).
async fn can_browse(request: &ApiRequest<'_>) -> Result<bool, MethodError> {
    let context = request.context;
    let applications = phab_policy::ApplicationPolicies::from_config(&context.config);
    let allow_public = context.config.allow_public();
    let mut subject = applications.subject(crate::APPLICATION.class, allow_public);
    subject.capabilities.push(BROWSE);
    subject = subject.with_policy(
        BROWSE,
        applications.policy(crate::APPLICATION.class, BROWSE, allow_public),
    );
    let facts = phab_policy::loader::load_facts(
        &context.database,
        &context.config,
        &request.viewer,
        &[&subject],
        &NoExtendedObjects,
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    phab_policy::has_capability(&request.viewer, &subject, BROWSE, &facts)
        .map_err(MethodError::internal)
}

/// `ExternalAccountsSearchEngineAttachment`: each user's accounts with an
/// identifier, newest first, as the identifier and the provider's name.
async fn external_accounts(
    database: &Database,
    users: &[UserRow],
) -> Result<HashMap<Phid, Vec<Value>>, MethodError> {
    if users.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT account.phid, account.userPHID, config.providerClass FROM {} account \
         JOIN {} config ON config.phid = account.providerConfigPHID \
         WHERE account.userPHID IN ({}) ORDER BY account.id DESC",
        database.table("user", "user_externalaccount"),
        database.table("auth", "auth_providerconfig"),
        phab_db::placeholders(users.len()),
    );
    let rows = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        users.iter().map(|user| user.phid.as_str()),
    )
    .fetch_all(database.pool())
    .await?;
    let mut accounts = Vec::new();
    for row in &rows {
        let provider = match rows::text(row, "providerClass")?.as_str() {
            "PhabricatorBMOAuthProvider" => "bmo",
            "PhabricatorPasswordAuthProvider" => "username/password",
            _ => {
                return Err(MethodError::Unsupported(
                    "external accounts of other providers",
                ));
            }
        };
        accounts.push((
            ExternalAccount {
                phid: Phid::new(rows::text(row, "phid")?),
                user_phid: rows::opt_text(row, "userPHID")?,
            },
            provider,
        ));
    }
    let identifiers = external_account::first_identifiers(
        database,
        &accounts
            .iter()
            .map(|(account, _)| account.clone())
            .collect::<Vec<_>>(),
    )
    .await?;
    let mut by_user: HashMap<Phid, Vec<Value>> = HashMap::new();
    for (account, provider) in accounts {
        // PHP skips accounts without an identifier, or whose identifier is
        // falsy.
        let Some(identifier) = identifiers
            .get(&account.phid)
            .filter(|identifier| !identifier.is_empty() && identifier.as_str() != "0")
        else {
            continue;
        };
        if let Some(user) = account.user_phid {
            by_user
                .entry(Phid::new(user))
                .or_default()
                .push(json!({"id": identifier, "type": provider}));
        }
    }
    Ok(by_user)
}

/// `user.search`.
pub struct UserSearch;

impl Method for UserSearch {
    fn name(&self) -> &'static str {
        "user.search"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "queryKey",
            "constraints",
            "attachments",
            "order",
            "before",
            "after",
            "limit",
        ]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            search::check_query_key(request)?;
            let constraints = Constraints::parse(request.value("constraints"), VALID_CONSTRAINTS)?;
            let mut filters = Filters::parse(&constraints, request.strictly_typed)?;
            let requested =
                search::requested_attachments(request.value("attachments"), ATTACHMENTS)?;
            if requested.contains(&"availability") {
                return Err(MethodError::Unsupported("the availability attachment"));
            }
            let order = IdOrder::parse(request.value("order"), QUERY_CLASS, ORDERS)?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            if !can_browse(request).await? {
                let Some(phid) = &request.viewer.phid else {
                    return Err(MethodError::Unsupported("a viewer without a PHID"));
                };
                // The engine extensions apply the phids constraint after
                // this, so a phids constraint given replaces it, as in PHP.
                filters
                    .phids
                    .get_or_insert_with(|| vec![phid.as_str().to_owned()]);
            }
            let (start, direction) = paging.start();
            let query = Users {
                request,
                filters,
                order,
            };
            let found = pager::execute(&query, start, direction, paging.size.value + 1).await?;
            let page = paging.pager().slice(found);
            let database = &request.context.database;
            let accounts = if requested.contains(&"external-accounts") {
                external_accounts(database, &page.items).await?
            } else {
                HashMap::new()
            };
            let verification_required = request
                .context
                .config
                .truthy("auth.require-email-verification");
            let data: Vec<Value> = page
                .items
                .iter()
                .map(|user| {
                    let mut attachments = serde_json::Map::new();
                    for key in &requested {
                        if *key == "external-accounts" {
                            attachments.insert(
                                (*key).to_owned(),
                                json!({
                                    "external-accounts":
                                        accounts.get(&user.phid).cloned().unwrap_or_default(),
                                }),
                            );
                        }
                    }
                    json!({
                        "id": user.id,
                        "type": "USER",
                        "phid": user.phid.as_str(),
                        "fields": {
                            "username": user.user_name,
                            "realName": user.real_name,
                            "roles": user.roles(verification_required),
                            "dateCreated": user.date_created,
                            "dateModified": user.date_modified,
                            "policy": {"view": POLICY_PUBLIC, "edit": user.edit_policy()},
                        },
                        "attachments": attachments,
                    })
                })
                .collect();
            Ok(search::response(request, &data, &paging, &page, |user| {
                user.id.to_string()
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: &Value, strict: bool) -> Result<Filters, MethodError> {
        Filters::parse(&Constraints::parse(Some(value), VALID_CONSTRAINTS)?, strict)
    }

    #[test]
    fn constraints_follow_php() {
        assert_eq!(
            parse(
                &json!({"usernames": ["alice"], "needsApproval": true, "nameLike": ""}),
                true
            ),
            Ok(Filters {
                usernames: Some(vec!["alice".to_owned()]),
                flags: vec![("isApproved", false)],
                ..Filters::default()
            })
        );
        assert_eq!(
            parse(&json!({"isBot": "true", "ids": ["3"]}), false),
            Ok(Filters {
                flags: vec![("isSystemAgent", true)],
                ids: Some(vec![3]),
                ..Filters::default()
            })
        );
        assert!(matches!(
            parse(&json!({"query": "alice"}), true),
            Err(MethodError::Unsupported(_))
        ));
        assert_eq!(parse(&json!({"query": ""}), true), Ok(Filters::default()));
        assert_eq!(
            parse(&json!({"isAdmin": 1}), true),
            Err(pager::core_error(
                "Error while reading \"isAdmin\": Expected boolean (true or false), got \
                 something else."
                    .to_owned()
            ))
        );
    }

    #[test]
    fn roles_like_php() {
        let user = UserRow {
            id: 1,
            phid: Phid::new("PHID-USER-1"),
            user_name: "a".to_owned(),
            real_name: String::new(),
            date_created: 1,
            date_modified: 1,
            is_system_agent: true,
            is_mailing_list: false,
            is_disabled: false,
            is_admin: false,
            is_email_verified: false,
            is_approved: true,
        };
        assert_eq!(user.roles(false), ["bot", "approved", "activated"]);
        assert_eq!(user.roles(true), ["bot", "approved"]);
        assert_eq!(user.edit_policy(), POLICY_ADMIN);
    }
}
