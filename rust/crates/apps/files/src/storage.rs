//! Storing file data and reading it back: `PhabricatorFile::newFromFileData()`,
//! `PhabricatorChunkedFileStorageEngine::allocateChunks()` and
//! `loadFileData()`, for the MySQL blob engine (`blob`) and the chunk engine
//! on top of it (`chunks`), with raw data.
//!
//! The local disk and S3 engines, encrypted storage (a default key in
//! `keyring`) and files PHP would measure as images are left to PHP, which
//! the checks here decide before anything is written.

use phab_conduit::{ApiRequest, MethodError};
use phab_core::{Config, Phid, PhidType, hash, php, random};
use phab_db::sqlx::MySqlConnection;
use phab_db::{Database, rows, worker};
use phab_policy::{ApplicationPolicies, Capability, POLICY_NOONE};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// `PhabricatorMySQLFileStorageEngine`.
pub const ENGINE_BLOB: &str = "blob";
/// `PhabricatorChunkedFileStorageEngine`.
pub const ENGINE_CHUNKS: &str = "chunks";
/// The other engines' identifiers.
const OTHER_ENGINES: &[&str] = &["local-disk", "amazon-s3", "unit-test"];
/// `PhabricatorFileRawStorageFormat::FORMATKEY`.
pub const FORMAT_RAW: &str = "raw";
/// `PhabricatorChunkedFileStorageEngine::getChunkSize()`.
pub const CHUNK_SIZE: i64 = 4 * 1024 * 1024;
/// `PhabricatorFileStorageEngine::getFilesizeLimit()`, which the local disk
/// and S3 engines keep.
const DEFAULT_LIMIT: i64 = 8 * 1024 * 1024;
/// `PhabricatorFileStorageEngine::HMAC_INTEGRITY`.
const INTEGRITY_KEY: &str = "file.integrity";
/// `FilesDefaultViewCapability::CAPABILITY`.
const DEFAULT_VIEW: Capability = Capability("files.default.view");
/// `PhabricatorWorker::PRIORITY_INDEX`.
const PRIORITY_INDEX: i64 = 3500;
/// `PhabricatorFilesApplication`.
const APPLICATION_CLASS: &str = "PhabricatorFilesApplication";

/// `ERR-CONDUIT-CORE`: the message of an exception.
pub fn core_error(message: impl Into<String>) -> MethodError {
    MethodError::Conduit {
        code: "ERR-CONDUIT-CORE".to_owned(),
        info: Some(message.into()),
    }
}

/// Refuses to write in a read-only call.
pub fn require_writes(request: &ApiRequest<'_>) -> Result<(), MethodError> {
    if request.writes == phab_auth::Writes::Allowed {
        Ok(())
    } else {
        Err(MethodError::Unsupported(
            "file writes without database writes",
        ))
    }
}

/// A writable engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// `blob`, the only one ported.
    MySql,
    /// `local-disk`.
    LocalDisk,
    /// `amazon-s3`.
    S3,
}

/// The production engines which can write files, from the configuration.
#[derive(Debug, Clone, Copy)]
pub struct Engines {
    mysql_limit: i64,
    local_disk: bool,
    s3: bool,
}

