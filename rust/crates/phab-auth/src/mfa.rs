//! Multi-factor authentication with TOTP factors:
//! `PhabricatorAuthSessionEngine::requireHighSecuritySession()` and
//! `requireHighSecurityToken()`, with `PhabricatorTOTPAuthFactor` and the
//! `auth_challenge` rows of `PhabricatorAuthChallenge`.
//!
//! Only TOTP is ported: a user with a factor of another type (SMS, Duo) is
//! [`Outcome::Unsupported`]. So is everything PHP answers with an
//! exception page (rate limits, malformed or forged challenge tokens, a
//! factor with several live challenges) and the challenge digest key PHP
//! would create. All of it is decided by [`plan()`] before anything is
//! written, so the caller can hand the request to PHP, which replays it on
//! the same rows; [`execute()`] then writes what PHP writes, in its order.
//!
//! The replay defences are PHP's: a challenge is bound to one session and
//! workflow, lives until no code which answers it can answer another
//! challenge, answers must fit the window of both the challenge and the
//! current timestep, an answered challenge can only be carried forward by
//! the client holding its response token, and a completed one is never
//! accepted again.

use std::fmt;

use phab_core::{Phid, PhidType, php, random};
use phab_db::{Database, rows};
use serde_json::{Map, Value, json};

use crate::rate_limit::{self, TRY_FACTOR};
use crate::session::{self, SessionUser};
use crate::totp::{self, TotpSecret};
use crate::user_log::{self, LogEntry, LogType, RequestInfo};
use crate::{AuthError, Outcome};

/// `PhabricatorAuthChallengePHIDType`.
const CHALLENGE_PHID_TYPE: PhidType = PhidType::new(*b"CHAL");
/// `PhabricatorAuthChallenge::TOKEN_DIGEST_KEY`.
const TOKEN_DIGEST_KEY: &str = "auth.challenge.token";
/// `PhabricatorAuthChallenge::HTTPKEY`: the form field carrying response
/// tokens of challenges answered earlier.
pub const CHALLENGES_FIELD: &str = "__hisec.challenges__";
/// `getWorkflowKey()` of a session engine nobody gave a workflow key.
pub const LEGACY_WORKFLOW: &str = "legacy";
/// `PhabricatorTOTPAuthFactor::getFactorKey()`.
const TOTP_FACTOR_KEY: &str = "totp";
/// Length of response tokens (`markChallengeAsAnswered()`).
const RESPONSE_TOKEN_LENGTH: usize = 32;
/// How long a response token proves an answer (`newResultFromChallengeResponse()`).
const RESPONSE_TTL: i64 = 60;
/// How long a session stays in high security (`15 minutes in seconds`).
const HIGH_SECURITY_DURATION: i64 = 15 * 60;

/// A user's TOTP factor (`auth_factorconfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Factor {
    /// The row ID, which names the response field.
    pub id: i64,
    /// The factor PHID, which challenges point to.
    pub phid: Phid,
    /// The name the user gave it.
    pub name: String,
    secret: TotpSecret,
}

impl Factor {
    /// `getChallengeResponseParameterName()`.
    pub fn response_field(&self) -> String {
        format!("authfactor.{}.mfa.response", self.id)
    }
}

/// A challenge response token (`PhutilOpaqueEnvelope`), never printed.
#[derive(Clone, PartialEq, Eq)]
struct ResponseToken(String);

impl fmt::Debug for ResponseToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ResponseToken(<redacted>)")
    }
}

/// An `auth_challenge` row.
// The fields are the columns.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, PartialEq)]
struct Challenge {
    /// `None` until inserted.
    id: Option<i64>,
    phid: Phid,
    factor_phid: Phid,
    session_phid: Phid,
    workflow_key: String,
    challenge_key: String,
    challenge_ttl: i64,
    response_digest: Option<String>,
    response_ttl: Option<i64>,
    is_completed: bool,
    properties: Map<String, Value>,
    /// The token the request proved it holds (`setResponseToken()`).
    response_token: Option<ResponseToken>,
}

impl Challenge {
    /// `getIsAnsweredChallenge()`.
    fn is_answered(&self) -> bool {
        self.response_digest
            .as_deref()
            .is_some_and(|digest| !digest.is_empty())
    }

