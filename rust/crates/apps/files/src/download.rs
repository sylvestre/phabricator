//! `file.download` (`FileDownloadConduitAPIMethod`): a file's data, in
//! base64.
//!
//! Ported: raw data in MySQL, directly or in chunks. Left to PHP: other
//! engines, encrypted data, incomplete chunked files, data failing its
//! integrity check, and files the viewer can't see.

use base64::Engine as _;
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::Phid;
use serde_json::{Value, json};

use crate::search::{self, APPLICATION};
use crate::storage::Storage;

/// `ERR-BAD-PHID`.
const ERR_BAD_PHID: &str = "ERR-BAD-PHID";

/// `file.download`.
pub struct FileDownload;

impl FileDownload {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let Some(Value::String(phid)) = request.value("phid") else {
            return Err(MethodError::Unsupported("file PHIDs which are not strings"));
        };
        let database = &request.context.database;
        let candidates = crate::load_by_phids(database, &[Phid::new(phid.clone())]).await?;
        let exists = !candidates.is_empty();
        let Some(file) = search::visible_files(request, candidates)
            .await?
            .into_iter()
            .next()
        else {
            if exists {
                return Err(MethodError::Unsupported("a file the viewer can't see"));
            }
            return Err(MethodError::Conduit {
                code: ERR_BAD_PHID.to_owned(),
                info: None,
            });
        };
        let storage = Storage::load(database, file.phid.as_str())
            .await?
            .ok_or(MethodError::Unsupported("a file deleted meanwhile"))?;
        let data = storage.load_data(database).await?;
        Ok(json!(
            base64::engine::general_purpose::STANDARD.encode(data)
        ))
    }
}

impl Method for FileDownload {
    fn name(&self) -> &'static str {
        "file.download"
    }

    fn params(&self) -> &'static [&'static str] {
        &["phid"]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[(ERR_BAD_PHID, "No such file exists.")]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}