impl Engines {
    /// Reads `storage.mysql-engine.max-size`, `storage.local-disk.path`
    /// and the S3 options.
    pub fn from_config(config: &Config) -> Result<Self, MethodError> {
        let mysql_limit = config
            .value("storage.mysql-engine.max-size")
            .and_then(Value::as_i64)
            .ok_or(MethodError::Unsupported(
                "a MySQL engine size limit which is not an integer",
            ))?;
        let nonempty = |key| {
            config
                .value(key)
                .is_some_and(|value| !php::string_cast(value).is_empty())
        };
        Ok(Self {
            mysql_limit,
            local_disk: nonempty("storage.local-disk.path"),
            s3: [
                "storage.s3.bucket",
                "amazon-s3.access-key",
                "amazon-s3.secret-key",
                "amazon-s3.endpoint",
                "amazon-s3.region",
            ]
            .into_iter()
            .all(|key| {
                config
                    .value(key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            }),
        })
    }

    /// `loadWritableEngines()`, by priority, with their size limits.
    fn writable(self) -> Vec<(Engine, i64)> {
        let mut engines = Vec::new();
        if self.mysql_limit > 0 {
            engines.push((Engine::MySql, self.mysql_limit));
        }
        if self.local_disk {
            engines.push((Engine::LocalDisk, DEFAULT_LIMIT));
        }
        if self.s3 {
            engines.push((Engine::S3, DEFAULT_LIMIT));
        }
        engines
    }

    /// Whether any engine can write files.
    pub fn any_writable(self) -> bool {
        !self.writable().is_empty()
    }

    /// `loadStorageEngines()`: the engines which take `length` bytes.
    pub fn for_length(self, length: i64) -> Vec<Engine> {
        self.writable()
            .into_iter()
            .filter(|(_, limit)| *limit >= length)
            .map(|(engine, _)| engine)
            .collect()
    }

    /// Whether the chunk engine can write: some engine takes a chunk.
    pub fn chunks_writable(self) -> bool {
        self.writable()
            .iter()
            .any(|(_, limit)| *limit >= CHUNK_SIZE)
    }

    /// `getChunkThreshold()`.
    pub fn chunk_threshold(self) -> Option<i64> {
        self.chunks_writable().then_some(CHUNK_SIZE)
    }
}

/// The parameters of a new file (`readPropertiesFromParameters()`).
#[derive(Debug, Clone, Default)]
pub struct Properties<'a> {
    /// `name`.
    pub name: Option<&'a str>,
    /// `authorPHID`.
    pub author: Option<&'a str>,
    /// `ttl.absolute`.
    pub ttl: Option<i64>,
    /// `viewPolicy`, used when truthy.
    pub view_policy: Option<&'a str>,
    /// `isExplicitUpload`.
    pub explicit_upload: bool,
    /// `canCDN`.
    pub can_cdn: bool,
    /// `mime-type`, used when truthy.
    pub mime_type: Option<&'a str>,
    /// `chunk`: a chunk of a chunked file, which is not indexed.
    pub chunk: bool,
}

impl Properties<'_> {
    /// The values MySQL would refuse, checked before anything is written.
    fn check(&self) -> Result<(), MethodError> {
        if self.name.is_some_and(|name| name.chars().count() > 255) {
            return Err(MethodError::Unsupported(
                "file names longer than the column",
            ));
        }
        if self.view_policy.is_some_and(|policy| policy.len() > 64) {
            return Err(MethodError::Unsupported(
                "view policies longer than the column",
            ));
        }
        if self
            .mime_type
            .is_some_and(|mime| mime.chars().count() > 255)
        {
            return Err(MethodError::Unsupported(
                "MIME types longer than the column",
            ));
        }
        if self.author.is_some_and(|author| author.len() > 64) {
            return Err(MethodError::Unsupported(
                "author PHIDs longer than the column",
            ));
        }
        Ok(())
    }

    /// `readPropertiesFromParameters()` on `row`.
    fn apply(&self, row: &mut FileRow, now: i64) -> Result<(), MethodError> {
        self.check()?;
        row.name = self.name.map(str::to_owned);
        row.author = self.author.map(str::to_owned);
        if let Some(ttl) = self.ttl {
            if ttl < now {
                return Err(core_error(format!(
                    "Absolute TTL must be in the present or future, but TTL \"{ttl}\" is in the \
                     past."
                )));
            }
            if u32::try_from(ttl).is_err() {
                return Err(MethodError::Unsupported("TTLs beyond the column"));
            }
            row.ttl = Some(ttl);
        }
        if let Some(policy) = self.view_policy.filter(|policy| php::is_truthy_str(policy)) {
            policy.clone_into(&mut row.view_policy);
        }
        row.explicit_upload = self.explicit_upload;
        if self.can_cdn {
            row.metadata.insert("canCDN".to_owned(), json!(1));
        }
        if let Some(mime) = self.mime_type.filter(|mime| php::is_truthy_str(mime)) {
            row.mime_type = Some(mime.to_owned());
        }
        if self.chunk {
            row.metadata.insert("chunk".to_owned(), json!(true));
        }
        Ok(())
    }
}

