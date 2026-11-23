//! `closeRevisions()`: the revision a published commit's message names
//! (`DiffusionLowLevelCommitFieldsQuery`, which asks
//! `differential.parsecommitmessage`), when it belongs to the commit's
//! repository or to one Mozilla's `diffusion.legacy-repos-mapping` maps to
//! it. The commit's data records it, and
//! `DiffusionUpdateObjectAfterCommitWorker` (left to PHP) closes it.
//!
//! Left to PHP: revisions found by the hashes of the commit (when the
//! message names none), revisions without a repository (PHP fails on
//! them), and messages `differential.parsecommitmessage` leaves to PHP.

use phab_app_differential::commit_message::parse::ParseCommitMessage;
use phab_conduit::{Context, Method};
use phab_db::{Database, rows};
use serde_json::{Map, Value, json};

use crate::import::commit::{CommitData, FollowUp, ParserTask};
use crate::import::publish::request;
use crate::import::{ImportError, PRIORITY_DEFAULT};

/// The task closing the revision.
pub const UPDATE_WORKER: &str = "DiffusionUpdateObjectAfterCommitWorker";

/// `DiffusionCommitRef`, as `getCommitRef()` makes it of the commit's data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitRef {
    /// `authorName`.
    pub author_name: Option<String>,
    /// `authorEmail`.
    pub author_email: Option<String>,
    /// `committerName`.
    pub committer_name: Option<String>,
    /// `committerEmail`.
    pub committer_email: Option<String>,
    /// `message`.
    pub message: Option<String>,
    /// `hashes`: type and value.
    pub hashes: Vec<(Value, Value)>,
}

/// `formatUser()`.
fn format_user(name: Option<&str>, email: Option<&str>) -> Option<String> {
    match (name.unwrap_or_default(), email.unwrap_or_default()) {
        ("", "") => None,
        (name, "") => Some(name.to_owned()),
        ("", email) => Some(email.to_owned()),
        (name, email) => Some(format!("{name} <{email}>")),
    }
}

/// `getCommitDetailString()`: a detail cast to a string, `None` if empty.
fn detail_string(data: &CommitData, key: &str) -> Option<String> {
    let value = data.detail(key)?;
    let text = phab_core::php::string_cast(value);
    (!text.is_empty()).then(|| text.into_owned())
}

/// A string or null of the ref; other values are left to PHP.
fn ref_string(value: Option<&Value>) -> Result<Option<String>, ImportError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ImportError::unsupported("commit refs with unusual values")),
    }
}

impl CommitRef {
    /// `PhabricatorRepositoryCommitData::getCommitRef()`: the `ref` detail,
    /// completed by the other details (`+`, so a key the ref has wins,
    /// even when null).
    pub fn of(data: &CommitData) -> Result<Self, ImportError> {
        let map = match data.detail("ref") {
            Some(Value::Object(map)) => map.clone(),
            _ => Map::new(),
        };
        let field = |key: &str| -> Result<Option<String>, ImportError> {
            match map.get(key) {
                Some(value) => ref_string(Some(value)),
                None if key == "message" => Ok(Some(data.commit_message.clone())),
                None => Ok(detail_string(data, key)),
            }
        };
        let hashes = match map.get("hashes") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(list)) => list
                .iter()
                .map(|hash| match hash {
                    Value::Object(hash) => Ok((
                        hash.get("type").cloned().unwrap_or(Value::Null),
                        hash.get("value").cloned().unwrap_or(Value::Null),
                    )),
                    _ => Err(ImportError::unsupported("commit refs with unusual hashes")),
                })
                .collect::<Result<_, _>>()?,
            Some(Value::Object(map)) if map.is_empty() => Vec::new(),
            Some(_) => return Err(ImportError::unsupported("commit refs with unusual hashes")),
        };
        Ok(Self {
            author_name: field("authorName")?,
            author_email: field("authorEmail")?,
            committer_name: field("committerName")?,
            committer_email: field("committerEmail")?,
            message: field("message")?,
            hashes,
        })
    }

    /// `getAuthor()`.
    pub fn author(&self) -> Option<String> {
        format_user(self.author_name.as_deref(), self.author_email.as_deref())
    }

    /// `getCommitter()`.
    pub fn committer(&self) -> Option<String> {
        format_user(
            self.committer_name.as_deref(),
            self.committer_email.as_deref(),
        )
    }
}

/// `getAuthorString()`.
pub fn author_string(data: &CommitData, commit_ref: &CommitRef) -> Option<String> {
    commit_ref
        .author()
        .or_else(|| (!data.author_name.is_empty()).then(|| data.author_name.clone()))
}

