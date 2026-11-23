//! The mail of a Differential edit: `buildMail()` of
//! `PhabricatorApplicationTransactionEditor` with the overrides of
//! `DifferentialTransactionEditor`, one `PhabricatorMetaMTAMail` per
//! recipient (`DifferentialReplyHandler::getMailTargets()` with
//! `metamta.one-mail-per-recipient`), each rendered as its recipient sees
//! it (`buildMailForTarget()`, `PhabricatorMailTarget::willSendMail()`).

use std::cmp::Ordering;
use std::collections::HashMap;

use indexmap::IndexSet;
use phab_app_differential::storage;
use phab_app_metamta::actors::{self, Preferences};
use phab_auth::user::User;
use phab_core::{Config, Phid, PhidType, php};
use phab_db::{Database, DbError};
use phab_policy::Viewer;
use phab_remarkup::Mode;
use serde_json::{Map, Value, json};

use crate::Error;
use crate::body::Body;
use crate::handles::{self, Handles};
use crate::html::{self, BR};
use crate::state::State;
use crate::xaction::{self, Xaction};

/// The style of the "View Revision" button.
const BUTTON_STYLE: &str = "text-decoration: none; padding: 4px 8px; margin: 0 8px 8px; \
     float: right; color: #464C5C; font-weight: bold; border-radius: 3px; \
     background-color: #F7F7F9; background-image: linear-gradient(to bottom,#fff,#f1f0f1); \
     display: inline-block; border: 1px solid rgba(71,87,120,.2);";

/// `HeraldAdapter::getAdapterContentType()` of outbound mail rules.
const OUTBOUND_MAIL_ADAPTER: &str = "PhabricatorMailOutboundMailHeraldAdapter";

/// The stamp templates of revisions (`newMailStampTemplates()` of the mail
/// engine extensions, by class name), with their types.
const STAMP_TEMPLATES: &[(&str, &str)] = &[
    ("author", "phid"),
    ("reviewer", "phid"),
    ("blocking-reviewer", "phid"),
    ("resigned-reviewer", "phid"),
    ("revision-repository", "phid"),
    ("revision-status", "phid"),
    ("application", "string"),
    ("monogram", "string"),
    ("phid", "string"),
    ("object-type", "string"),
    ("actor", "phid"),
    ("via", "string"),
    ("silent", "bool"),
    ("encrypted", "bool"),
    ("new", "bool"),
    ("mention", "phid"),
    ("herald", "string"),
    ("removed", "phid"),
    ("tag", "phid"),
    ("subscriber", "phid"),
];

/// The revision as `willPublish()` reloads it, with what its mail shows.
#[derive(Debug, Clone)]
pub(crate) struct Revision {
    /// The row, with its reviewers.
    pub(crate) row: storage::Revision,
    /// `getRepository()`: the monogram and the name.
    pub(crate) repository: Option<(String, String)>,
    /// The branch description of the active diff
    /// (`DifferentialBranchField::getBranchDescription()`).
    pub(crate) branch: Option<String>,
    /// The projects it is tagged with.
    pub(crate) projects: Vec<String>,
}

/// What building the mail needs.
pub(crate) struct Context<'a> {
    pub(crate) database: &'a Database,
    pub(crate) config: &'a Config,
    pub(crate) now: i64,
    pub(crate) revision: &'a Revision,
    pub(crate) xactions: &'a [Xaction],
    pub(crate) state: &'a State,
    /// `getActingAsPHID()`.
    pub(crate) actor: &'a str,
}

/// A mail to save.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Message {
    /// `parameters`, in the order PHP sets them.
    pub(crate) parameters: Map<String, Value>,
    /// `actorPHID`.
    pub(crate) actor_phid: String,
    /// "To" and "Cc", for the recipient edges.
    pub(crate) recipients: Vec<String>,
    /// `buildRecipientList()`: who will get it, for the feed.
    pub(crate) mailed: Vec<String>,
}