    /// `getIsReusedChallenge()`.
    fn is_reused(&self) -> bool {
        self.is_completed || (self.is_answered() && self.response_token.is_none())
    }

    /// The timestep the challenge was issued at, `(int)` of its key.
    fn timestep(&self) -> i64 {
        php_int(&self.challenge_key)
    }
}

/// PHP's `(int)` of a numeric string: leading digits, 0 otherwise.
fn php_int(text: &str) -> i64 {
    let text = text.trim_start();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let end = digits
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse::<i64>().map_or(0, |value| sign * value)
}

/// What a factor's part of the prompt shows (`PhabricatorAuthFactorResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactorState {
    /// Nothing submitted yet: an empty code field.
    Empty,
    /// No answer can be taken now (`setIsWait()`); the message says why.
    Wait(String),
    /// The code field with what was typed and `Invalid` or `Required`.
    Rejected {
        /// The submitted code, shown back.
        value: String,
        /// `Invalid` or `Required`.
        error: &'static str,
    },
    /// The factor was answered; its token rides in [`CHALLENGES_FIELD`].
    Answered,
}

/// One factor of the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptFactor {
    /// The factor.
    pub factor: Factor,
    /// What to show for it.
    pub state: FactorState,
}

/// The value of [`CHALLENGES_FIELD`] for the re-rendered form: response
/// tokens, which only the form may hold. Never printed.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct AnsweredTokens(String);

impl AnsweredTokens {
    /// The field value, for the form.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether any challenge was answered.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for AnsweredTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AnsweredTokens(<redacted>)")
    }
}

/// What `requireHighSecuritySession()` ends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HighSecurity {
    /// A `PhabricatorAuthHighSecurityToken`: the caller may go on.
    Token,
    /// `PhabricatorAuthHighSecurityRequiredException`: the prompt.
    Prompt {
        /// The factors, oldest last as PHP orders them.
        factors: Vec<PromptFactor>,
        /// Tokens of the challenges answered so far.
        answered: AnsweredTokens,
    },
}

/// How the caller wants high security.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode<'a> {
    /// `requireHighSecuritySession()` (true) or `requireHighSecurityToken()`.
    pub upgrade_session: bool,
    /// Put a partial session straight into high security
    /// (`jump_to_hisec`).
    pub jump_into_hisec: bool,
    /// `setWorkflowKey()`: challenges answer one workflow only.
    pub workflow_key: &'a str,
}

/// What the request says, read by the caller from its form.
pub struct HisecRequest<'a> {
    /// `isHTTPPost()`. The caller has validated its CSRF token: PHP throws
    /// on a bad one.
    pub is_post: bool,
    /// `getExists('__hisec__')`.
    pub is_hisec_form: bool,
    /// `getStr()` of a form field.
    pub field: &'a (dyn Fn(&str) -> Option<String> + Sync),
}

/// `PhabricatorAuthSession::isHighSecuritySession()`.
pub fn is_high_security(found: &SessionUser, now: i64) -> bool {
    found
        .session
        .high_security_until
        .is_some_and(|until| until != 0 && until >= now)
}

