//! The objects Rust indexes, loaded as `PhabricatorObjectQuery` loads them
//! for the omnipotent viewer, and their abstract documents
//! (`PhabricatorFulltextEngine::buildFulltextIndexes()`).
//!
//! The enrichment extensions run in the order of their class names:
//! custom fields (summary and test plan of revisions, description of
//! projects), Lisk's dates, projects and subscribers, transaction comments.
//! The projects and subscribers become relationships the Ferret engine
//! doesn't store, and `cluster.search` has no other engine than Ferret
//! (`mysql`, whose `reindexAbstractDocument()` does nothing), so they are
//! not read.

use phab_core::{Config, php};
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::sqlx::{self, Row};
use phab_db::{Database, DbError, rows};
use serde_json::Value;

use crate::ferret::{self, Engine, field};
use crate::{Error, text};

/// `PhabricatorProjectStatus::STATUS_ARCHIVED`.
const PROJECT_ARCHIVED: &str = "100";
/// `PhabricatorProjectDescriptionField`'s key.
const PROJECT_DESCRIPTION: &str = "std:project:internal:description";
/// `PhabricatorTransactionsFulltextEngineExtension`: the newest comments
/// it indexes.
const COMMENT_LIMIT: i64 = 1000;

/// The object types Rust indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `DifferentialRevision`.
    Revision,
    /// `PhabricatorUser`.
    User,
    /// `PhabricatorProject`.
    Project,
}

impl Kind {
    /// The type of `phid` (`phid_get_type()`), if Rust indexes it.
    pub fn of(phid: &str) -> Option<Self> {
        match phab_core::phid::phid_type_of(phid).as_str() {
            "DREV" => Some(Self::Revision),
            "USER" => Some(Self::User),
            "PROJ" => Some(Self::Project),
            _ => None,
        }
    }

    /// The object's database.
    pub fn database(self) -> &'static str {
        match self {
            Self::Revision => "differential",
            Self::User => "user",
            Self::Project => "project",
        }
    }

    /// The object's table.
    fn table(self) -> &'static str {
        match self {
            Self::Revision => "differential_revision",
            Self::User => "user",
            Self::Project => "project",
        }
    }

    /// `newFerretEngine()`.
    pub fn ferret(self) -> Engine {
        let (application, scope) = match self {
            Self::Revision => ("differential", "revision"),
            Self::User => ("user", "user"),
            Self::Project => ("project", "project"),
        };
        Engine {
            database: self.database(),
            application,
            scope,
        }
    }

    /// `getApplicationTransactionTemplate()`'s table.
    pub fn transaction_table(self) -> &'static str {
        match self {
            Self::Revision => "differential_transaction",
            Self::User => "user_transaction",
            Self::Project => "project_transaction",
        }
    }

    /// `getApplicationTransactionCommentObject()`'s table, if the
    /// transactions have comments.
    pub fn comment_table(self) -> Option<&'static str> {
        match self {
            Self::Revision => Some("differential_transaction_comment"),
            Self::User | Self::Project => None,
        }
    }
}

/// What each object type keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Details {
    Revision {
        title: String,
        status: String,
        summary: String,
        test_plan: String,
        author_phid: Option<String>,
    },
    User {
        user_name: String,
        real_name: String,
        is_disabled: bool,
        is_approved: bool,
        is_email_verified: bool,
    },
    Project {
        name: String,
        status: String,
    },
}

/// A loaded object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// Its type.
    pub kind: Kind,
    /// `id`.
    pub id: i64,
    /// `phid`.
    pub phid: String,
    date_created: i64,
    date_modified: i64,
    details: Details,
}

/// A text column, which must be UTF-8 as PHP's string functions need it.
fn utf8(row: &MySqlRow, column: &str) -> Result<String, Error> {
    let bytes = match row.try_get::<Option<Vec<u8>>, _>(column) {
        Ok(bytes) => bytes,
        Err(_) => row
            .try_get::<Option<String>, _>(column)
            .map(|text| text.map(String::into_bytes))
            .map_err(|_| {
                Error::Db(DbError::ColumnType {
                    column: column.to_owned(),
                })
            })?,
    };
    String::from_utf8(bytes.unwrap_or_default())
        .map_err(|_| Error::unsupported(format!("{column} which is not UTF-8")))
}