/// `PhabricatorEnv::getProductionURI()` of a path.
pub(crate) fn production_uri(config: &Config, path: &str) -> Result<String, Error> {
    let base = match config.value("phabricator.production-uri") {
        Some(value) if php::is_truthy(value) => value
            .as_str()
            .ok_or_else(|| Error::unsupported("a production URI which is not a string"))?,
        _ => config
            .base_uri()
            .map_err(|_| Error::unsupported("mail without phabricator.base-uri"))?,
    };
    Ok(format!("{}{path}", base.trim_end_matches('/')))
}

/// A config option which is true by default.
fn enabled(config: &Config, key: &str) -> bool {
    config.value(key).is_none_or(php::is_truthy)
}

/// A recipient: the user, and whether they are in "To".
struct Target {
    user: User,
    is_to: bool,
}

/// `getMailTargets()`: the users among the expanded recipients who can see
/// the revision, "To" first.
async fn targets(
    cx: &Context<'_>,
    (raw_to, raw_cc): (&[String], &[String]),
    unexpandable: &[String],
) -> Result<Vec<Target>, Error> {
    let all: Vec<String> = raw_to.iter().chain(raw_cc).cloned().collect();
    if all.is_empty() {
        return Ok(Vec::new());
    }
    let map = actors::expansion_map(cx.database, &all).await?;
    let expand = |phids: &[String]| -> Result<IndexSet<String>, Error> {
        let mut result = IndexSet::new();
        for phid in phids {
            let expanded = map
                .get(phid)
                .ok_or_else(|| Error::unsupported("a recipient the member query drops"))?;
            for member in expanded {
                if member != phid && unexpandable.contains(member) {
                    continue;
                }
                result.insert(member.clone());
            }
        }
        Ok(result)
    };
    let to = expand(raw_to)?;
    let cc: IndexSet<String> = expand(raw_cc)?
        .into_iter()
        .filter(|phid| !to.contains(phid))
        .collect();

    let phids: Vec<Phid> = to.iter().chain(&cc).map(Phid::new).collect();
    let mut users: HashMap<String, User> = phab_auth::user::load_users_by_phid(cx.database, &phids)
        .await?
        .into_iter()
        .map(|user| (user.phid.as_str().to_owned(), user))
        .collect();
    // `needUserSettings()` writes the settings cache of users without one.
    let loaded: Vec<&str> = to
        .iter()
        .chain(&cc)
        .map(String::as_str)
        .filter(|phid| users.contains_key(*phid))
        .collect();
    actors::check_settings_caches(cx.database, &loaded).await?;

    let mut targets = Vec::new();
    for (phid, is_to) in to
        .iter()
        .map(|phid| (phid, true))
        .chain(cc.iter().map(|phid| (phid, false)))
    {
        let Some(user) = users.remove(phid) else {
            continue;
        };
        let viewer = Viewer::user(user.phid.clone(), user.is_admin);
        let visible = storage::visible_revisions(
            cx.database,
            cx.config,
            &viewer,
            vec![cx.revision.row.clone()],
            cx.now,
        )
        .await?;
        if !visible.is_empty() {
            targets.push(Target { user, is_to });
        }
    }
    Ok(targets)
}

/// Whether outbound mail rules exist (`runHeraldMailRules()` would run
/// them).
async fn has_outbound_mail_rules(database: &Database) -> Result<bool, Error> {
    let sql = format!(
        "SELECT 1 FROM {} WHERE contentType = ? AND isDisabled = 0 LIMIT 1",
        database.table("herald", "herald_rule")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(OUTBOUND_MAIL_ADAPTER)
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?
        .is_some())
}