/// The user's factors whose provider is active or deprecated, newest
/// first (the query's order, which sorting by `newSortVector()` keeps for
/// factors of one type). `Unsupported` for factors other than TOTP.
pub async fn load_factors(
    database: &Database,
    user: &Phid,
) -> Result<Outcome<Vec<Factor>>, AuthError> {
    let sql = format!(
        "SELECT config.id, config.phid, config.factorName, config.factorSecret, \
           provider.providerFactorKey FROM {} config \
         JOIN {} provider ON config.factorProviderPHID = provider.phid \
         WHERE config.userPHID IN (?) AND provider.status IN ('active', 'deprecated') \
         ORDER BY config.id DESC",
        database.table("auth", "auth_factorconfig"),
        database.table("auth", "auth_factorprovider"),
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(user.as_str())
        .fetch_all(database.pool())
        .await?;
    let mut factors = Vec::with_capacity(found.len());
    for row in &found {
        if rows::text(row, "providerFactorKey")? != TOTP_FACTOR_KEY {
            return Ok(Outcome::Unsupported(
                "a multi-factor factor other than TOTP",
            ));
        }
        let Some(secret) = TotpSecret::new(rows::text(row, "factorSecret")?) else {
            return Ok(Outcome::Unsupported("a TOTP secret which is not base32"));
        };
        factors.push(Factor {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            name: rows::text(row, "factorName")?,
            secret,
        });
    }
    Ok(Outcome::Done(factors))
}

/// The unexpired challenges of `factors` (`withChallengeTTLBetween($now,
/// null)`), newest first.
async fn load_challenges(
    database: &Database,
    user: &Phid,
    factors: &[Factor],
    now: i64,
) -> Result<Vec<Challenge>, AuthError> {
    let sql = format!(
        "SELECT id, phid, factorPHID, sessionPHID, workflowKey, challengeKey, challengeTTL, \
           responseDigest, responseTTL, isCompleted, properties FROM {} \
         WHERE userPHID IN (?) AND factorPHID IN ({}) AND challengeTTL >= ? ORDER BY id DESC",
        database.table("auth", "auth_challenge"),
        phab_db::placeholders(factors.len()),
    );
    let mut query = phab_db::sqlx::query(&sql).bind(user.as_str());
    for factor in factors {
        query = query.bind(factor.phid.as_str());
    }
    let found = query.bind(now).fetch_all(database.pool()).await?;
    let mut challenges = Vec::with_capacity(found.len());
    for row in &found {
        let properties = match rows::json(row, "properties")? {
            Value::Object(properties) => properties,
            _ => Map::new(),
        };
        challenges.push(Challenge {
            id: Some(rows::int(row, "id")?),
            phid: Phid::new(rows::text(row, "phid")?),
            factor_phid: Phid::new(rows::text(row, "factorPHID")?),
            session_phid: Phid::new(rows::text(row, "sessionPHID")?),
            workflow_key: rows::text(row, "workflowKey")?,
            challenge_key: rows::text(row, "challengeKey")?,
            challenge_ttl: rows::int(row, "challengeTTL")?,
            response_digest: rows::opt_text(row, "responseDigest")?,
            response_ttl: rows::opt_int(row, "responseTTL")?,
            is_completed: rows::flag(row, "isCompleted")?,
            properties,
            response_token: None,
        });
    }
    Ok(challenges)
}

/// The `auth.challenge.token` HMAC key, which PHP creates the first time
/// it needs it.
#[derive(Clone)]
struct TokenKey(Vec<u8>);

impl TokenKey {
    /// `digestWithNamedKey($token, 'auth.challenge.token')`.
    fn digest(&self, token: &str) -> Result<String, AuthError> {
        Ok(phab_core::hash::hmac_sha256(token.as_bytes(), &self.0)?)
    }
}

/// `newChallengeResponsesFromRequest()`: attaches the tokens the request
/// carries to the challenges they answer, if their response window is
/// still open. `Err` where PHP throws.
fn attach_response_tokens(
    challenges: &mut [Challenge],
    field: Option<&str>,
    key: &TokenKey,
    now: i64,
) -> Result<Result<(), &'static str>, AuthError> {
    let mut tokens: Vec<(&str, &str)> = Vec::new();
    for element in field.unwrap_or_default().split(' ') {
        let element = element.trim_matches(' ');
        if element.is_empty() {
            continue;
        }
        // `/^[^:]+:/`.
        let Some((phid, token)) = element.split_once(':').filter(|(phid, _)| !phid.is_empty())
        else {
            return Ok(Err("a malformed multi-factor challenge token"));
        };
        if tokens.iter().any(|(known, _)| *known == phid) {
            return Ok(Err("a multi-factor challenge token given twice"));
        }
        tokens.push((phid, token));
    }
    for challenge in challenges {
        // `getResponseTTL() < $now`, where a missing TTL is 0.
        if challenge.response_ttl.unwrap_or(0) < now {
            continue;
        }
        let Some((_, token)) = tokens
            .iter()
            .find(|(phid, _)| *phid == challenge.phid.as_str())
        else {
            continue;
        };
        // `setResponseToken()`.
        let digest = key.digest(token)?;
        match &challenge.response_digest {
            Some(stored) => {
                if !phab_core::hash::hashes_are_identical(digest.as_bytes(), stored.as_bytes()) {
                    return Ok(Err("a forged multi-factor challenge token"));
                }
            }
            None => challenge.response_digest = Some(digest),
        }
        challenge.response_token = Some(ResponseToken((*token).to_owned()));
    }
    Ok(Ok(()))
}