/// What custom fields change, which Rust doesn't follow.
fn check_custom_fields(config: &Config, kind: Kind) -> Result<(), Error> {
    let truthy = |key: &str| config.value(key).is_some_and(php::is_truthy);
    match kind {
        Kind::Revision => {
            if truthy("differential.custom-field-definitions") {
                return Err(Error::unsupported("differential custom fields"));
            }
        }
        Kind::User => {
            if truthy("user.custom-field-definitions") {
                return Err(Error::unsupported("user custom fields"));
            }
        }
        Kind::Project => {
            if config.value("projects.fields").is_some()
                || truthy("projects.custom-field-definitions")
                || truthy("projects.subtypes")
            {
                return Err(Error::unsupported("project custom fields or subtypes"));
            }
        }
    }
    Ok(())
}

/// The revision fields which add to the document
/// (`updateAbstractDocument()` of `DifferentialSummaryField`,
/// `DifferentialTestPlanField` and `DifferentialRevertPlanField`), in the
/// order of `differential.fields`.
fn revision_fields(config: &Config) -> Result<Vec<&'static str>, Error> {
    const SUMMARY: &str = "differential:summary";
    const TEST_PLAN: &str = "differential:test-plan";
    const REVERT_PLAN: &str = "phabricator:revert-plan";
    let spec = match config.value("differential.fields") {
        // The default lists every field, the summary first.
        None => return Ok(vec![SUMMARY, TEST_PLAN]),
        Some(Value::Object(spec)) => spec,
        Some(_) => return Err(Error::unsupported("an unusual differential.fields")),
    };
    let position = |key: &str| spec.keys().position(|listed| listed == key);
    let mut listed = Vec::new();
    for key in [SUMMARY, TEST_PLAN, REVERT_PLAN] {
        // The others follow in the order of the class map, not ported.
        let position = position(key)
            .ok_or_else(|| Error::unsupported("a differential.fields without every field"))?;
        // `isset($spec[$key]['disabled'])`, else `shouldDisableByDefault()`.
        let disabled = spec
            .get(key)
            .and_then(Value::as_object)
            .and_then(|entry| entry.get("disabled"))
            .filter(|value| !value.is_null())
            .map_or(key == REVERT_PLAN, php::is_truthy);
        // The summary can't be disabled.
        if disabled && key != SUMMARY {
            continue;
        }
        if key == REVERT_PLAN {
            return Err(Error::unsupported("the revert plan field"));
        }
        listed.push((position, key));
    }
    listed.sort_unstable();
    Ok(listed.into_iter().map(|(_, key)| key).collect())
}

/// Loads `phid` as `PhabricatorObjectQuery` does for the omnipotent viewer;
/// `None` when it doesn't load.
pub async fn load(
    database: &Database,
    config: &Config,
    kind: Kind,
    phid: &str,
) -> Result<Option<Object>, Error> {
    check_custom_fields(config, kind)?;
    let columns = match kind {
        Kind::Revision => "id, phid, title, status, summary, testPlan, authorPHID",
        Kind::User => "id, phid, userName, realName, isDisabled, isApproved, isEmailVerified",
        Kind::Project => "id, phid, name, status, parentProjectPHID, milestoneNumber",
    };
    let sql = format!(
        "SELECT {columns}, dateCreated, dateModified FROM {} WHERE phid = ?",
        database.table(kind.database(), kind.table())
    );
    let Some(row) = sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?
    else {
        return Ok(None);
    };
    let details = match kind {
        Kind::Revision => Details::Revision {
            title: utf8(&row, "title")?,
            status: rows::text(&row, "status")?,
            summary: utf8(&row, "summary")?,
            test_plan: utf8(&row, "testPlan")?,
            author_phid: rows::opt_text(&row, "authorPHID")?,
        },
        Kind::User => Details::User {
            user_name: utf8(&row, "userName")?,
            real_name: utf8(&row, "realName")?,
            is_disabled: rows::flag(&row, "isDisabled")?,
            is_approved: rows::flag(&row, "isApproved")?,
            is_email_verified: rows::flag(&row, "isEmailVerified")?,
        },
        Kind::Project => {
            // A subproject or milestone loads with its ancestors (and not
            // at all without them), and its membership is materialized
            // from them: not ported.
            if rows::opt_text(&row, "parentProjectPHID")?.is_some()
                || rows::opt_int(&row, "milestoneNumber")?.is_some()
            {
                return Err(Error::unsupported("subprojects and milestones"));
            }
            Details::Project {
                name: utf8(&row, "name")?,
                status: rows::text(&row, "status")?,
            }
        }
    };
    let id = rows::int(&row, "id")?;
    if kind == Kind::Project {
        let sql = format!(
            "SELECT id FROM {} WHERE parentProjectPHID = ? LIMIT 1",
            database.table("project", "project")
        );
        let child = sqlx::query(&sql)
            .bind(phid)
            .fetch_optional(database.pool())
            .await
            .map_err(DbError::from)?;
        if child.is_some() {
            return Err(Error::unsupported(
                "projects with subprojects or milestones",
            ));
        }
    }
    Ok(Some(Object {
        kind,
        id,
        phid: phid.to_owned(),
        date_created: rows::int(&row, "dateCreated")?,
        date_modified: rows::int(&row, "dateModified")?,
        details,
    }))
}

