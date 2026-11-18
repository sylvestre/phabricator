//! `file.info` (`FileInfoConduitAPIMethod`): one file by ID or PHID.

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::{Phid, php};
use phab_db::rows;
use serde_json::{Value, json};

use crate::search::{self, APPLICATION};

/// `file.info`.
pub struct FileInfo;

impl Method for FileInfo {
    fn name(&self) -> &'static str {
        "file.info"
    }

    fn params(&self) -> &'static [&'static str] {
        &["phid", "id"]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let context = request.context;
            let database = &context.database;
            // A truthy ID wins; otherwise the PHID, which may be missing.
            let candidates = match request.value("id") {
                Some(id) if php::is_truthy(id) => {
                    let id = match id {
                        Value::Number(number) if !number.is_f64() => number.as_i64(),
                        Value::String(text) if text.bytes().all(|b| b.is_ascii_digit()) => {
                            text.parse().ok()
                        }
                        _ => None,
                    }
                    .ok_or(MethodError::Unsupported("an unusual file ID"))?;
                    crate::load_by_ids(database, &[id]).await?
                }
                _ => match request.value("phid") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::String(phid)) => {
                        crate::load_by_phids(database, &[Phid::new(phid.clone())]).await?
                    }
                    Some(_) => return Err(MethodError::Unsupported("an unusual file PHID")),
                },
            };
            let file = search::visible_files(request, candidates)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| MethodError::Conduit {
                    code: "ERR-NOT-FOUND".to_owned(),
                    info: Some("No such file exists.".to_owned()),
                })?;
            let sql = format!(
                "SELECT dateCreated, dateModified FROM {} WHERE id = ?",
                database.table("file", "file")
            );
            let row = phab_db::sqlx::query(&sql)
                .bind(file.id)
                .fetch_one(database.pool())
                .await?;
            let production = match context.config.nonempty_string("phabricator.production-uri") {
                Some(uri) => uri.to_owned(),
                None => context
                    .config
                    .base_uri()
                    .map(str::to_owned)
                    .map_err(MethodError::internal)?,
            };
            // Lisk keeps MySQL's strings.
            Ok(json!({
                "id": file.id.to_string(),
                "phid": file.phid.as_str(),
                "objectName": file.monogram(),
                "name": file.name,
                "mimeType": file.mime_type,
                "byteSize": file.byte_size.to_string(),
                "authorPHID": file.author_phid.as_ref().map(Phid::as_str),
                "dateCreated": rows::int(&row, "dateCreated")?.to_string(),
                "dateModified": rows::int(&row, "dateModified")?.to_string(),
                "uri": format!("{}/{}", production.trim_end_matches('/'), file.monogram()),
            }))
        })
    }
}