/// `PhutilNumber` seconds, pluralized like `PhabricatorUSEnglishTranslation`.
fn seconds(count: i64) -> String {
    if count == 1 {
        "1 second".to_owned()
    } else {
        format!("{count} seconds")
    }
}

/// `PhabricatorTOTPAuthFactor::newResultFromIssuedChallenges()`: a wait
/// message if the factor can't take an answer now.
fn wait_reason(
    challenges: &[&Challenge],
    session_phid: &Phid,
    workflow_key: &str,
    now: i64,
) -> Option<String> {
    let current = totp::timestep(now);
    for challenge in challenges {
        let wait = seconds(challenge.challenge_ttl - now + 1);
        if challenge.session_phid != *session_phid {
            return Some(format!(
                "This factor recently issued a challenge to a different login session. Wait \
                 {wait} for the code to cycle, then try again."
            ));
        }
        if challenge.workflow_key != workflow_key {
            return Some(format!(
                "This factor recently issued a challenge for a different workflow. Wait {wait} \
                 for the code to cycle, then try again."
            ));
        }
        if !totp::window_contains(challenge.timestep(), current) {
            return Some(format!(
                "This factor recently issued a challenge which has expired. A new challenge can \
                 not be issued yet. Wait {wait} for the code to cycle, then try again."
            ));
        }
        if challenge.is_reused() {
            return Some(format!(
                "You recently provided a response to this factor. Responses may not be reused. \
                 Wait {wait} for the code to cycle, then try again."
            ));
        }
    }
    None
}

/// `getChallengeResponseFromRequest()`: the trimmed code.
fn response(request: &HisecRequest<'_>, factor: &Factor) -> String {
    (request.field)(&factor.response_field())
        .unwrap_or_default()
        .trim_matches([' ', '\t', '\n', '\r', '\0', '\x0b'])
        .to_owned()
}

/// What happens to one factor.
#[derive(Debug, Clone, PartialEq)]
enum Verdict {
    /// No answer is looked at: a wait, or the prompt before a submission.
    Shown(FactorState),
    /// Answered earlier; the token proves it.
    CarriedOver { challenge: usize },
    /// Answered now, at this timestep.
    AnsweredNow { challenge: usize, timestep: i64 },
    /// Answered wrong, or not at all.
    Rejected { value: String, error: &'static str },
}

impl Verdict {
    fn is_valid(&self) -> bool {
        matches!(self, Self::CarriedOver { .. } | Self::AnsweredNow { .. })
    }

    /// Whether `newHighSecurityToken()` has a validation result for the
    /// factor before looking at answers.
    fn is_early(&self) -> bool {
        matches!(self, Self::Shown(FactorState::Wait(_)))
    }
}

/// Everything [`execute()`] writes, decided by [`plan()`].
pub struct Plan {
    viewer: SessionUser,
    factors: Vec<Factor>,
    challenges: Vec<Challenge>,
    /// Indexes of `challenges` to insert.
    new_challenges: Vec<usize>,
    verdicts: Vec<Verdict>,
    any_attempt: bool,
    validated: bool,
    upgrade_session: bool,
    jump_into_hisec: bool,
    key: Option<TokenKey>,
}

impl fmt::Debug for Plan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Plan")
            .field("factors", &self.factors.len())
            .field("new_challenges", &self.new_challenges.len())
            .field("any_attempt", &self.any_attempt)
            .field("validated", &self.validated)
            .finish_non_exhaustive()
    }
}

/// What [`plan()`] decided.
#[derive(Debug)]
pub enum Planned {
    /// High security is already granted, or needs no factor; nothing to
    /// write.
    Token,
    /// The prompt or the answers, to [`execute()`].
    Challenge(Box<Plan>),
}

