//! `file.allocate` (`FileAllocateConduitAPIMethod`): finds a file to reuse
//! or resume, tells the client to upload small files, and allocates the
//! chunks of large ones.
//!
//! Ported: every parameter given with the types PHP checks (integer
//! lengths and TTLs, string names, hashes and policies). Left to PHP:
//! other types, and files found which the viewer can't see.

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::php;
use serde_json::{Value, json};

use crate::File;
use crate::search::{self, APPLICATION};
use crate::storage::{self, Engines, Properties};
use crate::upload::{author, optional_string};

/// The one visible file among `candidates` (`executeOne()`).
async fn one(request: &ApiRequest<'_>, candidates: Vec<File>) -> Result<Option<File>, MethodError> {
    let count = candidates.len();
    let mut visible = search::visible_files(request, candidates).await?;
    if visible.len() != count {
        return Err(MethodError::Unsupported("files the viewer can't see"));
    }
    if visible.len() > 1 {
        return Err(storage::core_error("Expected a single result!"));
    }
    Ok(visible.pop())
}

/// The newest partial file of `author` with this name and length, which
/// an upload without a hash resumes.
async fn partial_file(
    request: &ApiRequest<'_>,
    author: &str,
    name: Option<&str>,
    length: i64,
) -> Result<Option<File>, MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT id FROM {} WHERE authorPHID IN (?) AND name IN (?) AND byteSize >= ? \
         AND byteSize <= ? AND isPartial = 1 ORDER BY id DESC LIMIT 1",
        database.table("file", "file")
    );
    let ids = phab_db::sqlx::query(&sql)
        .bind(author)
        .bind(name)
        .bind(length)
        .bind(length)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| phab_db::rows::int(row, "id"))
        .collect::<Result<Vec<i64>, _>>()?;
    let candidates = crate::load_by_ids(database, &ids).await?;
    one(request, candidates).await
}

/// `file.allocate`.
pub struct FileAllocate;

/// `readPropertiesFromParameters()` type-checks the `name` the method always
/// passes: `null` (no name given) is not an `optional string`.
fn check_name(name: Option<&str>) -> Result<(), MethodError> {
    match name {
        Some(_) => Ok(()),
        None => Err(storage::core_error(
            "Parameter 'name' has invalid type. Expected type 'optional string', got type \
             'null'.",
        )),
    }
}

impl FileAllocate {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        storage::require_writes(request)?;
        let database = &request.context.database;
        let viewer = author(request)?;
        let hash = optional_string(
            request,
            "contentHash",
            "content hashes which are not strings",
        )?;
        let name = optional_string(request, "name", "file names which are not strings")?;
        let view_policy =
            optional_string(request, "viewPolicy", "view policies which are not strings")?;
        let length = match request.value("contentLength") {
            Some(Value::Number(number)) => number.as_i64(),
            value @ Some(Value::String(_)) => storage::offset(value),
            _ => None,
        }
        .ok_or(MethodError::Unsupported("lengths which are not integers"))?;
        let ttl = match request.value("deleteAfterEpoch") {
            Some(value) if php::is_truthy(value) => Some(
                value
                    .as_i64()
                    .filter(|_| value.is_i64() || value.is_u64())
                    .ok_or(MethodError::Unsupported("TTLs which are not integers"))?,
            ),
            _ => None,
        };
        let properties = Properties {
            name,
            author: Some(viewer),
            ttl,
            view_policy,
            explicit_upload: true,
            ..Properties::default()
        };

        let mut found = None;
        let mut chunked_hash = None;
        if let Some(hash) = hash {
            if let Some(copy) = storage::copy_of_hash(request, hash, &properties).await? {
                check_name(name)?;
                let mut transaction = database.pool().begin().await?;
                let phid = copy.write(&mut transaction, database, request.now).await?;
                transaction.commit().await?;
                return Ok(json!({"upload": false, "filePHID": phid.as_str()}));
            }
            let chunked = storage::chunked_hash(request, hash).await?;
            let candidates =
                crate::load_where(database, "contentHash", vec![chunked.clone()]).await?;
            found = one(request, candidates).await?;
            chunked_hash = Some(chunked);
        }

        let engines = Engines::from_config(&request.context.config)?;
        let resumable = name.is_some_and(|name| !name.is_empty())
            && !hash.is_some_and(php::is_truthy_str)
            && found.is_none();
        // `$length > null` compares booleans.
        let large = match engines.chunk_threshold() {
            Some(threshold) => length > threshold,
            None => length != 0,
        };
        if resumable && large {
            found = partial_file(request, viewer, name, length).await?;
        }

        if let Some(file) = found {
            return Ok(json!({"upload": file.is_partial, "filePHID": file.phid.as_str()}));
        }
        if !engines.for_length(length).is_empty() {
            return Ok(json!({"upload": true, "filePHID": null}));
        }
        if engines.chunks_writable() {
            check_name(name)?;
            let prepared =
                storage::allocate_chunks(request, length, &properties, chunked_hash.as_deref())?;
            let mut transaction = database.pool().begin().await?;
            let phid = prepared
                .write(&mut transaction, database, request.now)
                .await?;
            transaction.commit().await?;
            return Ok(json!({"upload": true, "filePHID": phid.as_str()}));
        }
        let error = if engines.any_writable() {
            "Unable to upload file: this file is too large for any configured storage engine."
        } else {
            "Unable to upload file: the server is not configured with any writable storage \
             engines."
        };
        Ok(json!({"upload": false, "filePHID": null, "error": error}))
    }
}

impl Method for FileAllocate {
    fn name(&self) -> &'static str {
        "file.allocate"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "name",
            "contentLength",
            "contentHash",
            "viewPolicy",
            "deleteAfterEpoch",
        ]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}
