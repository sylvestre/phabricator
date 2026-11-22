//! `harbormaster.createartifact`
//! (`HarbormasterCreateArtifactConduitAPIMethod`): an artifact of a build
//! target (`HarbormasterBuildTarget::createArtifact()`), a URI
//! (`HarbormasterURIArtifact`) or a file (`HarbormasterFileArtifact`).
//!
//! Ported: both types, with the checks of the target and its plan.
//! Left to PHP: parameters which are not strings, data PHP's type checks
//! refuse (their messages are `PhutilTypeSpec`'s), raw HTTP parameters
//! (cast by `readArtifactHTTPParameter()`), URIs `PhutilURI` refuses to
//! parse, keys already used in the build's generation (MySQL refuses the
//! row) or longer than their column, and run capability checks on plans
//! editable by policies other than the global ones.

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture, pager};
use phab_core::{Phid, PhidType, hash, php};
use serde_json::{Map, Value, json};

use crate::storage::Target;
use crate::writes::{self, Receiver, fits, string_param, unsupported};

/// `HarbormasterBuildArtifactPHIDType::TYPECONST`.
const ARTIFACT: PhidType = PhidType::new(*b"HMBA");

/// `getArtifactParameterSpecification()`: each key, whether it is
/// required, and whether it is a `bool` (otherwise a `string`).
fn specification(kind: &str) -> Option<&'static [(&'static str, bool, bool)]> {
    match kind {
        "file" => Some(&[("filePHID", true, false)]),
        "uri" => Some(&[
            ("uri", true, false),
            ("name", false, false),
            ("ui.external", false, true),
        ]),
        _ => None,
    }
}

/// `validateArtifactData()` (`PhutilTypeSpec::checkMap()`): failures are
/// `PhutilTypeSpec` exceptions, left to PHP.
fn validate(kind: &str, data: &Map<String, Value>) -> Result<(), MethodError> {
    let spec = specification(kind).ok_or_else(|| MethodError::internal("an unknown type"))?;
    if data
        .keys()
        .any(|key| !spec.iter().any(|(known, _, _)| known == key))
    {
        return Err(unsupported("artifact data with extra keys"));
    }
    for (key, required, is_bool) in spec {
        let valid = match data.get(*key) {
            None => !required,
            Some(Value::Bool(_)) => *is_bool,
            Some(Value::String(_)) => !is_bool,
            Some(_) => false,
        };
        if !valid {
            return Err(unsupported("artifact data PHP's type checks refuse"));
        }
    }
    Ok(())
}

/// `HarbormasterURIArtifact::validateURI()`.
fn validate_uri(request: &ApiRequest<'_>, uri: &str) -> Result<(), MethodError> {
    let protocol = phab_remarkup::uri::protocol(uri.as_bytes())
        .map_err(|_| unsupported("URIs PhutilURI refuses"))?;
    let protocol = String::from_utf8(protocol).map_err(|_| unsupported("unusual protocols"))?;
    if protocol.is_empty() {
        return Err(pager::core_error(format!(
            "Unable to identify the protocol for URI \"{uri}\". URIs must be fully qualified \
             and have an identifiable protocol."
        )));
    }
    let key = "uri.allowed-protocols";
    let protocols = request.context.config.object(key);
    let allowed = protocols
        .and_then(|protocols| protocols.get(&protocol))
        .is_some_and(php::is_truthy);
    if !allowed {
        let names: Vec<&str> = protocols
            .into_iter()
            .flat_map(|protocols| protocols.keys().map(String::as_str))
            .collect();
        return Err(pager::core_error(format!(
            "URI \"{uri}\" does not have an allowable protocol. Configure protocols in `{key}`. \
             Allowed protocols are: {}.",
            names.join(", ")
        )));
    }
    Ok(())
}

/// `HarbormasterFileArtifact::loadArtifactFile()`: the file must be one
/// the viewer sees.
async fn check_file(request: &ApiRequest<'_>, file_phid: &str) -> Result<(), MethodError> {
    let database = &request.context.database;
    let files = phab_app_files::load_by_phids(database, &[Phid::new(file_phid)]).await?;
    let visible = phab_app_files::search::visible_files(request, files).await?;
    if visible.is_empty() {
        return Err(pager::core_error(format!(
            "File PHID \"{file_phid}\" does not correspond to a valid file."
        )));
    }
    Ok(())
}

