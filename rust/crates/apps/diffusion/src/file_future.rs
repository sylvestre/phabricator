//! `DiffusionFileFutureQuery`: the queries which store a Git command's
//! output as a temporary file the repository gets attached
//! (`diffusion.filecontentquery`, `diffusion.rawdiffquery`), and answer
//! with its PHID (`respondToConduitRequest()`).
//!
//! The file is what `PhabricatorExecFutureFileUploadSource` uploads: named
//! after the path (`basename()`), no one may see it, gone after 48 hours,
//! its MIME type detected.
//!
//! Left to PHP: commands Git fails (PHP raises a `CommandException` whose
//! message quotes the command) or which time out (`tooSlow`, which depends
//! on the host's speed), output large enough to be stored in chunks, the
//! files `file.upload` leaves to PHP (other storage engines, encryption,
//! MIME types without `PHAB_MIME_COMMAND`), and calls which may not write
//! (shadow mode, read-only installs).

use std::time::Duration;

use phab_app_files::mime::MimeCommand;
use phab_app_files::storage::{self, Engines, Properties};
use phab_conduit::{ApiRequest, MethodError};
use serde_json::{Value, json};

use crate::drequest::DiffusionRequest;

/// `phutil_units('48 hours in seconds')`.
const RELATIVE_TTL: i64 = 48 * 60 * 60;
/// `PhabricatorFileAttachment::MODE_ATTACH`.
const MODE_ATTACH: &str = "attach";
/// How long a command may run when the call gives no `timeout`: PHP waits
/// forever, Rust leaves slower commands to PHP.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// The `timeout` and `byteLimit` parameters
/// (`DiffusionFileFutureQuery::getConduitParameters()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// How long the command may run.
    pub timeout: Duration,
    /// The largest output stored; `None` for no limit.
    pub byte_limit: Option<i64>,
}

/// An `optional int` which is used when truthy.
fn truthy_int(value: Option<&Value>) -> Result<Option<i64>, MethodError> {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => Ok(None),
        Some(Value::Number(number)) => match number.as_i64() {
            Some(0) => Ok(None),
            Some(number) => Ok(Some(number)),
            None => Err(MethodError::Unsupported("an unusual integer parameter")),
        },
        Some(_) => Err(MethodError::Unsupported("an unusual integer parameter")),
    }
}

impl Limits {
    /// The limits of the call.
    pub fn from_request(request: &ApiRequest<'_>) -> Result<Self, MethodError> {
        let timeout = match truthy_int(request.value("timeout"))? {
            None => DEFAULT_TIMEOUT,
            Some(seconds) => Duration::from_secs(
                u64::try_from(seconds)
                    .map_err(|_| MethodError::Unsupported("negative timeouts"))?,
            ),
        };
        Ok(Self {
            timeout,
            byte_limit: truthy_int(request.value("byteLimit"))?,
        })
    }
}

/// PHP's `basename()` of a path.
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or_default()
}

/// Runs `git <argv>` in the working copy, as `executeQuery()` does.
pub async fn run(
    drequest: &DiffusionRequest,
    argv: Vec<String>,
    timeout: Duration,
) -> Result<Vec<u8>, MethodError> {
    let output = drequest.copy.exec_with_timeout(argv, None, timeout).await?;
    if !output.success() {
        return Err(MethodError::Unsupported(
            "a Git command which fails (PHP quotes it in its error)",
        ));
    }
    Ok(output.stdout)
}

/// `respondToConduitRequest()` with the command's output: the file, unless
/// the output is over the byte limit.
pub async fn respond(
    request: &ApiRequest<'_>,
    drequest: &DiffusionRequest,
    mime: Option<&MimeCommand>,
    (name, data): (&str, &[u8]),
    limits: Limits,
) -> Result<Value, MethodError> {
    storage::require_writes(request)?;
    let length = i64::try_from(data.len()).map_err(MethodError::internal)?;
    let engines = Engines::from_config(&request.context.config)?;
    let threshold = engines.chunk_threshold();
    // shouldChunkFile() reads past the threshold before anything is
    // written, so a byte limit under it stops the upload before it writes;
    // over it, a chunked file is partly written.
    if let Some(limit) = limits.byte_limit
        && length > limit
    {
        if threshold.is_some_and(|threshold| limit > threshold) {
            return Err(MethodError::Unsupported(
                "chunked output over the byte limit",
            ));
        }
        return Ok(json!({"tooSlow": false, "tooHuge": true, "filePHID": null}));
    }
    if threshold.is_some_and(|threshold| length > threshold) {
        return Err(MethodError::Unsupported("output stored in chunks"));
    }
    let properties = Properties {
        name: Some(name),
        ttl: Some(request.now + RELATIVE_TTL),
        view_policy: Some("no-one"),
        ..Properties::default()
    };
    let prepared = storage::from_data(request, data, &properties, mime).await?;
    let database = &request.context.database;
    let mut transaction = database.pool().begin().await?;
    let phid = prepared
        .write(&mut transaction, database, request.now)
        .await?;
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (objectPHID, filePHID, attachmentMode, attacherPHID, dateCreated, \
         dateModified) VALUES (?, ?, ?, NULL, ?, ?) ON DUPLICATE KEY UPDATE attachmentMode = \
         VALUES(attachmentMode), attacherPHID = VALUES(attacherPHID), dateModified = \
         VALUES(dateModified)",
        database.table("file", "file_attachment")
    ))
    .bind(drequest.repository.phid.as_str())
    .bind(phid.as_str())
    .bind(MODE_ATTACH)
    .bind(request.now)
    .bind(request.now)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(json!({"tooSlow": false, "tooHuge": false, "filePHID": phid.as_str()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_basenames_like_php() {
        assert_eq!(basename("a/b/c.txt"), "c.txt");
        assert_eq!(basename("c.txt"), "c.txt");
        assert_eq!(basename("a/b/"), "b");
        assert_eq!(basename(""), "");
        assert_eq!(basename("/"), "");
    }
}
