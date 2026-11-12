//! The request flow of `PhabricatorConduitAPIController::handleRequest()`
//! and `ConduitCall`.

use std::time::Instant;

use phab_auth::{
    Outcome, User, Writes,
    session::{SessionKind, session_kind},
    token::authenticate_token,
};
use phab_core::config::is_truthy;
use phab_policy::{
    Viewer,
    filter::can_see_application,
    loader::{NoExtendedObjects, load_facts},
};
use serde_json::{Map, Value};

use crate::{
    ApiRequest, Context, Envelope, HttpRequest, Method, MethodError, Mode, Registry, Response,
    request::{DecodeError, Params, decode, get_str, metadata_key, request_data},
};

/// Result of `dispatch`, with what the call log needs.
pub struct Dispatched {
    pub response: Response,
    pub caller: Option<User>,
}

enum Failure {
    Envelope(Envelope),
    Fallback(&'static str),
}

impl From<Envelope> for Failure {
    fn from(envelope: Envelope) -> Self {
        Self::Envelope(envelope)
    }
}

impl From<DecodeError> for Failure {
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::Core(message) => Envelope::error("ERR-CONDUIT-CORE", message).into(),
            DecodeError::Unsupported(reason) => Self::Fallback(reason),
        }
    }
}

fn internal(context: &str, error: impl std::fmt::Display) -> Failure {
    tracing::error!(%error, "{context}");
    Failure::Fallback("internal error")
}

/// Text PHP would print for a scalar with `%s`.
fn php_string(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(false) => String::new(),
        Value::Bool(true) => "1".to_owned(),
        Value::String(text) => text.clone(),
        Value::Array(_) | Value::Object(_) => "Array".to_owned(),
        Value::Number(number) => number.to_string(),
    }
}

pub async fn dispatch(
    context: &Context,
    registry: &Registry,
    method_name: &str,
    http: &HttpRequest,
    mode: Mode,
    now: i64,
) -> Dispatched {
    let start = Instant::now();
    let Some(method) = registry.get(method_name) else {
        return Dispatched {
            response: Response::Fallback("method not ported"),
            caller: None,
        };
    };
    let writes = if mode == Mode::Serve && !context.config.is_read_only() {
        Writes::Allowed
    } else {
        Writes::Skipped
    };

    let mut caller = None;
    let response = match run(context, method.as_ref(), http, writes, now, &mut caller).await {
        Ok(envelope) | Err(Failure::Envelope(envelope)) => Response::Envelope(envelope),
        Err(Failure::Fallback(reason)) => Response::Fallback(reason),
    };

    if let Response::Envelope(envelope) = &response
        && writes == Writes::Allowed
    {
        let duration = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        if let Err(error) = write_call_log(
            context,
            method_name,
            envelope,
            caller.as_ref(),
            duration,
            now,
        )
        .await
        {
            tracing::error!(%error, "writing the Conduit call log");
        }
    }

    Dispatched { response, caller }
}

async fn run(
    context: &Context,
    method: &dyn Method,
    http: &HttpRequest,
    writes: Writes,
    now: i64,
    caller: &mut Option<User>,
) -> Result<Envelope, Failure> {
    let Some(data) = request_data(http) else {
        return Err(Failure::Fallback("multipart request"));
    };
    if get_str(&data, "output").as_deref() == Some("human") {
        return Err(Failure::Fallback("human-readable output"));
    }

    let Params {
        metadata,
        params,
        strictly_typed,
    } = decode(http, &data, method.name())?;

    check_method_and_params(context, method, &params)?;

    let mut viewer = Viewer::anonymous();
    if method.requires_authentication() {
        let user = authenticate(context, http, &metadata, writes, now).await?;
        viewer = user.viewer();
        *caller = Some(user);
    }

    let request = ApiRequest {
        context,
        user: caller.clone(),
        viewer,
        params,
        strictly_typed,
        now,
        writes,
    };
    execute(context, method, &request).await
}

/// The checks of `ConduitCall::__construct()`.
fn check_method_and_params(
    context: &Context,
    method: &dyn Method,
    params: &Map<String, Value>,
) -> Result<(), Failure> {
    if let Some(application) = method.application() {
        let policies = phab_policy::ApplicationPolicies::from_config(&context.config);
        if !policies.is_installed(application.class) {
            return Err(Envelope::error(
                "ERR-CONDUIT-CALL",
                format!(
                    "Method '{}' belongs to application '{}', which is not installed.",
                    method.name(),
                    application.name
                ),
            )
            .into());
        }
    }
    let invalid: Vec<&str> = params
        .keys()
        .map(String::as_str)
        .filter(|key| !method.params().contains(key))
        .collect();
    if !invalid.is_empty() {
        return Err(Envelope::error(
            "ERR-CONDUIT-CALL",
            format!(
                "API Method \"{}\" does not define these parameters: '{}'.",
                method.name(),
                invalid.join("', '")
            ),
        )
        .into());
    }
    debug_assert!(
        method
            .params()
            .iter()
            .all(|key| metadata_key(key).is_none())
    );
    Ok(())
}