/// A `file` row to insert.
#[derive(Debug, Clone)]
struct FileRow {
    name: Option<String>,
    mime_type: Option<String>,
    byte_size: u64,
    storage_engine: String,
    storage_format: String,
    storage_handle: String,
    author: Option<String>,
    content_hash: String,
    metadata: Map<String, Value>,
    ttl: Option<i64>,
    explicit_upload: bool,
    view_policy: String,
    is_partial: bool,
    is_chunk: bool,
}

impl FileRow {
    /// `initializeNewFile()`: the default view policy of Files.
    fn new(config: &Config) -> Self {
        let view_policy = ApplicationPolicies::from_config(config)
            .policy(APPLICATION_CLASS, DEFAULT_VIEW, config.allow_public())
            .to_owned();
        Self {
            name: None,
            mime_type: None,
            byte_size: 0,
            storage_engine: String::new(),
            storage_format: String::new(),
            storage_handle: String::new(),
            author: None,
            content_hash: String::new(),
            metadata: Map::new(),
            ttl: None,
            explicit_upload: true,
            view_policy,
            is_partial: false,
            is_chunk: false,
        }
    }

    /// `saveAndIndex()`: `save()` generates the keys and the PHID, and
    /// files other than chunks are queued for indexing.
    async fn insert(
        &self,
        connection: &mut MySqlConnection,
        database: &Database,
        now: i64,
    ) -> Result<Phid, MethodError> {
        let phid = Phid::generate(PhidType::FILE, None);
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (phid, name, mimeType, byteSize, storageEngine, storageFormat, \
             storageHandle, dateCreated, dateModified, authorPHID, secretKey, contentHash, \
             metadata, ttl, isExplicitUpload, mailKey, viewPolicy, isPartial, builtinKey, \
             isDeleted) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 0)",
            database.table("file", "file")
        ))
        .bind(phid.as_str())
        .bind(self.name.as_deref())
        .bind(self.mime_type.as_deref())
        .bind(self.byte_size)
        .bind(&self.storage_engine)
        .bind(&self.storage_format)
        .bind(&self.storage_handle)
        .bind(now)
        .bind(now)
        .bind(self.author.as_deref())
        .bind(random::random_characters(20))
        .bind(&self.content_hash)
        .bind(encode_metadata(&self.metadata))
        .bind(self.ttl)
        .bind(i64::from(self.explicit_upload))
        .bind(random::random_characters(20))
        .bind(&self.view_policy)
        .bind(i64::from(self.is_partial))
        .execute(&mut *connection)
        .await?;
        if !self.is_chunk {
            // `PhabricatorSearchWorker::queueDocumentForIndexing()`.
            let data = php::json_encode(&json!({
                "documentPHID": phid.as_str(),
                "parameters": [],
                "strict": false,
            }));
            let task = worker::Task {
                class: "PhabricatorSearchWorker",
                data: &data,
                priority: PRIORITY_INDEX,
                object_phid: Some(phid.as_str()),
            };
            worker::schedule_task(connection, database, task, now).await?;
        }
        Ok(phid)
    }
}

/// `phutil_json_encode()` of a metadata dictionary: an empty one is `[]`.
fn encode_metadata(metadata: &Map<String, Value>) -> String {
    php::json_encode(&php::round_trip(&Value::Object(metadata.clone())))
}