/// The document of an object, as the Ferret extension stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// The `*_fdocument` row.
    pub document: ferret::Document,
    /// The fields and ngrams.
    pub corpus: ferret::Corpus,
    /// For a project, the tokens `updateDatasourceTokens()` writes.
    pub tokens: Option<Vec<String>>,
}

/// `differential_reviewer` of a revision, in ID order.
async fn load_reviewers(database: &Database, phid: &str) -> Result<Vec<String>, Error> {
    let sql = format!(
        "SELECT reviewerPHID FROM {} WHERE revisionPHID = ? ORDER BY id ASC",
        database.table("differential", "differential_reviewer")
    );
    sqlx::query(&sql)
        .bind(phid)
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?
        .iter()
        .map(|row| Ok(rows::text(row, "reviewerPHID")?))
        .collect()
}

/// `PhabricatorTransactionsFulltextEngineExtension`: the comments of the
/// newest thousand transactions which have one, newest first; empty ones
/// (only white space) left out.
async fn load_comments(database: &Database, object: &Object) -> Result<Vec<String>, Error> {
    let Some(comment_table) = object.kind.comment_table() else {
        // `withComments(true)` on transactions without comments is an empty
        // query.
        return Ok(Vec::new());
    };
    let database_name = object.kind.database();
    let transactions = database.table(database_name, object.kind.transaction_table());
    let comments = database.table(database_name, comment_table);
    let sql = format!(
        "SELECT x.id, x.commentPHID FROM {transactions} x \
         JOIN {comments} c ON x.phid = c.transactionPHID \
         WHERE x.objectPHID = ? GROUP BY x.id ORDER BY x.id DESC LIMIT {COMMENT_LIMIT}"
    );
    let rows = sqlx::query(&sql)
        .bind(&object.phid)
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?;
    let comment_phids: Vec<Option<String>> = rows
        .iter()
        .map(|row| rows::opt_text(row, "commentPHID"))
        .collect::<Result<_, _>>()?;
    let wanted: Vec<&str> = comment_phids
        .iter()
        .flatten()
        .map(String::as_str)
        .filter(|phid| php::is_truthy_str(phid))
        .collect();
    let mut contents = std::collections::HashMap::new();
    for chunk in wanted.chunks(1000) {
        let sql = format!(
            "SELECT phid, content FROM {comments} WHERE phid IN ({})",
            phab_db::placeholders(chunk.len())
        );
        let found = phab_db::bind_all(sqlx::query(&sql), chunk.iter().copied())
            .fetch_all(database.pool())
            .await
            .map_err(DbError::from)?;
        for row in found {
            contents.insert(rows::text(&row, "phid")?, utf8(&row, "content")?);
        }
    }
    let mut result = Vec::new();
    for phid in comment_phids.iter().flatten() {
        if let Some(content) = contents.get(phid) {
            // `isEmptyComment()`: `trim()`'s default characters.
            if !content
                .trim_matches([' ', '\t', '\n', '\r', '\0', '\x0b'])
                .is_empty()
            {
                result.push(content.clone());
            }
        }
    }
    Ok(result)
}

/// The project's hashtags (`project_slug`), in ID order.
async fn load_slugs(database: &Database, phid: &str) -> Result<Vec<String>, Error> {
    let sql = format!(
        "SELECT slug FROM {} WHERE projectPHID = ? ORDER BY id",
        database.table("project", "project_slug")
    );
    sqlx::query(&sql)
        .bind(phid)
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?
        .iter()
        .map(|row| utf8(row, "slug"))
        .collect()
}

/// `PhabricatorProjectDescriptionField`'s value, if stored.
async fn load_description(database: &Database, phid: &str) -> Result<Option<String>, Error> {
    let sql = format!(
        "SELECT fieldValue FROM {} WHERE objectPHID = ? AND fieldIndex = ?",
        database.table("project", "project_customfieldstorage")
    );
    let index = phab_core::hash::digest_for_index(PROJECT_DESCRIPTION.as_bytes());
    let row = sqlx::query(&sql)
        .bind(phid)
        .bind(index)
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?;
    row.map(|row| utf8(&row, "fieldValue")).transpose()
}