/// Everything `newHighSecurityToken()` reads and decides, without
/// writing. `Unsupported` for every path PHP answers with a page Rust
/// doesn't render.
// One function, as `newHighSecurityToken()` is, so the order of its checks
// can be compared.
#[allow(clippy::too_many_lines)]
pub async fn plan(
    database: &Database,
    viewer: &SessionUser,
    request: &HisecRequest<'_>,
    mode: Mode<'_>,
    now: i64,
) -> Result<Outcome<Planned>, AuthError> {
    if is_high_security(viewer, now) {
        return Ok(Outcome::Done(Planned::Token));
    }
    let factors = match load_factors(database, &viewer.user.phid).await? {
        Outcome::Done(factors) => factors,
        Outcome::Unsupported(reason) => return Ok(Outcome::Unsupported(reason)),
    };
    // No factor: a token, without high security (`setIsUnchallengedToken()`).
    if factors.is_empty() {
        return Ok(Outcome::Done(Planned::Token));
    }
    // `willTakeAction(..., 0)`: PHP's rate limit page, before any write.
    if rate_limit::is_blocked(database, viewer.user.phid.as_str(), TRY_FACTOR, now).await? {
        return Ok(Outcome::Unsupported("too many multi-factor attempts"));
    }
    let key = session::named_hmac_key(database, TOKEN_DIGEST_KEY)
        .await?
        .map(TokenKey);
    let validating = request.is_post && request.is_hisec_form;
    let mut challenges = load_challenges(database, &viewer.user.phid, &factors, now).await?;
    let field = (request.field)(CHALLENGES_FIELD);
    if field
        .as_deref()
        .is_some_and(|field| !field.trim().is_empty())
        || validating
    {
        // Checking or making response tokens needs the key PHP would
        // otherwise create.
        let Some(key) = &key else {
            return Ok(Outcome::Unsupported(
                "the challenge token key does not exist yet",
            ));
        };
        if let Err(reason) = attach_response_tokens(&mut challenges, field.as_deref(), key, now)? {
            return Ok(Outcome::Unsupported(reason));
        }
    }

    let session_phid = &viewer.session.phid;
    let current = totp::timestep(now);
    let mut new_challenges = Vec::new();
    let mut verdicts = Vec::with_capacity(factors.len());
    for factor in &factors {
        let mut issued: Vec<usize> = (0..challenges.len())
            .filter(|index| challenges[*index].factor_phid == factor.phid)
            .collect();
        // `newIssuedChallenges()`: a challenge for the current timestep,
        // alive until no code answering it could answer a later one.
        if issued.is_empty() {
            challenges.push(Challenge {
                id: None,
                phid: Phid::generate(CHALLENGE_PHID_TYPE, None),
                factor_phid: factor.phid.clone(),
                session_phid: session_phid.clone(),
                workflow_key: mode.workflow_key.to_owned(),
                challenge_key: current.to_string(),
                challenge_ttl: now + (totp::WINDOW_SIZE * 2 + 1) * totp::TIMESTEP_DURATION,
                response_digest: None,
                response_ttl: None,
                is_completed: false,
                properties: Map::new(),
                response_token: None,
            });
            new_challenges.push(challenges.len() - 1);
            issued.push(challenges.len() - 1);
        }
        let issued_challenges: Vec<&Challenge> =
            issued.iter().map(|index| &challenges[*index]).collect();
        if let Some(reason) = wait_reason(&issued_challenges, session_phid, mode.workflow_key, now)
        {
            verdicts.push(Verdict::Shown(FactorState::Wait(reason)));
            continue;
        }
        if !validating {
            verdicts.push(Verdict::Shown(FactorState::Empty));
            continue;
        }
        // `newResultFromChallengeResponse()` throws unless exactly one
        // challenge is live.
        let [index] = issued.as_slice() else {
            return Ok(Outcome::Unsupported(
                "a TOTP factor with several live challenges",
            ));
        };
        let challenge = &challenges[*index];
        if challenge.is_answered() {
            verdicts.push(Verdict::CarriedOver { challenge: *index });
            continue;
        }
        let code = response(request, factor);
        match totp::valid_timestep(&factor.secret, challenge.timestep(), current, &code) {
            // `if ($valid_timestep)`: timestep 0 is falsy.
            Some(timestep) if timestep != 0 => verdicts.push(Verdict::AnsweredNow {
                challenge: *index,
                timestep,
            }),
            _ => verdicts.push(Verdict::Rejected {
                error: if code.is_empty() {
                    "Required"
                } else {
                    "Invalid"
                },
                value: code,
            }),
        }
    }
    // `getRequestHasChallengeResponse()` of a factor without an early
    // result: the attempt counts against the rate limit.
    let any_attempt = validating
        && factors
            .iter()
            .zip(&verdicts)
            .any(|(factor, verdict)| !verdict.is_early() && !response(request, factor).is_empty());
    Ok(Outcome::Done(Planned::Challenge(Box::new(Plan {
        viewer: viewer.clone(),
        factors,
        challenges,
        new_challenges,
        verdicts,
        any_attempt,
        validated: validating,
        upgrade_session: mode.upgrade_session,
        jump_into_hisec: mode.jump_into_hisec,
        key,
    }))))
}