/// `buildMail()`: the messages, built without writing.
pub(crate) async fn build(cx: &Context<'_>) -> Result<Vec<Message>, Error> {
    let config = cx.config;
    if !enabled(config, "metamta.one-mail-per-recipient") {
        return Err(Error::unsupported("mail to all recipients at once"));
    }
    if config.truthy("metamta.reply-handler-domain") || config.truthy("metamta.public-replies") {
        return Err(Error::unsupported("mail with reply addresses"));
    }
    let raw_to = cx.state.list("mailToPHIDs")?;
    let mut raw_cc = cx.state.list("mailCCPHIDs")?;
    raw_cc.extend(cx.state.list("heraldEmailPHIDs")?);
    let unexpandable = cx.state.lenient_list("mailUnexpandablePHIDs")?;

    let targets = targets(cx, (&raw_to, &raw_cc), &unexpandable).await?;
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    // `buildMailForTarget()` sends nothing when every transaction is
    // hidden from mail, whoever the recipient.
    let mut any_visible = false;
    for xaction in cx.xactions {
        if !xaction.hidden_for_mail()? {
            any_visible = true;
            break;
        }
    }
    if !any_visible {
        return Ok(Vec::new());
    }
    if has_outbound_mail_rules(cx.database).await? {
        return Err(Error::unsupported("outbound mail Herald rules"));
    }
    if config.value("differential.fields").is_some() {
        return Err(Error::unsupported("custom differential.fields"));
    }
    if cx
        .xactions
        .iter()
        .any(|xaction| xaction.row.transaction_type == "differential:update")
    {
        return Err(Error::unsupported("mail about a diff update"));
    }
    if cx
        .xactions
        .iter()
        .any(|xaction| xaction.comment.is_some() && xaction.row.transaction_type != "core:comment")
    {
        return Err(Error::unsupported(
            "a comment on a transaction other than a comment",
        ));
    }

    let user_phids: Vec<&str> = targets
        .iter()
        .map(|target| target.user.phid.as_str())
        .collect();
    let global = phab_app_settings::preferences::load_global(cx.database).await?;
    let preferences = actors::user_preferences(cx.database, &user_phids, global.as_ref()).await?;

    let mut messages = Vec::new();
    for target in &targets {
        let preferences = preferences
            .get(target.user.phid.as_str())
            .cloned()
            .unwrap_or_default();
        messages.push(message(cx, target, &preferences, (&raw_to, &raw_cc)).await?);
    }
    Ok(messages)
}

/// The PHIDs the handles of a mail are loaded for.
fn handle_phids(cx: &Context<'_>, raw: (&[String], &[String])) -> Vec<String> {
    let mut phids: Vec<String> = Vec::new();
    for xaction in cx.xactions {
        phids.push(xaction.row.author_phid.as_str().to_owned());
        for value in [&xaction.row.old_value, &xaction.row.new_value] {
            match value {
                Value::Array(items) => {
                    phids.extend(items.iter().filter_map(Value::as_str).map(str::to_owned));
                }
                Value::Object(entries) => phids.extend(entries.keys().cloned()),
                _ => {}
            }
        }
    }
    phids.extend(raw.0.iter().cloned());
    phids.extend(raw.1.iter().cloned());
    phids.extend(cx.revision.projects.iter().cloned());
    if let Value::Array(stamps) = cx.state.value("mailStamps") {
        for stamp in stamps {
            match stamp.get("value") {
                Some(Value::String(phid)) => phids.push(phid.clone()),
                Some(Value::Array(items)) => {
                    phids.extend(items.iter().filter_map(Value::as_str).map(str::to_owned));
                }
                _ => {}
            }
        }
    }
    phids.retain(|phid| phid.starts_with("PHID-"));
    phids.sort();
    phids.dedup();
    phids
}

/// `array_unique()` of a list: the first of each value keeps its index, so
/// a list with gaps becomes a dictionary.
fn unique_keeping_keys(values: &[String]) -> Value {
    let mut seen: Vec<&String> = Vec::new();
    let mut kept = Map::new();
    for (index, value) in values.iter().enumerate() {
        if !seen.contains(&value) {
            seen.push(value);
            kept.insert(index.to_string(), json!(value));
        }
    }
    php::round_trip(&Value::Object(kept))
}