/// `PhabricatorConduitAPIController::authenticateUser()`.
async fn authenticate(
    context: &Context,
    http: &HttpRequest,
    metadata: &Map<String, Value>,
    writes: Writes,
    now: i64,
) -> Result<User, Failure> {
    // A logged-in browser session takes precedence and needs CSRF checks.
    if http
        .session_cookie
        .as_deref()
        .is_some_and(|cookie| session_kind(cookie) == SessionKind::User)
    {
        return Err(Failure::Fallback("web session cookie"));
    }

    match metadata.get("auth.type") {
        None | Some(Value::Null) => {}
        Some(Value::String(kind)) if kind == "asymmetric" => {
            return Err(Failure::Fallback("signed request"));
        }
        Some(other) => {
            return Err(Envelope::error(
                "ERR-INVALID-AUTH",
                format!(
                    "Provided \"auth.type\" (\"{}\") is not recognized.",
                    php_string(other)
                ),
            )
            .into());
        }
    }

    let token = metadata.get("token").map(php_string).unwrap_or_default();
    if !token.is_empty() {
        let outcome = authenticate_token(&context.database, &context.config, &token, now, writes)
            .await
            .map_err(|error| internal("authenticating an API token", error))?;
        return match outcome {
            Outcome::Done(Ok(user)) => Ok(user),
            Outcome::Done(Err(invalid)) => {
                Err(Envelope::error("ERR-INVALID-AUTH", invalid.0).into())
            }
            Outcome::Unsupported(reason) => Err(Failure::Fallback(reason)),
        };
    }

    if context.config.truthy("cluster.addresses") {
        return Err(Failure::Fallback("cluster request"));
    }
    if metadata.contains_key("authUser") {
        return Err(Failure::Fallback("certificate authentication"));
    }
    if metadata.get("sessionKey").is_some_and(is_truthy) {
        return Err(Failure::Fallback("Conduit session"));
    }
    Err(Envelope::error("ERR-INVALID-SESSION", "Session key is not present.").into())
}

/// `ConduitCall::executeMethod()` and the error handling around it.
async fn execute(
    context: &Context,
    method: &dyn Method,
    request: &ApiRequest<'_>,
) -> Result<Envelope, Failure> {
    if method.requires_authentication() {
        let allow_public = method.allows_public() && context.config.allow_public();
        if !allow_public && !request.viewer.is_logged_in() && !request.viewer.is_omnipotent {
            return Err(conduit_exception(method, "ERR-INVALID-AUTH", None).into());
        }
        if let Some(application) = method.application() {
            let subject = phab_policy::ApplicationPolicies::from_config(&context.config)
                .subject(application.class, context.config.allow_public());
            let facts = load_facts(
                &context.database,
                &context.config,
                &request.viewer,
                &[&subject],
                &NoExtendedObjects,
                request.now,
            )
            .await
            .map_err(|error| internal("loading application policies", error))?;
            let can_view = can_see_application(&request.viewer, application.class, &facts)
                .map_err(|error| internal("checking application policies", error))?;
            if !can_view {
                // PHP passes the message as the error code.
                return Err(conduit_exception(
                    method,
                    "You do not have access to the application which provides this API method.",
                    None,
                )
                .into());
            }
        }
    }

    match method.execute(request).await {
        Ok(result) => Ok(Envelope::success(result)),
        Err(MethodError::Conduit { code, info }) => {
            Err(conduit_exception(method, &code, info).into())
        }
        Err(MethodError::Unsupported(reason)) => Err(Failure::Fallback(reason)),
        Err(MethodError::Internal(error)) => Err(internal("running a Conduit method", error)),
    }
}

fn conduit_exception(method: &dyn Method, code: &str, info: Option<String>) -> Envelope {
    let info = info.unwrap_or_else(|| {
        method
            .error_types()
            .iter()
            .find(|(known, _)| *known == code)
            .map_or("Unknown Error", |(_, description)| description)
            .to_owned()
    });
    Envelope::error(code, info)
}

async fn write_call_log(
    context: &Context,
    method_name: &str,
    envelope: &Envelope,
    caller: Option<&User>,
    duration: u64,
    now: i64,
) -> Result<(), phab_db::sqlx::Error> {
    let sql = format!(
        "INSERT INTO {} (method, error, duration, dateCreated, dateModified, callerPHID) \
         VALUES (?, ?, ?, ?, ?, ?)",
        context.database.table("conduit", "conduit_methodcalllog")
    );
    phab_db::sqlx::query(&sql)
        .bind(method_name)
        .bind(envelope.error_code.as_deref().unwrap_or_default())
        .bind(duration)
        .bind(now)
        .bind(now)
        .bind(caller.map(|user| user.phid.as_str()))
        .execute(context.database.pool())
        .await?;
    Ok(())
}