/// `PhabricatorFile::hashFileContent()`.
pub fn content_hash(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// The key of `PhabricatorHash::digestWithNamedKey()`, if PHP has made it
/// (PHP makes it the first time, which Rust leaves to PHP).
async fn named_key(database: &Database, name: &str) -> Result<Vec<u8>, MethodError> {
    let sql = format!(
        "SELECT keyValue FROM {} WHERE keyName = ?",
        database.table("auth", "auth_hmackey")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(name)
        .fetch_optional(database.pool())
        .await?;
    row.map(|row| rows::text(&row, "keyValue"))
        .transpose()?
        .and_then(|value| hex::decode(value).ok())
        .filter(|key| !key.is_empty())
        .ok_or(MethodError::Unsupported("an HMAC key PHP has not made yet"))
}

/// `newIntegrityHash()` of raw data: the format adds nothing after the
/// slash.
fn integrity_hash(data: &[u8], key: &[u8]) -> Result<String, MethodError> {
    let data_hash = hash::hmac_sha256(data, key).map_err(MethodError::internal)?;
    hash::hmac_sha256(format!("{data_hash}/").as_bytes(), key).map_err(MethodError::internal)
}

/// Whether `keyring` has a default key: new files are then encrypted.
fn has_default_key(config: &Config) -> bool {
    let keys: Vec<&Value> = match config.value("keyring") {
        Some(Value::Array(keys)) => keys.iter().collect(),
        Some(Value::Object(keys)) => keys.values().collect(),
        _ => Vec::new(),
    };
    keys.iter()
        .any(|key| key.get("default").is_some_and(php::is_truthy))
}

/// A file ready to be written: a copy of stored data, or new data with
/// its blob.
#[derive(Debug, Clone)]
pub struct Prepared {
    row: FileRow,
    blob: Option<Vec<u8>>,
}

impl Prepared {
    /// The MIME type the file gets.
    pub fn mime_type(&self) -> Option<&str> {
        self.row.mime_type.as_deref()
    }

    /// Writes the file. Returns its PHID.
    pub async fn write(
        mut self,
        connection: &mut MySqlConnection,
        database: &Database,
        now: i64,
    ) -> Result<Phid, MethodError> {
        if let Some(blob) = self.blob.take() {
            // `PhabricatorMySQLFileStorageEngine::writeFile()`.
            let id = phab_db::sqlx::query(&format!(
                "INSERT INTO {} (data, dateCreated, dateModified) VALUES (?, ?, ?)",
                database.table("file", "file_storageblob")
            ))
            .bind(blob)
            .bind(now)
            .bind(now)
            .execute(&mut *connection)
            .await?
            .last_insert_id();
            self.row.storage_handle = id.to_string();
        }
        self.row.insert(connection, database, now).await
    }
}

/// `newFileFromContentHash()`: a new file sharing the storage of a file
/// with this content hash, if there is one.
pub async fn copy_of_hash(
    request: &ApiRequest<'_>,
    hash: &str,
    properties: &Properties<'_>,
) -> Result<Option<Prepared>, MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT mimeType, byteSize, storageEngine, storageFormat, storageHandle, metadata \
         FROM {} WHERE contentHash = ? LIMIT 1",
        database.table("file", "file")
    );
    let Some(existing) = phab_db::sqlx::query(&sql)
        .bind(hash)
        .fetch_optional(database.pool())
        .await?
    else {
        return Ok(None);
    };
    if hash.len() > 64 {
        return Err(MethodError::Unsupported(
            "content hashes longer than the column",
        ));
    }
    let metadata = match php::round_trip(&rows::json(&existing, "metadata")?) {
        Value::Object(metadata) => metadata,
        _ => Map::new(),
    };
    let mut row = FileRow::new(&request.context.config);
    row.byte_size = rows::uint(&existing, "byteSize")?;
    hash.clone_into(&mut row.content_hash);
    row.storage_engine = rows::text(&existing, "storageEngine")?;
    row.storage_handle = rows::text(&existing, "storageHandle")?;
    row.storage_format = rows::text(&existing, "storageFormat")?;
    row.metadata.insert(
        "storage".to_owned(),
        metadata.get("storage").cloned().unwrap_or(json!([])),
    );
    row.mime_type = rows::opt_text(&existing, "mimeType")?;
    // `copyDimensions()`.
    for key in ["width", "height"] {
        if let Some(value) = metadata.get(key).filter(|value| php::is_truthy(value)) {
            row.metadata.insert(key.to_owned(), value.clone());
        }
    }
    properties.apply(&mut row, request.now)?;
    row.is_chunk = properties.chunk;
    Ok(Some(Prepared { row, blob: None }))
}

