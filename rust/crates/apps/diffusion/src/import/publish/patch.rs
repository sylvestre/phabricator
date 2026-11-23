//! `loadRawPatchText()`: the commit's patch for its mail, which
//! `diffusion.rawdiffquery` (`DiffusionGitRawDiffQuery`, three lines of
//! context) stores as a temporary file the repository gets attached
//! (`DiffusionFileFutureQuery`).
//!
//! Left to PHP: patches over `metamta.diffusion.byte-limit` or slower than
//! `metamta.diffusion.time-limit` (whose mail says so), patches Git fails
//! to make, patches large enough to be chunked, and the files `file.upload`
//! leaves to PHP (other storage engines, encryption, MIME types without
//! `PHAB_MIME_COMMAND`).

use std::time::Duration;

use phab_app_files::mime::MimeCommand;
use phab_app_files::storage::{self, Engines, Properties};
use phab_conduit::Context;
use phab_db::Database;
use serde_json::Map;

use crate::git::ref_selector;
use crate::import::ImportError;
use crate::import::commit::ParserTask;
use crate::import::publish::request;

/// `ArcanistGitAPI::GIT_MAGIC_ROOT_COMMIT`, the empty tree.
const GIT_MAGIC_ROOT_COMMIT: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// The TTL of the file: 48 hours.
const RELATIVE_TTL: i64 = 48 * 60 * 60;
/// `PhabricatorFile::attachToObject()`'s mode.
const MODE_ATTACH: &str = "attach";

/// The patch and the file storing it.
#[derive(Debug)]
pub struct Patch {
    /// The patch.
    pub text: Vec<u8>,
    file: storage::Prepared,
}

/// An integer option with PHP's default.
fn int_option(context: &Context, key: &str, default: i64) -> Result<i64, ImportError> {
    match context.config.value(key) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(serde_json::Value::Number(number)) => number
            .as_i64()
            .ok_or_else(|| ImportError::unsupported("unusual patch limits")),
        Some(_) => Err(ImportError::unsupported("unusual patch limits")),
    }
}

/// `DiffusionGitRawDiffQuery::newQueryFuture()` of the commit, run.
async fn raw_diff(task: &ParserTask, timeout: i64) -> Result<Vec<u8>, ImportError> {
    let copy = task.repository.working_copy()?;
    let commit = ref_selector(&task.commit.identifier)?;
    let parents = copy
        .execx(
            vec![
                "log".to_owned(),
                "-n".to_owned(),
                "1".to_owned(),
                "--format=%P".to_owned(),
                commit.clone(),
                "--".to_owned(),
            ],
            None,
        )
        .await?;
    let against = if parents.trim_ascii().is_empty() {
        GIT_MAGIC_ROOT_COMMIT.to_owned()
    } else {
        format!("{commit}^")
    };
    let timeout = u64::try_from(timeout)
        .ok()
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| ImportError::unsupported("patches without a time limit"))?;
    let output = copy
        .exec_with_timeout(
            [
                "diff",
                "-M",
                "-C",
                "--no-ext-diff",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "-U3",
                &against,
                &commit,
                "--",
                ".",
            ]
            .map(str::to_owned)
            .to_vec(),
            None,
            Duration::from_secs(timeout),
        )
        .await?;
    if !output.success() {
        return Err(ImportError::unsupported("patches Git fails to make"));
    }
    Ok(output.stdout)
}

/// Makes the patch and plans its file, as
/// `PhabricatorExecFutureFileUploadSource` uploads it: no name
/// (`basename()` of no path), no one may see it, gone after 48 hours.
/// `mime` detects its type.
pub async fn load(
    context: &Context,
    task: &ParserTask,
    mime: Option<&MimeCommand>,
    now: i64,
) -> Result<Patch, ImportError> {
    let time_limit = int_option(context, "metamta.diffusion.time-limit", 60)?;
    let byte_limit = int_option(context, "metamta.diffusion.byte-limit", 1024 * 1024)?;
    let text = raw_diff(task, time_limit).await?;
    let length = i64::try_from(text.len()).unwrap_or(i64::MAX);
    if byte_limit > 0 && length > byte_limit {
        return Err(ImportError::unsupported("patches over the byte limit"));
    }
    let engines = Engines::from_config(&context.config)?;
    if engines
        .chunk_threshold()
        .is_some_and(|threshold| length > threshold)
    {
        return Err(ImportError::unsupported("patches stored in chunks"));
    }
    let properties = Properties {
        name: Some(""),
        ttl: Some(now + RELATIVE_TTL),
        view_policy: Some("no-one"),
        ..Properties::default()
    };
    let request = request(context, Map::new(), now);
    let file = storage::from_data(&request, &text, &properties, mime).await?;
    Ok(Patch { text, file })
}

impl Patch {
    /// Writes the file and attaches it to the repository.
    pub async fn write(
        &self,
        database: &Database,
        task: &ParserTask,
        now: i64,
    ) -> Result<(), ImportError> {
        let mut connection = database.pool().acquire().await?;
        let phid = self
            .file
            .clone()
            .write(&mut connection, database, now)
            .await?;
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (objectPHID, filePHID, attachmentMode, attacherPHID, dateCreated, \
             dateModified) VALUES (?, ?, ?, NULL, ?, ?) ON DUPLICATE KEY UPDATE attachmentMode = \
             VALUES(attachmentMode), attacherPHID = VALUES(attacherPHID), dateModified = \
             VALUES(dateModified)",
            database.table("file", "file_attachment")
        ))
        .bind(&task.repository.phid)
        .bind(phid.as_str())
        .bind(MODE_ATTACH)
        .bind(now)
        .bind(now)
        .execute(&mut *connection)
        .await?;
        Ok(())
    }
}