/// Whether the build's generation already has an artifact of this type
/// and index (the `key_artifact` unique key).
async fn exists(request: &ApiRequest<'_>, kind: &str, index: &str) -> Result<bool, MethodError> {
    let database = &request.context.database;
    let sql = format!(
        "SELECT id FROM {} WHERE artifactType = ? AND artifactIndex = ?",
        database.table("harbormaster", "harbormaster_buildartifact")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(kind)
        .bind(index)
        .fetch_optional(database.pool())
        .await?
        .is_some())
}

/// `createArtifact()`: checks the data, then saves the row. Returns its
/// PHID.
async fn create(
    request: &ApiRequest<'_>,
    receiver: &Receiver,
    (key, kind, data): (&str, &str, &Map<String, Value>),
) -> Result<String, MethodError> {
    if specification(kind).is_none() {
        return Err(pager::core_error(format!(
            "There is no implementation available for artifacts of type \"{kind}\"."
        )));
    }
    validate(kind, data)?;
    let target: &Target = &receiver.target;
    // `HarbormasterBuildArtifact::getArtifactIndex()`.
    let index = hash::digest_for_index(
        format!(
            "{}\0{}\0{key}",
            receiver.build.phid.as_str(),
            target.generation
        )
        .as_bytes(),
    );
    // `willCreateArtifact()`.
    match (kind, data.get("uri"), data.get("filePHID")) {
        ("uri", Some(Value::String(uri)), _) => validate_uri(request, uri)?,
        ("file", _, Some(Value::String(file))) => check_file(request, file).await?,
        _ => {
            return Err(MethodError::internal(
                "artifact data without its required key",
            ));
        }
    }
    if !fits(key, 255) {
        return Err(unsupported("artifact keys longer than their column"));
    }
    if exists(request, kind, &index).await? {
        return Err(unsupported("artifact keys already used"));
    }
    writes::actor(request)?;
    let phid = Phid::generate(ARTIFACT, None).into_string();
    let database = &request.context.database;
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, buildTargetPHID, artifactType, artifactIndex, artifactKey, \
         artifactData, isReleased, dateCreated, dateModified) VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?)",
        database.table("harbormaster", "harbormaster_buildartifact")
    ))
    .bind(&phid)
    .bind(target.phid.as_str())
    .bind(kind)
    .bind(&index)
    .bind(key)
    .bind(php::json_encode(&php::round_trip(&Value::Object(
        data.clone(),
    ))))
    .bind(request.now)
    .bind(request.now)
    .execute(database.pool())
    .await?;
    Ok(phid)
}

/// `harbormaster.createartifact`.
pub struct CreateArtifact;

impl CreateArtifact {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let target_phid =
            string_param(request, "buildTargetPHID")?.ok_or_else(|| unsupported("no target"))?;
        let target = writes::visible_where::<Target>(request, "phid", target_phid)
            .await?
            .pop()
            .ok_or_else(|| pager::core_error(format!("No such build target \"{target_phid}\"!")))?;
        let receiver = writes::check_target(
            request,
            target,
            ("create an artifact for", "create artifacts"),
        )
        .await?;
        if !request.strictly_typed {
            return Err(unsupported("raw HTTP artifact parameters"));
        }
        let kind = string_param(request, "artifactType")?
            .ok_or_else(|| unsupported("artifacts without a type"))?;
        let Some(Value::Object(data)) = request.value("artifactData") else {
            return Err(unsupported("artifact data which is not a dictionary"));
        };
        let key = string_param(request, "artifactKey")?
            .ok_or_else(|| unsupported("artifacts without a key"))?;
        let phid = create(request, &receiver, (key, kind, data)).await?;
        Ok(json!({"data": [{"phid": phid}]}))
    }
}

impl Method for CreateArtifact {
    fn name(&self) -> &'static str {
        "harbormaster.createartifact"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "buildTargetPHID",
            "artifactKey",
            "artifactType",
            "artifactData",
        ]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    #[test]
    fn validates_like_php() {
        assert!(
            validate(
                "uri",
                &map(json!({"uri": "https://x", "ui.external": true}))
            )
            .is_ok()
        );
        assert!(validate("file", &map(json!({"filePHID": "PHID-FILE-1"}))).is_ok());
        for (kind, data) in [
            ("uri", json!({})),
            ("uri", json!({"uri": "https://x", "other": 1})),
            ("uri", json!({"uri": "https://x", "ui.external": "1"})),
            ("uri", json!({"uri": "https://x", "name": null})),
            ("file", json!({"filePHID": 1})),
        ] {
            assert!(validate(kind, &map(data.clone())).is_err(), "{data}");
        }
    }
}