/// `PhabricatorUser::isUserActivated()`.
fn is_user_activated(config: &Config, details: &Details) -> bool {
    let Details::User {
        is_disabled,
        is_approved,
        is_email_verified,
        ..
    } = details
    else {
        return false;
    };
    if *is_disabled || !*is_approved {
        return false;
    }
    *is_email_verified || !config.truthy("auth.require-email-verification")
}

/// Builds the abstract document of `object` and what the Ferret extension
/// makes of it.
pub async fn build(database: &Database, config: &Config, object: &Object) -> Result<Built, Error> {
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut tokens = None;
    let mut author_phid = None;
    let mut owner_phid = None;
    let is_closed;
    match &object.details {
        Details::Revision {
            title,
            status,
            summary,
            test_plan,
            author_phid: author,
        } => {
            fields.push((field::TITLE.to_owned(), title.clone()));
            // `DifferentialRevisionStatus`: only published and abandoned
            // revisions are closed.
            is_closed = matches!(status.as_str(), "published" | "abandoned");
            author_phid.clone_from(author);
            // Owners are the reviewers of a revision which needs review (the
            // last one stands), else the author.
            owner_phid = if status == "needs-review" {
                load_reviewers(database, &object.phid).await?.pop()
            } else {
                author.clone()
            };
            for key in revision_fields(config)? {
                let (field_key, value) = match key {
                    "differential:summary" => (field::BODY, summary),
                    _ => ("plan", test_plan),
                };
                if !value.is_empty() {
                    fields.push((field_key.to_owned(), value.clone()));
                }
            }
        }
        Details::User {
            user_name,
            real_name,
            ..
        } => {
            // `getFullName()`.
            let title = if real_name.is_empty() {
                user_name.clone()
            } else {
                format!("{user_name} ({real_name})")
            };
            fields.push((field::TITLE.to_owned(), title));
            is_closed = !is_user_activated(config, &object.details);
        }
        Details::Project { name, status } => {
            let slugs = load_slugs(database, &object.phid).await?;
            // `updateDatasourceTokens()`: the slugs, then the name.
            let mut strings = slugs.clone();
            strings.push(name.clone());
            tokens = Some(text::tokenize_string(&strings.join(" "))?);
            fields.push((field::TITLE.to_owned(), name.clone()));
            fields.push((field::BODY.to_owned(), slugs.join("\n")));
            if let Some(description) = load_description(database, &object.phid).await?
                && !description.is_empty()
            {
                fields.push((field::BODY.to_owned(), description));
            }
            is_closed = status == PROJECT_ARCHIVED;
        }
    }
    for comment in load_comments(database, object).await? {
        fields.push((field::COMMENT.to_owned(), comment));
    }
    let corpus = ferret::build(&fields)?;
    Ok(Built {
        document: ferret::Document {
            object_phid: object.phid.clone(),
            is_closed,
            epoch_created: object.date_created,
            epoch_modified: object.date_modified,
            author_phid,
            owner_phid,
        },
        corpus,
        tokens,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;

    fn config(key: &str, value: Value) -> Config {
        let mut config = Config::default();
        let mut layer = Map::new();
        layer.insert(key.to_owned(), value);
        config.push_layer("test", layer);
        config
    }

    #[test]
    fn orders_revision_fields_like_differential_fields() {
        assert_eq!(
            revision_fields(&Config::default()).unwrap(),
            ["differential:summary", "differential:test-plan"]
        );
        let spec = json!({
            "differential:test-plan": {"disabled": false},
            "phabricator:revert-plan": {"disabled": true},
            "differential:summary": {"disabled": true},
        });
        assert_eq!(
            revision_fields(&config("differential.fields", spec)).unwrap(),
            ["differential:test-plan", "differential:summary"]
        );
        let spec = json!({
            "differential:summary": {},
            "differential:test-plan": {"disabled": true},
            "phabricator:revert-plan": {},
        });
        assert_eq!(
            revision_fields(&config("differential.fields", spec)).unwrap(),
            ["differential:summary"]
        );
        let spec = json!({"differential:summary": {}});
        assert!(revision_fields(&config("differential.fields", spec)).is_err());
        let spec = json!({
            "differential:summary": {},
            "differential:test-plan": {},
            "phabricator:revert-plan": {"disabled": false},
        });
        assert!(revision_fields(&config("differential.fields", spec)).is_err());
    }
}
