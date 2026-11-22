//! `file.upload` (`FileUploadConduitAPIMethod`) and `file.uploadhash`
//! (`FileUploadHashConduitAPIMethod`).
//!
//! Ported: files with the content of a stored file (whatever its engine),
//! and new data stored in MySQL. Left to PHP: other engines, encryption,
//! images (PHP measures them), MIME types without `PHAB_MIME_COMMAND`, and
//! names or policies which are not strings.

use std::sync::Arc;

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use serde_json::{Value, json};

use crate::mime::MimeCommand;
use crate::search::APPLICATION;
use crate::storage::{self, Properties};

/// An optional string parameter; `None` when missing or null.
pub(crate) fn optional_string<'a>(
    request: &'a ApiRequest<'_>,
    key: &str,
    what: &'static str,
) -> Result<Option<&'a str>, MethodError> {
    match request.value(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(MethodError::Unsupported(what)),
    }
}

/// The acting user's PHID.
pub(crate) fn author<'a>(request: &'a ApiRequest<'_>) -> Result<&'a str, MethodError> {
    request
        .viewer
        .phid
        .as_ref()
        .map(phab_core::Phid::as_str)
        .ok_or(MethodError::Unsupported("uploads without a user"))
}

/// `decodeBase64()`.
pub(crate) fn decode(text: &str) -> Result<Vec<u8>, MethodError> {
    storage::decode_base64(text.as_bytes())
        .ok_or_else(|| storage::core_error("Unable to decode base64 data!"))
}

/// `file.upload`.
pub struct FileUpload {
    mime: Option<Arc<MimeCommand>>,
}

impl FileUpload {
    /// The method, detecting MIME types with `mime`.
    pub fn new(mime: Option<Arc<MimeCommand>>) -> Self {
        Self { mime }
    }

    async fn run(&self, request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        storage::require_writes(request)?;
        let name = optional_string(request, "name", "file names which are not strings")?;
        let can_cdn = request
            .value("canCDN")
            .is_some_and(phab_core::php::is_truthy);
        let view_policy =
            optional_string(request, "viewPolicy", "view policies which are not strings")?;
        let data = match request.value("data_base64") {
            None | Some(Value::Null) => "",
            Some(Value::String(text)) => text.as_str(),
            Some(_) => return Err(MethodError::Unsupported("data which is not a string")),
        };
        if data.is_empty() {
            return Err(storage::core_error(
                "Field \"data_base64\" must be non-empty.",
            ));
        }
        let data = decode(data)?;
        let properties = Properties {
            name,
            author: Some(author(request)?),
            view_policy,
            explicit_upload: true,
            can_cdn,
            ..Properties::default()
        };
        let prepared =
            storage::from_data(request, &data, &properties, self.mime.as_deref()).await?;
        let database = &request.context.database;
        let mut transaction = database.pool().begin().await?;
        let phid = prepared
            .write(&mut transaction, database, request.now)
            .await?;
        transaction.commit().await?;
        Ok(json!(phid.as_str()))
    }
}

impl Method for FileUpload {
    fn name(&self) -> &'static str {
        "file.upload"
    }

    fn params(&self) -> &'static [&'static str] {
        &["data_base64", "name", "viewPolicy", "canCDN"]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(self.run(request))
    }
}

/// `file.uploadhash`: obsolete, does nothing.
pub struct FileUploadHash;

impl Method for FileUploadHash {
    fn name(&self) -> &'static str {
        "file.uploadhash"
    }

    fn params(&self) -> &'static [&'static str] {
        &["hash", "name"]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, _request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async { Ok(Value::Null) })
    }
}