/// `getCommitterString()`.
pub fn committer_string(data: &CommitData, commit_ref: &CommitRef) -> Option<String> {
    commit_ref
        .committer()
        .or_else(|| detail_string(data, "committer"))
}

/// `differential.parsecommitmessage` of `corpus`, partially (as
/// `DiffusionLowLevelCommitFieldsQuery` and `newStandardParser()` without
/// missing field errors parse it).
pub async fn parse_message(
    context: &Context,
    corpus: &str,
    now: i64,
) -> Result<Value, ImportError> {
    let mut params = Map::new();
    params.insert("corpus".to_owned(), json!(corpus));
    params.insert("partial".to_owned(), json!(true));
    let request = request(context, params, now);
    Ok(ParseCommitMessage.execute(&request).await?)
}

/// What `closeRevisions()` reads of a revision.
struct Revision {
    phid: String,
    repository_phid: Option<String>,
}

async fn load_revision(database: &Database, id: i64) -> Result<Option<Revision>, ImportError> {
    let sql = format!(
        "SELECT phid, repositoryPHID FROM {} WHERE id = ?",
        database.table("differential", "differential_revision")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(id)
        .fetch_optional(database.pool())
        .await?;
    row.map(|row| {
        Ok(Revision {
            phid: rows::text(&row, "phid")?,
            repository_phid: rows::opt_text(&row, "repositoryPHID")?,
        })
    })
    .transpose()
    .map_err(|error: phab_db::DbError| ImportError::Database(error))
}

/// The PHID of the revision with this ID, if it exists
/// (`DiffusionCommitRevisionQuery::loadRevisionForCommit()`).
pub async fn revision_phid(database: &Database, id: i64) -> Result<Option<String>, ImportError> {
    Ok(load_revision(database, id)
        .await?
        .map(|revision| revision.phid))
}

/// The callsign of a repository, `None` without one; an error when the
/// repository doesn't exist (PHP attaches no repository and fails).
async fn callsign(database: &Database, phid: &str) -> Result<Option<String>, ImportError> {
    let sql = format!(
        "SELECT callsign FROM {} WHERE phid = ?",
        database.table("repository", "repository")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await?
        .ok_or_else(|| ImportError::unsupported("revisions of missing repositories"))?;
    Ok(rows::opt_text(&row, "callsign")?)
}

/// Whether a revision matches the commit's hashes
/// (`DifferentialRevisionQuery::withCommitHashes()`).
async fn matches_hashes(
    database: &Database,
    hashes: &[(Value, Value)],
) -> Result<bool, ImportError> {
    if hashes.is_empty() {
        return Ok(false);
    }
    let clauses = vec!["(hash.type = ? AND hash.hash = ?)"; hashes.len()].join(" OR ");
    let sql = format!(
        "SELECT 1 FROM {} hash JOIN {} r ON r.id = hash.revisionID WHERE {clauses} LIMIT 1",
        database.table("differential", "differential_revisionhash"),
        database.table("differential", "differential_revision"),
    );
    let mut query = phab_db::sqlx::query(&sql);
    for (kind, value) in hashes {
        query = query
            .bind(phab_core::php::string_cast(kind).into_owned())
            .bind(phab_core::php::string_cast(value).into_owned());
    }
    Ok(query.fetch_optional(database.pool()).await?.is_some())
}

/// What closing the revision writes.
#[derive(Debug, Clone, PartialEq)]
pub struct Closing {
    revision_id: Value,
    revision_phid: String,
    reviewer_phid: Option<Value>,
    match_data: Value,
}

impl Closing {
    /// The details `closeRevisions()` sets on the commit's data.
    pub fn apply(&self, data: &mut CommitData) {
        if let Some(reviewer) = &self.reviewer_phid {
            data.set_detail("reviewerPHID", reviewer.clone());
        }
        data.set_detail("differential.revisionID", self.revision_id.clone());
        data.set_detail("revisionMatchData", self.match_data.clone());
    }

    /// `queueObjectUpdate()` of the revision:
    /// `DiffusionUpdateObjectAfterCommitWorker` at `PRIORITY_DEFAULT`,
    /// without an object.
    pub fn followup(&self, task: &ParserTask) -> FollowUp {
        FollowUp {
            class: UPDATE_WORKER,
            data: json!({
                "commitPHID": task.commit.phid,
                "objectPHID": self.revision_phid,
                "properties": {"revisionMatchData": self.match_data},
            }),
            priority: PRIORITY_DEFAULT,
            object_phid: None,
            container_phid: None,
        }
    }
}

/// `diffusion.legacy-repos-mapping`: whether it maps the revision's
/// repository to the commit's (`array_key_exists()` and `===`).
fn is_mapped(
    context: &Context,
    revision: Option<&str>,
    commit: Option<&str>,
) -> Result<bool, ImportError> {
    let config = context.config.value("diffusion.legacy-repos-mapping");
    if !config.is_some_and(phab_core::php::is_truthy) {
        return Ok(false);
    }
    let Some(Value::Object(mapping)) = config else {
        return Err(ImportError::unsupported(
            "a diffusion.legacy-repos-mapping which is not a dictionary",
        ));
    };
    // array_key_exists() of a null callsign is deprecated, which PHP's
    // error handler may raise.
    let Some(revision) = revision else {
        return Err(ImportError::unsupported(
            "legacy mappings of repositories without a callsign",
        ));
    };
    let Some(mapped) = mapping.get(revision) else {
        return Ok(false);
    };
    Ok(match (mapped, commit) {
        (Value::String(mapped), Some(commit)) => mapped == commit,
        (Value::Null, None) => true,
        _ => false,
    })
}

/// Plans `closeRevisions()`: the revision to close, if any.
pub async fn plan(
    context: &Context,
    task: &ParserTask,
    data: &CommitData,
    now: i64,
) -> Result<Option<Closing>, ImportError> {
    let database = &context.database;
    let commit_ref = CommitRef::of(data)?;
    let Some(message) = &commit_ref.message else {
        return Err(ImportError::unsupported("commit refs without a message"));
    };
    let result = parse_message(context, message, now).await?;
    let fields = &result["fields"];
    let revision_id = fields.get("revisionID").cloned().unwrap_or(Value::Null);
    let id = match &revision_id {
        Value::Null => None,
        Value::Number(number) => number.as_i64().filter(|id| *id != 0),
        _ => return Err(ImportError::unsupported("unusual revision IDs")),
    };
    let Some(id) = id else {
        if matches_hashes(database, &commit_ref.hashes).await? {
            return Err(ImportError::unsupported(
                "revisions found by the hashes of a commit",
            ));
        }
        return Ok(None);
    };
    let Some(revision) = load_revision(database, id).await? else {
        return Ok(None);
    };
    let Some(repository_phid) = &revision.repository_phid else {
        return Err(ImportError::unsupported("revisions without a repository"));
    };
    let revision_callsign = callsign(database, repository_phid).await?;
    let commit_callsign = task.repository.callsign.clone();
    let must_close = revision_callsign == commit_callsign
        || is_mapped(
            context,
            revision_callsign.as_deref(),
            commit_callsign.as_deref(),
        )?;
    if !must_close {
        return Ok(None);
    }
    let reviewer_phid = match fields.get("reviewedByPHIDs") {
        Some(Value::Array(phids)) => phids.first().cloned(),
        _ => None,
    };
    let info = &result["revisionIDFieldInfo"];
    // getRevisionMatchData(), in the order of its defaults.
    let match_data = json!({
        "usedURI": true,
        "foundURI": info["value"],
        "validDomain": info["validDomain"],
        "matchHashType": null,
        "matchHashValue": null,
    });
    Ok(Some(Closing {
        revision_id,
        revision_phid: revision.phid,
        reviewer_phid,
        match_data,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(details: Value, message: &str, author: &str) -> CommitData {
        let Value::Object(details) = details else {
            panic!("details are a dictionary");
        };
        CommitData {
            id: Some(1),
            commit_id: 1,
            author_name: author.to_owned(),
            commit_message: message.to_owned(),
            details,
        }
    }

    #[test]
    fn reads_the_ref_like_php() {
        // Without a ref: the details and the message.
        let plain = data(
            json!({"authorName": "alice", "authorEmail": "", "committer": "bob"}),
            "Fix",
            "alice <a@x>",
        );
        let commit_ref = CommitRef::of(&plain).unwrap();
        assert_eq!(commit_ref.message.as_deref(), Some("Fix"));
        assert_eq!(commit_ref.author().as_deref(), Some("alice"));
        assert_eq!(
            committer_string(&plain, &commit_ref).as_deref(),
            Some("bob")
        );
        // A ref's null wins over the details.
        let with_ref = data(
            json!({"ref": {"authorName": null, "message": null, "hashes": [
                {"type": "gtcm", "value": "abc"}
            ]}, "authorName": "alice"}),
            "Fix",
            "",
        );
        let commit_ref = CommitRef::of(&with_ref).unwrap();
        assert_eq!(commit_ref.message, None);
        assert_eq!(commit_ref.author_name, None);
        assert_eq!(author_string(&with_ref, &commit_ref), None);
        assert_eq!(commit_ref.hashes, [(json!("gtcm"), json!("abc"))]);
    }
}