/// `newFromFileData()`: a copy of a file with the same content, or the
/// data stored in the MySQL engine. `mime` detects the type when the
/// properties don't give one.
pub async fn from_data(
    request: &ApiRequest<'_>,
    data: &[u8],
    properties: &Properties<'_>,
    mime: Option<&crate::mime::MimeCommand>,
) -> Result<Prepared, MethodError> {
    let hash = content_hash(data);
    if let Some(copy) = copy_of_hash(request, &hash, properties).await? {
        return Ok(copy);
    }
    let config = &request.context.config;
    let length = i64::try_from(data.len()).map_err(MethodError::internal)?;
    match Engines::from_config(config)?.for_length(length).first() {
        None => {
            return Err(core_error(
                "No configured storage engine can store this file. See \"Configuring File \
                 Storage\" in the documentation for information on configuring storage engines.",
            ));
        }
        Some(Engine::MySql) => {}
        Some(_) => return Err(MethodError::Unsupported("storage engines other than MySQL")),
    }
    if has_default_key(config) {
        return Err(MethodError::Unsupported("encrypted file storage"));
    }
    let key = named_key(&request.context.database, INTEGRITY_KEY).await?;
    let mut row = FileRow::new(config);
    FORMAT_RAW.clone_into(&mut row.storage_format);
    row.metadata.insert("storage".to_owned(), json!([]));
    row.metadata
        .insert("integrity".to_owned(), json!(integrity_hash(data, &key)?));
    row.byte_size = data.len() as u64;
    row.content_hash = hash;
    ENGINE_BLOB.clone_into(&mut row.storage_engine);
    properties.apply(&mut row, request.now)?;
    row.is_chunk = properties.chunk;
    if row.mime_type.as_deref().is_none_or(str::is_empty) {
        let mime = mime.ok_or(MethodError::Unsupported(
            "detecting MIME types without PHAB_MIME_COMMAND",
        ))?;
        let detected = mime.detect(data).ok_or(MethodError::Unsupported(
            "a MIME type the command can't detect",
        ))?;
        row.mime_type = Some(detected);
    }
    // `updateDimensions()` measures images with GD.
    if row
        .mime_type
        .as_deref()
        .is_some_and(|mime| crate::is_viewable_image_type(config, mime))
    {
        return Err(MethodError::Unsupported("measuring uploaded images"));
    }
    Ok(Prepared {
        row,
        blob: Some(data.to_vec()),
    })
}