/// Renders a comment for text mail and for HTML mail, as
/// `addRemarkupSection()` does: Rust renders what refers to nothing in the
/// database.
fn render_remarkup(config: &Config, text: &str) -> Result<(String, String), Error> {
    let engine_config = phab_remarkup::Config {
        uri_base: Some(production_uri(config, "/")?),
        ..phab_app_differential::markup::engine_config(config)
    };
    let engine = phab_remarkup::phabricator::engine(engine_config);
    let mut rendered = Vec::new();
    for mode in [Mode::Text, Mode::HtmlMail] {
        let prepared = engine.prepare(text.as_bytes(), mode);
        if !prepared.object_references().is_empty()
            || !prepared.mentioned_usernames().is_empty()
            || !prepared.handle_phids().is_empty()
            || !prepared.repository_identifiers().is_empty()
        {
            return Err(Error::unsupported(
                "a comment which mentions or references objects",
            ));
        }
        let output = engine
            .finish(prepared, phab_remarkup::Resolved::default())
            .output;
        rendered.push(
            String::from_utf8(output.into_bytes())
                .map_err(|_| Error::unsupported("a comment rendering to invalid UTF-8"))?,
        );
    }
    let html = rendered.pop().unwrap_or_default();
    let text = rendered.pop().unwrap_or_default();
    if config.truthy("pygments.enabled") && html.contains("<pre") {
        return Err(Error::unsupported("a code block highlighted by Pygments"));
    }
    if config
        .value("syntax-highlighter.engine")
        .is_some_and(|engine| engine != "PhutilDefaultSyntaxHighlighterEngine")
    {
        return Err(Error::unsupported("a custom syntax highlighter"));
    }
    Ok((text, html))
}

/// `buildMailBody()` of `DifferentialTransactionEditor`, with
/// `addHeadersAndCommentsToMailBody()` and the transaction mail of the
/// custom fields.
fn mail_body(cx: &Context<'_>, handles: &Handles) -> Result<Body, Error> {
    let revision = &cx.revision.row;
    let revision_uri = production_uri(cx.config, &format!("/D{}", revision.id))?;
    let new_uri = format!("{revision_uri}/new/");
    let name =
        |phid: &str| -> Result<String, Error> { Ok(handles.require(phid)?.link_name.clone()) };
    let title_html = |title: &xaction::Title| {
        if title.is_html {
            title.text.clone()
        } else {
            html::escape(&title.text)
        }
    };

    let mut body = Body::default();
    let mut headers = Vec::new();
    let mut headers_html = Vec::new();
    let mut comments = Vec::new();
    let mut seen_comment = false;
    for xaction in cx.xactions {
        if xaction.hidden_for_mail()? {
            continue;
        }
        let comment = xaction.mail_body();
        if let Some(comment) = comment {
            comments.push((xaction, comment, !seen_comment));
        }
        if (comment.is_none() || !seen_comment)
            && let Some(title) = xaction.mail_title(&name)?
        {
            headers_html.push(title_html(&title));
            headers.push(title.text);
        }
        if comment.is_some() {
            seen_comment = true;
        }
    }
    body.add_raw_plaintext(&headers.join("\n"));
    let button = html::tag(
        "a",
        &[("style", BUTTON_STYLE), ("href", &revision_uri)],
        "View Revision",
    );
    let cell = html::tag(
        "td",
        &[("style", "")],
        &format!("{}\n", headers_html.join(BR)),
    );
    body.add_raw_html(html::tag(
        "table",
        &[],
        &html::tag("tr", &[], &format!("{cell}{button}")),
    ));
    for (xaction, comment, is_initial) in comments {
        if !is_initial && let Some(title) = xaction.mail_title(&name)? {
            body.add_raw_plaintext(&title.text);
            body.add_raw_html(title_html(&title));
        }
        let (text, html) = render_remarkup(cx.config, comment)?;
        body.add_remarkup(None, &text, &html);
    }

    // The fields of `differential.fields` which add to transaction mail,
    // in their default order: DifferentialRepositoryField, then
    // DifferentialBranchField (shown on accepted revisions, as there is no
    // update here). Summary and test plan only show in the first
    // broadcast, the changes since the last update only with an update.
    if let Some((monogram, name)) = &cx.revision.repository {
        body.add_text("REPOSITORY", &format!("{monogram} {name}"));
    }
    if revision.status == "accepted"
        && let Some(branch) = &cx.revision.branch
    {
        body.add_text("BRANCH", branch);
    }
    body.add_link("CHANGES SINCE LAST ACTION", &new_uri);
    body.add_link("REVISION DETAIL", &revision_uri);
    Ok(body)
}