fn properties_json(properties: &Map<String, Value>) -> String {
    if properties.is_empty() {
        return "[]".to_owned();
    }
    php::json_encode(&Value::Object(properties.clone()))
}

async fn insert_challenge(
    database: &Database,
    user: &Phid,
    challenge: &mut Challenge,
    now: i64,
) -> Result<(), AuthError> {
    let sql = format!(
        "INSERT INTO {} (phid, userPHID, factorPHID, sessionPHID, workflowKey, challengeKey, \
           challengeTTL, responseDigest, responseTTL, isCompleted, properties, dateCreated, \
           dateModified) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, 0, ?, ?, ?)",
        database.table("auth", "auth_challenge")
    );
    let result = phab_db::sqlx::query(&sql)
        .bind(challenge.phid.as_str())
        .bind(user.as_str())
        .bind(challenge.factor_phid.as_str())
        .bind(challenge.session_phid.as_str())
        .bind(&challenge.workflow_key)
        .bind(&challenge.challenge_key)
        .bind(challenge.challenge_ttl)
        .bind(properties_json(&challenge.properties))
        .bind(now)
        .bind(now)
        .execute(database.pool())
        .await?;
    challenge.id = Some(i64::try_from(result.last_insert_id()).unwrap_or(i64::MAX));
    Ok(())
}

/// `PhabricatorAuthChallenge::save()` of a loaded challenge: Lisk writes
/// every column; these are the ones which change.
async fn update_challenge(
    database: &Database,
    challenge: &Challenge,
    now: i64,
) -> Result<(), AuthError> {
    let sql = format!(
        "UPDATE {} SET responseDigest = ?, responseTTL = ?, isCompleted = ?, properties = ?, \
           dateModified = ? WHERE id = ?",
        database.table("auth", "auth_challenge")
    );
    phab_db::sqlx::query(&sql)
        .bind(&challenge.response_digest)
        .bind(challenge.response_ttl)
        .bind(i32::from(challenge.is_completed))
        .bind(properties_json(&challenge.properties))
        .bind(now)
        .bind(challenge.id)
        .execute(database.pool())
        .await?;
    Ok(())
}

