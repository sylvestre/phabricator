//! `PhabricatorRepositoryGitCommitMessageParserWorker`
//! (`PhabricatorRepositoryCommitMessageParserWorker::parseCommit()`): reads
//! the commit's author, committer and message from Git
//! (`DiffusionLowLevelCommitQuery`, through `internal.commit.search`),
//! resolves the identities, and writes the commit, its data and the
//! `IMPORTED_MESSAGE` flag.

use phab_db::Database;
use serde_json::{Value, json};

use crate::git::{WorkingCopy, ref_selector};
use crate::import::commit::{CommitData, FollowUp, ParserTask};
use crate::import::identity::Identity;
use crate::import::text::{summarize_commit_message, truncate};
use crate::import::{CHANGE_PARSER, IMPORTED_MESSAGE, ImportError};

/// `DiffusionCommitRef`, as `newDictionary()` gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRef {
    /// `authorEpoch`.
    pub author_epoch: Option<i64>,
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
    /// The commit hash.
    pub commit: String,
    /// The tree hash.
    pub tree: Option<String>,
}

/// `formatUser()`: `name <email>`, either alone, or `None`.
fn format_user(name: Option<&str>, email: Option<&str>) -> Option<String> {
    match (name.unwrap_or_default(), email.unwrap_or_default()) {
        ("", "") => None,
        (name, "") => Some(name.to_owned()),
        ("", email) => Some(email.to_owned()),
        (name, email) => Some(format!("{name} <{email}>")),
    }
}

impl CommitRef {
    /// `DiffusionLowLevelCommitQuery::loadGitCommitRef()`.
    pub async fn load(copy: &WorkingCopy, identifier: &str) -> Result<Self, ImportError> {
        let stdout = copy
            .execx(
                vec![
                    "log".to_owned(),
                    "-n".to_owned(),
                    "1".to_owned(),
                    "-n".to_owned(),
                    "1".to_owned(),
                    "--encoding=UTF-8".to_owned(),
                    "--format=%e%x00%cn%x00%ce%x00%an%x00%ae%x00%T%x00%at%x00%B%x00~".to_owned(),
                    ref_selector(identifier)?,
                    "--".to_owned(),
                ],
                None,
            )
            .await?;
        let info = crate::php::text(stdout)?;
        let mut parts: Vec<&str> = info.split('\0').collect();
        let encoding = parts.remove(0);
        if !encoding.is_empty() {
            // phutil_utf8_convert() from the commit's encoding.
            return Err(ImportError::unsupported("a commit with an encoding header"));
        }
        let part = |index: usize| {
            parts
                .get(index)
                .filter(|part| !part.is_empty())
                .map(|part| (*part).to_owned())
        };
        // `(int)`: digits up to the first other character.
        let author_epoch = part(5).and_then(|epoch| {
            let digits: String = epoch.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<i64>().ok().filter(|epoch| *epoch != 0)
        });
        Ok(Self {
            committer_name: part(0),
            committer_email: part(1),
            author_name: part(2),
            author_email: part(3),
            tree: part(4),
            author_epoch,
            message: part(6),
            commit: identifier.to_owned(),
        })
    }

    /// `newDictionary()`.
    pub fn to_dictionary(&self) -> Value {
        json!({
            "authorEpoch": self.author_epoch,
            "authorName": self.author_name,
            "authorEmail": self.author_email,
            "committerName": self.committer_name,
            "committerEmail": self.committer_email,
            "message": self.message,
            "hashes": [
                {"type": "gtcm", "value": self.commit},
                {"type": "gttr", "value": self.tree},
            ],
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

/// What the step did.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    /// Whether the step was skipped, as already done.
    pub skipped: bool,
    /// The next step, if queued.
    pub followup: Option<FollowUp>,
}

/// Checks what the step needs before it writes anything: the commit ref,
/// which comes from Git, and the identities it names.
pub async fn prepare(
    database: &Database,
    task: &ParserTask,
) -> Result<Option<CommitRef>, ImportError> {
    if task.should_skip_step(IMPORTED_MESSAGE) {
        return Ok(None);
    }
    let copy = task.repository.working_copy()?;
    // newCommitRef(): internal.commit.search finds the commit, then reads it.
    let commit_ref = CommitRef::load(&copy, &task.commit.identifier).await?;
    Identity::check(database, &commit_ref.author().unwrap_or_default()).await?;
    if let Some(committer) = commit_ref.committer() {
        Identity::check(database, &committer).await?;
    }
    Ok(Some(commit_ref))
}

/// `parseCommit()` with the ref [`prepare`] read.
pub async fn parse(
    database: &Database,
    task: &mut ParserTask,
    commit_ref: Option<CommitRef>,
) -> Result<Parsed, ImportError> {
    let skipped = commit_ref.is_none();
    if let Some(commit_ref) = commit_ref {
        update_commit_data(database, task, &commit_ref).await?;
    }
    Ok(Parsed {
        skipped,
        followup: task.followup(CHANGE_PARSER),
    })
}

/// `updateCommitData()`.
async fn update_commit_data(
    database: &Database,
    task: &mut ParserTask,
    commit_ref: &CommitRef,
) -> Result<(), ImportError> {
    let commit = &mut task.commit;
    let mut data = CommitData::load(database, commit.id).await?;
    // setCommitRef(), then getCommitRef() reads it back.
    data.set_detail("ref", commit_ref.to_dictionary());

    let author = commit_ref.author().unwrap_or_default();
    let committer = commit_ref.committer().filter(|name| !name.is_empty());
    let author_identity = Identity::resolve(database, &author, &commit.phid).await?;
    let committer_identity = match &committer {
        Some(committer) => Some(Identity::resolve(database, committer, &commit.phid).await?),
        None => None,
    };

    data.author_name = truncate(&author, Some(255), None);
    data.set_detail("authorEpoch", json!(commit_ref.author_epoch));
    data.set_detail("authorName", json!(commit_ref.author_name));
    data.set_detail("authorEmail", json!(commit_ref.author_email));
    data.set_detail("authorIdentityPHID", json!(author_identity.phid));
    data.set_detail("authorPHID", json!(author_identity.effective_user_phid));
    data.commit_message = commit_ref.message.clone().unwrap_or_default();
    if let (Some(committer), Some(identity)) = (&committer, &committer_identity) {
        data.set_detail("committer", json!(committer));
        data.set_detail("committerName", json!(commit_ref.committer_name));
        data.set_detail("committerEmail", json!(commit_ref.committer_email));
        data.set_detail("committerPHID", json!(identity.effective_user_phid));
        data.set_detail("committerIdentityPHID", json!(identity.phid));
        commit.committer_identity_phid = Some(identity.phid.clone());
    }
    commit.author_phid = data
        .detail("authorPHID")
        .and_then(Value::as_str)
        .map(str::to_owned);
    commit.author_identity_phid = Some(author_identity.phid.clone());
    commit.summary = summarize_commit_message(&data.commit_message);

    commit.save(database).await?;
    data.save(database).await?;
    commit
        .write_import_status_flag(database, IMPORTED_MESSAGE)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_users_like_php() {
        assert_eq!(
            format_user(Some("a"), Some("a@x")).as_deref(),
            Some("a <a@x>")
        );
        assert_eq!(format_user(None, Some("a@x")).as_deref(), Some("a@x"));
        assert_eq!(format_user(Some("a"), None).as_deref(), Some("a"));
        assert_eq!(format_user(Some(""), None), None);
    }
}