/// `natcasesort()`'s comparison: `strnatcmp_ex()` folding case.
fn natural_compare(a: &[u8], b: &[u8]) -> Ordering {
    if a.is_empty() || b.is_empty() {
        return a.len().cmp(&b.len());
    }
    let at = |text: &[u8], index: usize| text.get(index).copied().unwrap_or(0);
    let (mut ai, mut bi) = (0, 0);
    let mut leading = true;
    loop {
        let mut ca = at(a, ai);
        let mut cb = at(b, bi);
        while leading && ca == b'0' && ai + 1 < a.len() && at(a, ai + 1).is_ascii_digit() {
            ai += 1;
            ca = at(a, ai);
        }
        while leading && cb == b'0' && bi + 1 < b.len() && at(b, bi + 1).is_ascii_digit() {
            bi += 1;
            cb = at(b, bi);
        }
        leading = false;
        while matches!(ca, b' ' | b'\t' | b'\n' | b'\r' | b'\x0B' | b'\x0C') {
            ai += 1;
            ca = at(a, ai);
        }
        while matches!(cb, b' ' | b'\t' | b'\n' | b'\r' | b'\x0B' | b'\x0C') {
            bi += 1;
            cb = at(b, bi);
        }
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let fractional = ca == b'0' || cb == b'0';
            let result = compare_digits(a, &mut ai, b, &mut bi, fractional);
            if result != Ordering::Equal {
                return result;
            }
            match (ai >= a.len(), bi >= b.len()) {
                (true, true) => return Ordering::Equal,
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                (false, false) => {
                    ca = at(a, ai);
                    cb = at(b, bi);
                }
            }
        }
        let (ca, cb) = (ca.to_ascii_uppercase(), cb.to_ascii_uppercase());
        match ca.cmp(&cb) {
            Ordering::Equal => {}
            other => return other,
        }
        ai += 1;
        bi += 1;
        match (ai >= a.len(), bi >= b.len()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }
    }
}

/// `compare_left()` (fractional) and `compare_right()` of `strnatcmp.c`.
fn compare_digits(
    a: &[u8],
    ai: &mut usize,
    b: &[u8],
    bi: &mut usize,
    fractional: bool,
) -> Ordering {
    let digit = |text: &[u8], index: usize| text.get(index).is_some_and(u8::is_ascii_digit);
    let mut bias = Ordering::Equal;
    loop {
        match (digit(a, *ai), digit(b, *bi)) {
            (false, false) => return bias,
            (false, true) => return Ordering::Less,
            (true, false) => return Ordering::Greater,
            (true, true) => {
                let order = a[*ai].cmp(&b[*bi]);
                if fractional {
                    if order != Ordering::Equal {
                        return order;
                    }
                } else if bias == Ordering::Equal {
                    bias = order;
                }
            }
        }
        *ai += 1;
        *bi += 1;
    }
}

/// `natcasesort()`: the values sorted, each keeping its index; a list when
/// nothing moved, a dictionary otherwise (as `json_encode()` writes it).
fn natcasesort(values: Vec<String>) -> Value {
    let mut indexed: Vec<(usize, String)> = values.into_iter().enumerate().collect();
    indexed.sort_by(|(_, a), (_, b)| natural_compare(a.as_bytes(), b.as_bytes()));
    let map: Map<String, Value> = indexed
        .into_iter()
        .map(|(index, value)| (index.to_string(), Value::String(value)))
        .collect();
    php::round_trip(&Value::Object(map))
}

/// A stamp value as a string (`'('.$value.')'`).
fn stamp_string(value: &Value) -> Result<String, Error> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        _ => Err(Error::unsupported(
            "a mail stamp value which is not a string",
        )),
    }
}