/// `getChunkedHash()`: the content hash of a chunked file, which differs
/// for each user.
pub async fn chunked_hash(request: &ApiRequest<'_>, hash: &str) -> Result<String, MethodError> {
    let phid = request
        .viewer
        .phid
        .as_ref()
        .ok_or(MethodError::Unsupported("chunked hashes without a user"))?;
    let database = &request.context.database;
    let sql = format!(
        "SELECT accountSecret FROM {} WHERE phid = ?",
        database.table("user", "user")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(phid.as_str())
        .fetch_optional(database.pool())
        .await?
        .ok_or(MethodError::Unsupported("chunked hashes of unknown users"))?;
    // `binary(64)`: a shorter secret keeps its padding, as in PHP.
    let secret = rows::opt_text(&row, "accountSecret")?.unwrap_or_default();
    hash_for_input(
        &request.context.config,
        format!("{secret}:{hash}:{}", phid.as_str()).as_bytes(),
    )
}

/// `getChunkedHashForInput()`.
fn hash_for_input(config: &Config, input: &[u8]) -> Result<String, MethodError> {
    let digest =
        hash::weak_digest(input, config.hmac_key()?.as_bytes()).map_err(MethodError::internal)?;
    Ok(format!("{}-C", &digest[..digest.len() - 2]))
}

/// A chunked file ready to be written, with its chunks.
#[derive(Debug, Clone)]
pub struct PreparedChunks {
    row: FileRow,
    length: i64,
}

impl PreparedChunks {
    /// Writes the chunks, then the file. Returns its PHID.
    pub async fn write(
        self,
        connection: &mut MySqlConnection,
        database: &Database,
        now: i64,
    ) -> Result<Phid, MethodError> {
        let table = database.table("file", "file_chunk");
        let mut start = 0;
        while start < self.length {
            let end = (start + CHUNK_SIZE).min(self.length);
            phab_db::sqlx::query(&format!(
                "INSERT INTO {table} (chunkHandle, byteStart, byteEnd, dataFilePHID) VALUES \
                 (?, ?, ?, NULL)"
            ))
            .bind(&self.row.storage_handle)
            .bind(start)
            .bind(end)
            .execute(&mut *connection)
            .await?;
            start = end;
        }
        self.row.insert(connection, database, now).await
    }
}

/// `allocateChunks()` (with `newChunkedFile()`): a partial file of
/// `length` bytes with empty chunks.
pub fn allocate_chunks(
    request: &ApiRequest<'_>,
    length: i64,
    properties: &Properties<'_>,
    chunked_hash: Option<&str>,
) -> Result<PreparedChunks, MethodError> {
    let config = &request.context.config;
    let mut row = FileRow::new(config);
    row.byte_size =
        u64::try_from(length).map_err(|_| MethodError::Unsupported("negative chunked lengths"))?;
    if properties.mime_type.is_none_or(str::is_empty) {
        row.mime_type = Some("application/octet-stream".to_owned());
    }
    row.content_hash = match chunked_hash.filter(|hash| php::is_truthy_str(hash)) {
        Some(hash) => hash.to_owned(),
        // A random seed, so the file matches no other.
        None => hash_for_input(config, random::random_characters(64).as_bytes())?,
    };
    if row.content_hash.len() > 64 {
        return Err(MethodError::Unsupported(
            "content hashes longer than the column",
        ));
    }
    ENGINE_CHUNKS.clone_into(&mut row.storage_engine);
    // `PhabricatorFileChunk::newChunkHandle()`.
    row.storage_handle = hash::digest_for_index(random::random_characters(64).as_bytes());
    FORMAT_RAW.clone_into(&mut row.storage_format);
    row.is_partial = true;
    properties.apply(&mut row, request.now)?;
    row.is_chunk = properties.chunk;
    Ok(PreparedChunks { row, length })
}

/// Where a file's data is.
#[derive(Debug, Clone)]
pub struct Storage {
    /// The row ID.
    pub id: i64,
    /// `storageEngine`.
    pub engine: String,
    /// `storageFormat`.
    pub format: String,
    /// `storageHandle`.
    pub handle: String,
    /// The decoded metadata.
    pub metadata: Value,
}

impl Storage {
    /// The storage of the file with this PHID.
    pub async fn load(database: &Database, phid: &str) -> Result<Option<Self>, MethodError> {
        let sql = format!(
            "SELECT id, storageEngine, storageFormat, storageHandle, metadata FROM {} \
             WHERE phid = ?",
            database.table("file", "file")
        );
        let Some(row) = phab_db::sqlx::query(&sql)
            .bind(phid)
            .fetch_optional(database.pool())
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(Self {
            id: rows::int(&row, "id")?,
            engine: rows::text(&row, "storageEngine")?,
            format: rows::text(&row, "storageFormat")?,
            handle: rows::text(&row, "storageHandle")?,
            metadata: rows::json(&row, "metadata")
                .map_err(|_| MethodError::Unsupported("file metadata which is not JSON"))?,
        }))
    }

    /// The chunk engine's check of `newChunkQuery()`: an engine which
    /// exists and holds chunks.
    pub fn require_chunks(&self, phid: &str) -> Result<(), MethodError> {
        if self.engine == ENGINE_CHUNKS {
            return Ok(());
        }
        if self.engine == ENGINE_BLOB || OTHER_ENGINES.contains(&self.engine.as_str()) {
            return Err(core_error(format!("File \"{phid}\" does not have chunks!")));
        }
        Err(MethodError::Unsupported("unknown storage engines"))
    }

    /// `loadFileData()`.
    pub async fn load_data(&self, database: &Database) -> Result<Vec<u8>, MethodError> {
        match self.engine.as_str() {
            ENGINE_BLOB => self.load_blob(database).await,
            ENGINE_CHUNKS => {
                let sql = format!(
                    "SELECT byteStart, dataFilePHID FROM {} WHERE chunkHandle = ? \
                     ORDER BY byteStart, id DESC",
                    database.table("file", "file_chunk")
                );
                let chunks = phab_db::sqlx::query(&sql)
                    .bind(&self.handle)
                    .fetch_all(database.pool())
                    .await?;
                let mut data = Vec::new();
                for chunk in chunks {
                    let phid = rows::opt_text(&chunk, "dataFilePHID")?
                        .filter(|phid| php::is_truthy_str(phid))
                        .ok_or(MethodError::Unsupported("incomplete chunked files"))?;
                    let storage = Self::load(database, &phid)
                        .await?
                        .ok_or(MethodError::Unsupported("chunks without their data"))?;
                    if storage.engine != ENGINE_BLOB {
                        return Err(MethodError::Unsupported("chunks outside MySQL"));
                    }
                    data.extend(storage.load_blob(database).await?);
                }
                Ok(data)
            }
            _ => Err(MethodError::Unsupported("storage engines other than MySQL")),
        }
    }

    /// The MySQL engine's `getRawFileDataIterator()`: the blob, checked
    /// against the integrity hash.
    async fn load_blob(&self, database: &Database) -> Result<Vec<u8>, MethodError> {
        if self.format != FORMAT_RAW {
            return Err(MethodError::Unsupported("encrypted file storage"));
        }
        let id: u64 = self
            .handle
            .parse()
            .map_err(|_| MethodError::Unsupported("blob handles which are not IDs"))?;
        let sql = format!(
            "SELECT data FROM {} WHERE id = ?",
            database.table("file", "file_storageblob")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(id)
            .fetch_optional(database.pool())
            .await?
            .ok_or(MethodError::Unsupported("missing blobs"))?;
        let data: Vec<u8> = phab_db::sqlx::Row::try_get(&row, "data")?;
        match self.metadata.get("integrity") {
            None | Some(Value::Null) => {}
            Some(Value::String(known)) => {
                let key = named_key(database, INTEGRITY_KEY).await?;
                let actual = integrity_hash(&data, &key)?;
                if !hash::hashes_are_identical(known.as_bytes(), actual.as_bytes()) {
                    return Err(MethodError::Unsupported(
                        "files failing their integrity check",
                    ));
                }
            }
            Some(_) => return Err(MethodError::Unsupported("unusual integrity hashes")),
        }
        Ok(data)
    }
}

/// `$file->save()` of a loaded file, as `file.uploadchunk` changes it:
/// Lisk writes every column back, so the metadata is encoded again.
pub async fn update_file(
    connection: &mut MySqlConnection,
    database: &Database,
    storage: &Storage,
    (is_partial, mime_type): (Option<bool>, Option<Option<&str>>),
    now: i64,
) -> Result<(), MethodError> {
    let mut sets = vec!["metadata = ?", "dateModified = ?"];
    if is_partial.is_some() {
        sets.push("isPartial = ?");
    }
    if mime_type.is_some() {
        sets.push("mimeType = ?");
    }
    let sql = format!(
        "UPDATE {} SET {} WHERE id = ?",
        database.table("file", "file"),
        sets.join(", ")
    );
    let metadata = match php::round_trip(&storage.metadata) {
        Value::Null => return Err(MethodError::Unsupported("files without metadata")),
        metadata => php::json_encode(&metadata),
    };
    let mut query = phab_db::sqlx::query(&sql).bind(metadata).bind(now);
    if let Some(partial) = is_partial {
        query = query.bind(i64::from(partial));
    }
    if let Some(mime) = mime_type {
        query = query.bind(mime);
    }
    query.bind(storage.id).execute(&mut *connection).await?;
    Ok(())
}

/// The properties of a chunk's data (`file.uploadchunk`).
pub fn chunk_properties<'a>(name: &'a str, mime_type: Option<&'a str>) -> Properties<'a> {
    Properties {
        name: Some(name),
        view_policy: Some(POLICY_NOONE),
        chunk: true,
        mime_type,
        ..Properties::default()
    }
}

/// PHP's strict `base64_decode()`: whitespace is skipped, other characters
/// outside the alphabet, data after padding, a truncated last group and
/// wrong padding fail. Trailing bits are dropped.
pub fn decode_base64(text: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(text.len() / 4 * 3);
    let mut count = 0usize;
    let mut padding = 0usize;
    let mut pending = 0u8;
    for &byte in text {
        if byte == b'=' {
            padding += 1;
            continue;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'\t' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        if padding > 0 {
            return None;
        }
        match count % 4 {
            0 => pending = value << 2,
            1 => {
                output.push(pending | (value >> 4));
                pending = (value & 0x0f) << 4;
            }
            2 => {
                output.push(pending | (value >> 2));
                pending = (value & 0x03) << 6;
            }
            _ => output.push(pending | value),
        }
        count += 1;
    }
    if count % 4 == 1 {
        return None;
    }
    if padding > 0 && (padding > 2 || (count + padding) % 4 != 0) {
        return None;
    }
    Some(output)
}

/// A byte offset, as PHP would add it: an integer or its digits.
pub fn offset(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64().filter(|offset| *offset >= 0),
        Value::String(text)
            if !text.is_empty()
                && text.bytes().all(|byte| byte.is_ascii_digit())
                && (text == "0" || !text.starts_with('0')) =>
        {
            text.parse().ok()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    use super::*;

    #[test]
    fn decodes_base64_like_php() {
        assert_eq!(decode_base64(b"aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(decode_base64(b"aGVs\nbG8"), Some(b"hello".to_vec()));
        assert_eq!(decode_base64(b"QQ"), Some(b"A".to_vec()));
        assert_eq!(decode_base64(b"  "), Some(Vec::new()));
        assert_eq!(decode_base64(b"Q"), None);
        assert_eq!(decode_base64(b"QQ=a"), None);
        assert_eq!(decode_base64(b"QQ==="), None);
        assert_eq!(decode_base64(b"QUI=="), None);
        assert_eq!(decode_base64(b"a-b"), None);
        assert_eq!(decode_base64(b"\x0bQQ"), None);
        let bytes: Vec<u8> = (0..=255).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert_eq!(decode_base64(encoded.as_bytes()), Some(bytes));
    }

    #[test]
    fn reads_offsets_like_php() {
        assert_eq!(offset(Some(&json!(0))), Some(0));
        assert_eq!(offset(Some(&json!("4194304"))), Some(4_194_304));
        assert_eq!(offset(Some(&json!("07"))), None);
        assert_eq!(offset(Some(&json!(-1))), None);
        assert_eq!(offset(Some(&json!(1.5))), None);
        assert_eq!(offset(None), None);
    }

    #[test]
    fn picks_engines_like_php() {
        let engines = Engines {
            mysql_limit: 8_388_608,
            local_disk: false,
            s3: false,
        };
        assert_eq!(engines.for_length(8_388_608), [Engine::MySql]);
        assert_eq!(engines.for_length(8_388_609), []);
        assert_eq!(engines.chunk_threshold(), Some(CHUNK_SIZE));
        let small = Engines {
            mysql_limit: 1_000_000,
            ..engines
        };
        assert!(!small.chunks_writable());
        assert!(small.any_writable());
        let disk = Engines {
            local_disk: true,
            ..small
        };
        assert_eq!(disk.for_length(2_000_000), [Engine::LocalDisk]);
        assert!(disk.chunks_writable());
    }

    #[test]
    fn hashes_chunked_files_like_php() {
        let mut config = Config::default();
        if let Value::Object(map) = json!({"security.hmac-key": "key"}) {
            config.push_layer("test", map);
        }
        // substr(hash_hmac('sha1', 's:abc:PHID-USER-x', 'key'), 0, -2).'-C'
        let expected = hash::weak_digest(b"s:abc:PHID-USER-x", b"key").unwrap();
        let hash = hash_for_input(&config, b"s:abc:PHID-USER-x").unwrap();
        assert_eq!(hash.len(), 40);
        assert_eq!(hash, format!("{}-C", &expected[..38]));
    }

    #[test]
    fn hashes_content_like_php() {
        // hash('sha256', 'hello')
        assert_eq!(
            content_hash(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