/// `newHighSecurityToken()` after the reads: issues the challenges, counts
/// the attempt, records the answers and, when every factor is answered,
/// completes the challenges and grants high security.
pub async fn execute(
    database: &Database,
    mut plan: Plan,
    now: i64,
    info: &RequestInfo,
) -> Result<HighSecurity, AuthError> {
    let user_phid = plan.viewer.user.phid.clone();
    for index in plan.new_challenges.clone() {
        insert_challenge(database, &user_phid, &mut plan.challenges[index], now).await?;
    }
    if plan.any_attempt {
        rate_limit::record(database, user_phid.as_str(), TRY_FACTOR, 1.0, now).await?;
    }
    // `markChallengeAsAnswered()`.
    for verdict in &plan.verdicts {
        let Verdict::AnsweredNow {
            challenge,
            timestep,
        } = verdict
        else {
            continue;
        };
        let Some(key) = &plan.key else {
            // `plan()` refuses to validate without the key.
            continue;
        };
        let token = random::random_characters(RESPONSE_TOKEN_LENGTH);
        let challenge = &mut plan.challenges[*challenge];
        challenge
            .properties
            .insert("totp.timestep".to_owned(), json!(timestep));
        challenge.response_digest = Some(key.digest(&token)?);
        challenge.response_ttl = Some(now + RESPONSE_TTL);
        challenge.response_token = Some(ResponseToken(token));
        update_challenge(database, challenge, now).await?;
    }

    if plan.validated {
        let log = |action| LogEntry {
            actor: Some(&user_phid),
            session: Some(&plan.viewer.session.key_digest),
            user_phid: user_phid.as_str(),
            action,
            details: Map::new(),
        };
        if plan.verdicts.iter().all(Verdict::is_valid) {
            // `markChallengeAsCompleted()`: no answer is ever taken twice.
            for verdict in &plan.verdicts {
                if let Verdict::CarriedOver { challenge } | Verdict::AnsweredNow { challenge, .. } =
                    verdict
                {
                    let challenge = &mut plan.challenges[*challenge];
                    challenge.is_completed = true;
                    update_challenge(database, challenge, now).await?;
                }
            }
            if plan.any_attempt {
                rate_limit::record(database, user_phid.as_str(), TRY_FACTOR, -1.0, now).await?;
            }
            let partial = plan.viewer.session.is_partial && !plan.jump_into_hisec;
            if partial || !plan.upgrade_session {
                return Ok(HighSecurity::Token);
            }
            let sql = format!(
                "UPDATE {} SET highSecurityUntil = ? WHERE id = ?",
                database.table("user", "phabricator_session")
            );
            phab_db::sqlx::query(&sql)
                .bind(now + HIGH_SECURITY_DURATION)
                .bind(plan.viewer.session.id)
                .execute(database.pool())
                .await?;
            user_log::insert(database, log(LogType::EnterHisec), info, now).await?;
            return Ok(HighSecurity::Token);
        }
        user_log::insert(database, log(LogType::FailHisec), info, now).await?;
    }

    // The prompt, with the tokens of what was answered
    // (`newHTTPParametersFromChallenges()`).
    let mut answered = Vec::new();
    let mut factors = Vec::with_capacity(plan.factors.len());
    for (factor, verdict) in plan.factors.into_iter().zip(plan.verdicts) {
        let state = match verdict {
            Verdict::Shown(state) => state,
            Verdict::Rejected { value, error } => FactorState::Rejected { value, error },
            Verdict::CarriedOver { challenge } | Verdict::AnsweredNow { challenge, .. } => {
                let challenge = &plan.challenges[challenge];
                if let Some(token) = &challenge.response_token {
                    answered.push(format!("{}:{}", challenge.phid.as_str(), token.0));
                }
                FactorState::Answered
            }
        };
        factors.push(PromptFactor { factor, state });
    }
    Ok(HighSecurity::Prompt {
        factors,
        answered: AnsweredTokens(answered.join(" ")),
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn challenge(session: &str, key: i64, ttl: i64) -> Challenge {
        Challenge {
            id: Some(1),
            phid: Phid::new("PHID-CHAL-1"),
            factor_phid: Phid::new("PHID-AFTR-1"),
            session_phid: Phid::new(session),
            workflow_key: LEGACY_WORKFLOW.to_owned(),
            challenge_key: key.to_string(),
            challenge_ttl: ttl,
            response_digest: None,
            response_ttl: None,
            is_completed: false,
            properties: Map::new(),
            response_token: None,
        }
    }

    const NOW: i64 = 1_700_000_010;

    #[test]
    fn challenges_wait_like_php() {
        let session = Phid::new("PHID-SSSN-1");
        let step = totp::timestep(NOW);
        let fresh = challenge("PHID-SSSN-1", step, NOW + 90);
        assert_eq!(wait_reason(&[&fresh], &session, LEGACY_WORKFLOW, NOW), None);

        let other = challenge("PHID-SSSN-2", step, NOW + 30);
        assert_eq!(
            wait_reason(&[&other], &session, LEGACY_WORKFLOW, NOW).unwrap(),
            "This factor recently issued a challenge to a different login session. Wait 31 \
             seconds for the code to cycle, then try again."
        );
        let mut workflow = fresh.clone();
        workflow.workflow_key = "other".to_owned();
        assert!(
            wait_reason(&[&workflow], &session, LEGACY_WORKFLOW, NOW)
                .unwrap()
                .contains("different workflow")
        );
        let old = challenge("PHID-SSSN-1", step - 2, NOW);
        assert_eq!(
            wait_reason(&[&old], &session, LEGACY_WORKFLOW, NOW).unwrap(),
            "This factor recently issued a challenge which has expired. A new challenge can not \
             be issued yet. Wait 1 second for the code to cycle, then try again."
        );
        let mut completed = fresh.clone();
        completed.is_completed = true;
        assert!(
            wait_reason(&[&completed], &session, LEGACY_WORKFLOW, NOW)
                .unwrap()
                .contains("Responses may not be reused")
        );
        // Answered, without the token proving it: a replay.
        let mut answered = fresh.clone();
        answered.response_digest = Some("digest".to_owned());
        assert!(
            wait_reason(&[&answered], &session, LEGACY_WORKFLOW, NOW)
                .unwrap()
                .contains("Responses may not be reused")
        );
        answered.response_token = Some(ResponseToken("t".to_owned()));
        assert_eq!(
            wait_reason(&[&answered], &session, LEGACY_WORKFLOW, NOW),
            None
        );
    }

    fn key() -> TokenKey {
        TokenKey(b"k3y".to_vec())
    }

    #[test]
    fn response_tokens_are_checked_against_their_digest() {
        let mut answered = challenge("PHID-SSSN-1", 1, NOW + 90);
        // PHP: hash_hmac('sha256', 'tok', 'k3y').
        answered.response_digest =
            Some("34ddb100f1f5948578970e38b6b772a21d67321b137b8599c37b48bcfb50c797".to_owned());
        answered.response_ttl = Some(NOW + 60);
        let mut challenges = vec![answered.clone()];
        attach_response_tokens(&mut challenges, Some(" PHID-CHAL-1:tok "), &key(), NOW)
            .unwrap()
            .unwrap();
        assert!(challenges[0].response_token.is_some());

        let mut challenges = vec![answered.clone()];
        assert_eq!(
            attach_response_tokens(&mut challenges, Some("PHID-CHAL-1:forged"), &key(), NOW)
                .unwrap(),
            Err("a forged multi-factor challenge token")
        );
        // Once the response window closed, tokens are ignored.
        let mut challenges = vec![answered];
        attach_response_tokens(&mut challenges, Some("PHID-CHAL-1:tok"), &key(), NOW + 61)
            .unwrap()
            .unwrap();
        assert!(challenges[0].response_token.is_none());
    }

    #[rstest]
    #[case("nocolon", Err("a malformed multi-factor challenge token"))]
    #[case(":tok", Err("a malformed multi-factor challenge token"))]
    #[case("A:1 A:2", Err("a multi-factor challenge token given twice"))]
    #[case("", Ok(()))]
    #[case("PHID-CHAL-9:x", Ok(()))]
    fn token_lists(#[case] field: &str, #[case] expected: Result<(), &'static str>) {
        let mut challenges = vec![challenge("PHID-SSSN-1", 1, NOW + 90)];
        assert_eq!(
            attach_response_tokens(&mut challenges, Some(field), &key(), NOW).unwrap(),
            expected
        );
        assert!(challenges[0].response_token.is_none());
    }

    #[rstest]
    #[case("56666666", 56_666_666)]
    #[case(" 12abc", 12)]
    #[case("abc", 0)]
    #[case("-3", -3)]
    fn challenge_keys_cast_like_php(#[case] key: &str, #[case] expected: i64) {
        assert_eq!(php_int(key), expected);
    }

    #[test]
    fn tokens_are_not_printed() {
        assert_eq!(
            format!("{:?}", AnsweredTokens("PHID-CHAL-1:secret".to_owned())),
            "AnsweredTokens(<redacted>)"
        );
        assert_eq!(
            format!("{:?}", ResponseToken("secret".to_owned())),
            "ResponseToken(<redacted>)"
        );
    }
}