/// `generateMailStamps()`: the templates get the values of the state, then
/// render as the recipient sees the handles.
fn render_stamps(specs: &Value, handles: &Handles) -> Result<Option<Value>, Error> {
    let Value::Array(specs) = specs else {
        if php::is_truthy(specs) {
            return Err(Error::unsupported("mail stamps which are not a list"));
        }
        return Ok(None);
    };
    if specs.is_empty() {
        return Ok(None);
    }
    let mut values: Vec<Value> = vec![Value::Null; STAMP_TEMPLATES.len()];
    for spec in specs {
        let Value::Object(spec) = spec else {
            continue;
        };
        let key = spec.get("key").and_then(Value::as_str);
        let Some(index) = STAMP_TEMPLATES
            .iter()
            .position(|(template, _)| Some(*template) == key)
        else {
            continue;
        };
        if spec.get("type").and_then(Value::as_str) != Some(STAMP_TEMPLATES[index].1) {
            continue;
        }
        values[index] = spec.get("value").cloned().unwrap_or(Value::Null);
    }

    let mut results = Vec::new();
    for ((key, kind), value) in STAMP_TEMPLATES.iter().zip(values) {
        // `(array)$value`.
        let list = |value: &Value| -> Vec<Value> {
            match value {
                Value::Array(items) => items.clone(),
                Value::Object(entries) => entries.values().cloned().collect(),
                other => vec![other.clone()],
            }
        };
        match *kind {
            "bool" => {
                if php::is_truthy(&value) {
                    results.push(format!("{key}()"));
                }
            }
            "string" => {
                if value.is_null() || value == "" {
                    continue;
                }
                for item in list(&value) {
                    results.push(format!("{key}({})", stamp_string(&item)?));
                }
            }
            _ => {
                if value.is_null() {
                    continue;
                }
                for item in list(&value) {
                    if item.is_array() || item.is_object() {
                        continue;
                    }
                    let phid = stamp_string(&item)?;
                    if Phid::new(phid.as_str()).phid_type() == PhidType::SPACE {
                        return Err(Error::unsupported("a space in mail stamps"));
                    }
                    let name = handles
                        .get(&phid)
                        .and_then(|handle| handle.mail_stamp_name.clone())
                        .unwrap_or(phid);
                    results.push(format!("{key}({name})"));
                }
            }
        }
    }
    if results.is_empty() {
        return Ok(None);
    }
    Ok(Some(natcasesort(results)))
}

/// The names of the handles of `phids`, each PHID once (`loadHandles()`
/// through `iterator_to_array()`).
fn recipient_names(
    handles: &Handles,
    phids: &[String],
    name: impl Fn(&handles::Handle) -> String,
) -> Result<Vec<String>, Error> {
    let mut seen: Vec<&String> = Vec::new();
    let mut names = Vec::new();
    for phid in phids {
        if seen.contains(&phid) {
            continue;
        }
        seen.push(phid);
        names.push(name(handles.require(phid)?));
    }
    Ok(names)
}

/// The parameters `buildMailForTarget()` sets, in its order, with the
/// headers so far.
fn target_parameters(
    cx: &Context<'_>,
    handles: &Handles,
    body: &Body,
    stamps: Option<&Value>,
) -> Result<(Map<String, Value>, Vec<Value>), Error> {
    let state = cx.state;
    let revision = &cx.revision.row;
    let tags: Vec<String> = cx
        .xactions
        .iter()
        .map(|xaction| xaction.mail_tags().to_owned())
        .collect();
    let action = xaction::strongest(cx.xactions)?
        .ok_or_else(|| Error::unsupported("mail without transactions"))?
        .action_name()?;

    let mut parameters = Map::new();
    let mut set = |key: &str, value: Value| {
        parameters.insert(key.to_owned(), value);
    };
    set("sensitive", json!(true));
    set("mustEncrypt", json!(false));
    // buildMailTemplate().
    set(
        "subject",
        json!(format!("D{}: {}", revision.id, revision.title)),
    );
    set(
        "mustEncrypt.subject",
        json!(format!("D{}: Revision Updated", revision.id)),
    );
    set("mustEncrypt.uri", json!(format!("/D{}", revision.id)));
    set("sensitive", json!(false));
    set("from", json!(cx.actor));
    set("subject-prefix", json!("[Differential]"));
    set("vary-subject-prefix", json!(format!("[{action}]")));
    set(
        "thread-id",
        json!(format!("differential-rev-{}-req", revision.phid.as_str())),
    );
    set(
        "is-first-message",
        php::round_trip(state.value("isNewObject")),
    );
    set("exclude", json!(state.exclude()?));
    set("muted", json!(state.lenient_list("mailMutedPHIDs")?));
    set(
        "herald-force-recipients",
        json!(state.list("heraldForcedEmailPHIDs")?),
    );
    set("mailtags", unique_keeping_keys(&tags));
    set("is-bulk", json!(true));
    set("body", json!(body.render()));
    set("html-body", json!(body.render_html()));
    let mut headers: Vec<Value> = Vec::new();
    if state.flag("heraldHeader") {
        headers.push(json!(["X-Herald-Rules", state.value("heraldHeader")]));
    }
    // addMailProjectMetadata().
    let project_tags: Vec<String> = cx
        .revision
        .projects
        .iter()
        .filter_map(|phid| handles.get(phid))
        .filter(|handle| handle.complete)
        .map(|handle| format!("<{}>", handle.object_name))
        .collect();
    if !project_tags.is_empty() {
        headers.push(json!(["X-Phabricator-Projects", project_tags.join(", ")]));
    }
    if !headers.is_empty() {
        set("headers", Value::Array(headers.clone()));
    }
    if state.flag("parentMessageID") {
        set(
            "parent-message-id",
            php::round_trip(state.value("parentMessageID")),
        );
    }
    if state.flag("mailStamps") {
        set("stampMetadata", php::round_trip(state.value("mailStamps")));
    }
    if let Some(stamps) = stamps {
        set("stamps", stamps.clone());
    }
    Ok((parameters, headers))
}

