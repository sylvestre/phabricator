//! `file.uploadchunk` (`FileUploadChunkConduitAPIMethod`) and
//! `file.querychunks` (`FileQueryChunksConduitAPIMethod`): the chunks of a
//! file in the chunk engine.
//!
//! Ported: chunks stored in MySQL, as raw or base64 data. Left to PHP:
//! files the viewer can't see, offsets which are not integers, parameters
//! of other types, and what `file.upload` leaves to PHP for the chunk's
//! data (the first chunk needs its MIME type detected).

use std::sync::Arc;

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use phab_db::rows;
use serde_json::{Value, json};

use crate::File;
use crate::mime::MimeCommand;
use crate::search::{self, APPLICATION};
use crate::storage::{self, Storage};
use crate::upload::decode;

/// `loadFileByPHID()`: the file and its storage, whose engine
/// `newChunkQuery()` checks later.
async fn load_file(request: &ApiRequest<'_>) -> Result<(File, Storage), MethodError> {
    let phid = match request.value("filePHID") {
        Some(Value::String(phid)) => phid.as_str(),
        _ => return Err(MethodError::Unsupported("file PHIDs which are not strings")),
    };
    let database = &request.context.database;
    let candidates = crate::load_by_phids(database, &[Phid::new(phid)]).await?;
    let exists = !candidates.is_empty();
    let Some(file) = search::visible_files(request, candidates)
        .await?
        .into_iter()
        .next()
    else {
        if exists {
            return Err(MethodError::Unsupported("a file the viewer can't see"));
        }
        return Err(storage::core_error(format!("No such file \"{phid}\"!")));
    };
    let storage = Storage::load(database, file.phid.as_str())
        .await?
        .ok_or(MethodError::Unsupported("a file deleted meanwhile"))?;
    Ok((file, storage))
}

/// A `file_chunk` row.
struct Chunk {
    id: i64,
    start: u64,
    end: u64,
    data_file: Option<String>,
}

impl Chunk {
    fn from_row(row: &phab_db::sqlx::mysql::MySqlRow) -> Result<Self, MethodError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            start: rows::uint(row, "byteStart")?,
            end: rows::uint(row, "byteEnd")?,
            data_file: rows::opt_text(row, "dataFilePHID")?,
        })
    }

    /// `(bool)$chunk->getDataFilePHID()`.
    fn is_complete(&self) -> bool {
        self.data_file.as_deref().is_some_and(php::is_truthy_str)
    }
}

/// `PhabricatorFileChunkQuery` of the file's chunks, newest first, within
/// a byte range if one is given. Chunks are visible to every user.
async fn load_chunks(
    request: &ApiRequest<'_>,
    storage: &Storage,
    range: Option<(i64, i64)>,
) -> Result<Vec<Chunk>, MethodError> {
    let database = &request.context.database;
    let mut sql = format!(
        "SELECT id, byteStart, byteEnd, dataFilePHID FROM {} WHERE chunkHandle IN (?)",
        database.table("file", "file_chunk")
    );
    if range.is_some() {
        sql.push_str(" AND byteEnd > ? AND byteStart < ?");
    }
    sql.push_str(" ORDER BY id DESC");
    let mut query = phab_db::sqlx::query(&sql).bind(&storage.handle);
    if let Some((start, end)) = range {
        query = query.bind(start).bind(end);
    }
    query
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(Chunk::from_row)
        .collect()
}

/// `file.uploadchunk`.
pub struct FileUploadChunk {
    mime: Option<Arc<MimeCommand>>,
}

impl FileUploadChunk {
    /// The method, detecting the MIME types of first chunks with `mime`.
    pub fn new(mime: Option<Arc<MimeCommand>>) -> Self {
        Self { mime }
    }