/// The stamps appended to the body when the recipient wants them there.
fn append_stamps(stamps: &Value, text: &mut String, html_body: &mut String) {
    let list: Vec<String> = match stamps {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Value::Object(entries) => entries
            .values()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    text.push_str("\nSTAMPS\n");
    text.push_str(&list.join(" "));
    text.push('\n');
    let escaped: Vec<String> = list.iter().map(|stamp| html::escape(stamp)).collect();
    let span = html::tag(
        "span",
        &[("style", "font-size: smaller; color: #92969D")],
        &escaped.join(" "),
    );
    let strong = html::tag("strong", &[], "STAMPS");
    html_body.push_str(&html::tag(
        "div",
        &[],
        &format!("{strong}{BR}{span}{BR}{BR}"),
    ));
}

/// `getRecipientsSummary()` and `getRecipientsSummaryHTML()`.
fn recipients_summary(
    handles: &Handles,
    (raw_to, raw_cc): (&[String], &[String]),
) -> Result<(String, String), Error> {
    let mut text = String::new();
    let mut summary = String::new();
    for (label, phids) in [("To: ", raw_to), ("Cc: ", raw_cc)] {
        let names = recipient_names(handles, phids, |handle| handle.command_line_name.clone())?;
        if !names.is_empty() {
            text.push_str(label);
            text.push_str(&names.join(", "));
            text.push('\n');
        }
    }
    for (label, phids) in [("To: ", raw_to), ("Cc: ", raw_cc)] {
        let names = recipient_names(handles, phids, |handle| html::escape(&handle.name))?;
        if !names.is_empty() {
            summary.push_str(&html::tag("strong", &[], label));
            summary.push_str(&names.join(", "));
            summary.push_str(BR);
        }
    }
    Ok((text, html::tag("div", &[], &summary)))
}

/// `willSendMail()`: the stamps in the body if the recipient wants them,
/// the recipient headers and summary, and the recipient.
fn will_send_mail(
    (parameters, mut headers): (&mut Map<String, Value>, Vec<Value>),
    (target, preferences, config): (&Target, &Preferences, &Config),
    handles: &Handles,
    raw: (&[String], &[String]),
) -> Result<(), Error> {
    let string = |key: &str| {
        parameters
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let mut text = string("body");
    let mut html_body = string("html-body");
    let show_stamps =
        phab_app_metamta::php::loose_eq_str(&preferences.value("stamps", config), "body");
    if show_stamps && let Some(stamps) = parameters.get("stamps") {
        append_stamps(stamps, &mut text, &mut html_body);
    }
    for (name, phids) in [("X-Phabricator-To", raw.0), ("X-Phabricator-Cc", raw.1)] {
        let mut seen: Vec<&String> = Vec::new();
        for phid in phids {
            if !seen.contains(&phid) {
                seen.push(phid);
                headers.push(json!([name, format!("<{phid}>")]));
            }
        }
    }
    if !headers.is_empty() {
        parameters.insert("headers".to_owned(), Value::Array(headers));
    }
    text.push('\n');
    if enabled(config, "metamta.recipients.show-hints") {
        let (summary, summary_html) = recipients_summary(handles, raw)?;
        text.push_str(&summary);
        html_body.push_str(&summary_html);
    }
    parameters.insert("body".to_owned(), json!(text));
    parameters.insert("html-body".to_owned(), json!(html_body));
    let key = if target.is_to { "to" } else { "cc" };
    parameters.insert(key.to_owned(), json!([target.user.phid.as_str()]));
    Ok(())
}

/// One recipient's mail.
async fn message(
    cx: &Context<'_>,
    target: &Target,
    preferences: &Preferences,
    raw: (&[String], &[String]),
) -> Result<Message, Error> {
    let config = cx.config;
    let translation = preferences.value(phab_app_settings::setting::TRANSLATION, config);
    if php::is_truthy(&translation) && translation != "en_US" {
        return Err(Error::unsupported("mail in a language other than English"));
    }
    let viewer = Viewer::user(target.user.phid.clone(), target.user.is_admin);
    let handles =
        handles::load(cx.database, config, &viewer, &handle_phids(cx, raw), cx.now).await?;

    let mut body = mail_body(cx, &handles)?;
    let stamps = render_stamps(cx.state.value("mailStamps"), &handles)?;
    if enabled(config, "metamta.email-preferences") {
        body.add_link(
            "EMAIL PREFERENCES",
            &production_uri(config, "/settings/panel/emailpreferences/")?,
        );
    }
    let (mut parameters, headers) = target_parameters(cx, &handles, &body, stamps.as_ref())?;
    will_send_mail(
        (&mut parameters, headers),
        (target, preferences, config),
        &handles,
        raw,
    )?;
    // buildMailWithRecipients().
    if cx.state.flag("mustEncrypt") {
        parameters.insert("mustEncrypt".to_owned(), json!(true));
        parameters.insert(
            "mustEncryptReasons".to_owned(),
            json!(cx.state.list("mustEncrypt")?),
        );
    }

    let mailed = mailed(cx, &parameters).await?;
    Ok(Message {
        parameters,
        actor_phid: cx.actor.to_owned(),
        recipients: vec![target.user.phid.as_str().to_owned()],
        mailed,
    })
}

/// `buildRecipientList()` of a built mail: its deliverable actors.
async fn mailed(cx: &Context<'_>, parameters: &Map<String, Value>) -> Result<Vec<String>, Error> {
    let mail = phab_app_metamta::mail::Mail {
        id: 0,
        phid: String::new(),
        actor_phid: Some(cx.actor.to_owned()),
        parameters: parameters.clone(),
        status: phab_app_metamta::mail::STATUS_QUEUE.to_owned(),
        message: None,
        related_phid: Some(cx.revision.row.phid.as_str().to_owned()),
    };
    let parties = actors::Parties::of(&mail)?;
    let recipients = actors::load(cx.database, cx.config, &mail, &parties).await?;
    Ok(recipients
        .actors
        .values()
        .filter(|actor| actor.is_deliverable())
        .map(|actor| actor.phid.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_naturally_ignoring_case() {
        let sorted = natcasesort(
            ["b(1)", "a10", "A9", "a(x)", "B(0)"]
                .map(str::to_owned)
                .to_vec(),
        );
        assert_eq!(
            sorted,
            json!({"3": "a(x)", "2": "A9", "1": "a10", "4": "B(0)", "0": "b(1)"})
        );
        assert_eq!(
            natcasesort(vec!["a".to_owned(), "b".to_owned()]),
            json!(["a", "b"])
        );
    }

    #[test]
    fn keeps_the_keys_of_unique_values() {
        let tags = ["a", "b", "a", "c"].map(str::to_owned);
        assert_eq!(
            unique_keeping_keys(&tags),
            json!({"0": "a", "1": "b", "3": "c"})
        );
        assert_eq!(unique_keeping_keys(&tags[..2]), json!(["a", "b"]));
    }
}