    async fn run(&self, request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        storage::require_writes(request)?;
        if request.viewer.phid.is_none() {
            return Err(MethodError::Unsupported("chunks without a user"));
        }
        let (file, storage) = load_file(request).await?;
        let start = storage::offset(request.value("byteStart"))
            .ok_or(MethodError::Unsupported("offsets which are not integers"))?;
        let Some(Value::String(data)) = request.value("data") else {
            return Err(MethodError::Unsupported("chunk data which is not a string"));
        };
        // `switch` compares loosely: "" is null.
        let data = match request.value("dataEncoding") {
            Some(Value::String(encoding)) if encoding == "base64" => decode(data)?,
            None | Some(Value::Null) => data.as_bytes().to_vec(),
            Some(Value::String(encoding)) if encoding.is_empty() => data.as_bytes().to_vec(),
            Some(Value::String(_)) => {
                return Err(storage::core_error("Unsupported data encoding."));
            }
            Some(_) => return Err(MethodError::Unsupported("encodings which are not strings")),
        };
        let end = start + i64::try_from(data.len()).map_err(MethodError::internal)?;
        storage.require_chunks(file.phid.as_str())?;

        // `loadFileChunkForUpload()`.
        let chunks = load_chunks(request, &storage, Some((start, end))).await?;
        let chunk = match chunks.as_slice() {
            [] => {
                return Err(storage::core_error(format!(
                    "There are no file data chunks in byte range {start} - {end}."
                )));
            }
            [chunk] => chunk,
            _ => {
                return Err(storage::core_error(format!(
                    "There are multiple chunks in byte range {start} - {end}."
                )));
            }
        };
        if i64::try_from(chunk.start).ok() != Some(start) {
            return Err(storage::core_error(format!(
                "Chunk start byte is {}, not {start}.",
                chunk.start
            )));
        }
        if i64::try_from(chunk.end).ok() != Some(end) {
            return Err(storage::core_error(format!(
                "Chunk end byte is {}, not {end}.",
                chunk.end
            )));
        }
        if chunk.is_complete() {
            return Err(storage::core_error("Chunk has already been uploaded."));
        }

        // The first chunk's type is detected, and becomes the file's.
        let mime_type = (start != 0).then_some("application/octet-stream");
        let name = format!("{}.chunk-{}", file.monogram(), chunk.id);
        let properties = storage::chunk_properties(&name, mime_type);
        let prepared =
            storage::from_data(request, &data, &properties, self.mime.as_deref()).await?;
        let chunk_mime = prepared.mime_type().map(str::to_owned);

        let database = &request.context.database;
        let mut transaction = database.pool().begin().await?;
        let data_phid = prepared
            .write(&mut transaction, database, request.now)
            .await?;
        // Lisk writes the other columns back as they are.
        phab_db::sqlx::query(&format!(
            "UPDATE {} SET dataFilePHID = ? WHERE id = ?",
            database.table("file", "file_chunk")
        ))
        .bind(data_phid.as_str())
        .bind(chunk.id)
        .execute(&mut *transaction)
        .await?;
        // `loadAnyMissingChunk()`, after this chunk is complete.
        let missing = phab_db::sqlx::query(&format!(
            "SELECT id FROM {} WHERE chunkHandle IN (?) AND dataFilePHID IS NULL \
             ORDER BY id DESC LIMIT 1",
            database.table("file", "file_chunk")
        ))
        .bind(&storage.handle)
        .fetch_optional(&mut *transaction)
        .await?;
        let is_partial = missing.is_none().then_some(false);
        let mime_type = (start == 0).then_some(chunk_mime.as_deref());
        if is_partial.is_some() || mime_type.is_some() {
            storage::update_file(
                &mut transaction,
                database,
                &storage,
                (is_partial, mime_type),
                request.now,
            )
            .await?;
        }
        transaction.commit().await?;
        Ok(Value::Null)
    }
}

impl Method for FileUploadChunk {
    fn name(&self) -> &'static str {
        "file.uploadchunk"
    }

    fn params(&self) -> &'static [&'static str] {
        &["filePHID", "byteStart", "data", "dataEncoding"]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(self.run(request))
    }
}

/// `file.querychunks`.
pub struct FileQueryChunks;

impl FileQueryChunks {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        if request.viewer.phid.is_none() {
            return Err(MethodError::Unsupported("chunks without a user"));
        }
        let (file, storage) = load_file(request).await?;
        storage.require_chunks(file.phid.as_str())?;
        let chunks = load_chunks(request, &storage, None).await?;
        // Lisk keeps MySQL's strings.
        Ok(Value::Array(
            chunks
                .iter()
                .map(|chunk| {
                    json!({
                        "byteStart": chunk.start.to_string(),
                        "byteEnd": chunk.end.to_string(),
                        "complete": chunk.is_complete(),
                    })
                })
                .collect(),
        ))
    }
}

impl Method for FileQueryChunks {
    fn name(&self) -> &'static str {
        "file.querychunks"
    }

    fn params(&self) -> &'static [&'static str] {
        &["filePHID"]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}
