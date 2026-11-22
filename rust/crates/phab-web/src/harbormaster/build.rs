//! `/harbormaster/build/<id>/` and `/harbormaster/build/<id>/<run>/`: a
//! build, the targets of one of its runs with their artifacts, messages
//! and logs (`HarbormasterBuildViewController`).

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_harbormaster::web::{self as harbormaster, Artifact, Log, Lookup, Step, Target};
use phab_core::{Config, Phid, php};
use phab_remarkup::Mode;
use serde_json::{Map, Value};

use super::status::{self, Status};
use super::{Action, Command, Scope, TransactionTable, method_error};
use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::timeline::Entry;
use crate::viewer::WebUser;
use crate::{diff_html, remarkup, time};

/// The log lines shown unless `?l=` says otherwise.
const DEFAULT_LOG_LINES: i64 = 25;

/// A row of the artifacts table.
pub(crate) struct ArtifactRow {
    key: String,
    type_name: &'static str,
    summary_html: String,
}

/// A row of the messages table.
pub(crate) struct MessageRow {
    id: i64,
    from_html: String,
    kind: String,
    consumed: bool,
    date: String,
}

/// A build log, its last lines and the links choosing how many.
pub(crate) struct LogView {
    title: String,
    uri: String,
    /// The line number and the text of each line.
    lines: Vec<(i64, String)>,
    /// The label of each line count, and its link unless it is the
    /// current one.
    limits: Vec<(String, Option<String>)>,
}

/// A target of the run.
pub(crate) struct TargetView {
    id: i64,
    phid: String,
    name: String,
    links_html: Vec<String>,
    when: String,
    status: Status,
    description: Option<String>,
    step_deleted: bool,
    details: Vec<(String, String)>,
    variables: Vec<(String, String)>,
    artifacts: Vec<ArtifactRow>,
    messages: Vec<MessageRow>,
    logs: Vec<LogView>,
    empty_logs: Vec<LogView>,
}

#[derive(Template)]
#[template(path = "build.html")]
pub(crate) struct BuildPage {
    shell: Shell,
    id: i64,
    status: Status,
    older_run: bool,
    buildable_html: String,
    plan_html: String,
    /// The runs, newest first, and whether each is the one shown.
    history: Vec<(i64, bool)>,
    targets: Vec<TargetView>,
    timeline: Vec<Entry>,
    actions: Vec<Action>,
}

/// `(int)` of a request parameter.
fn php_int(text: &str) -> i64 {
    let text = text.trim_start();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let end = digits
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(digits.len());
    sign * digits[..end].parse::<i64>().unwrap_or(0)
}

/// `AphrontRequest::getInt('l', 25)`.
fn line_limit(query: Option<&str>) -> Result<i64, PageError> {
    let limit = form_urlencoded::parse(query.unwrap_or("").as_bytes())
        .filter(|(key, _)| key == "l")
        .last()
        .map_or(DEFAULT_LOG_LINES, |(_, value)| php_int(&value));
    if limit < 0 {
        return Err(PageError::Fallback("a negative number of log lines"));
    }
    Ok(limit)
}

/// A detail or variable as `PHUIPropertyListView` shows a scalar.
fn scalar(value: &Value) -> Result<String, PageError> {
    Ok(match value {
        Value::Null | Value::Bool(false) => String::new(),
        Value::Bool(true) => "1".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::Array(_) | Value::Object(_) => {
            return Err(PageError::Fallback("a build setting which is not a scalar"));
        }
    })
}

fn properties(map: &Map<String, Value>) -> Result<Vec<(String, String)>, PageError> {
    map.iter()
        .map(|(key, value)| Ok((key.clone(), scalar(value)?)))
        .collect()
}

/// The protocol of a URI, as `PhutilURI` reads it.
fn protocol(uri: &str) -> Result<Option<&str>, PageError> {
    if let Some((scheme, _)) = uri.split_once("://") {
        let valid = scheme.starts_with(|character: char| character.is_ascii_lowercase())
            && scheme.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"+.-".contains(&byte)
            });
        if valid {
            return Ok(Some(scheme));
        }
    }
    if !uri.contains(':') {
        return Ok(None);
    }
    Err(PageError::Fallback("an unusual artifact URI"))
}

/// `HarbormasterURIArtifact::renderLink()`.
fn uri_link(config: &Config, data: &Map<String, Value>) -> Result<String, PageError> {
    let Some(Value::String(uri)) = data.get("uri") else {
        return Err(PageError::Fallback("a URI artifact without URI"));
    };
    let allowed = protocol(uri)?.is_some_and(|protocol| {
        config
            .object("uri.allowed-protocols")
            .and_then(|protocols| protocols.get(protocol))
            .is_some_and(php::is_truthy)
    });
    if !allowed {
        return Ok(diff_html::escape("<Invalid URI>"));
    }
    let name = match data.get("name") {
        None => uri.clone(),
        Some(name) => scalar(name)?,
    };
    Ok(format!(
        "<a href=\"{}\" target=\"_blank\" rel=\"noreferrer\">{}</a>",
        diff_html::escape(uri),
        diff_html::escape(&name)
    ))
}

/// `HarbormasterURIArtifact::isExternalLink()`.
pub(crate) fn is_external_link(artifact: &Artifact) -> bool {
    artifact.kind == "uri" && artifact.data.get("ui.external").is_some_and(php::is_truthy)
}

/// The links of external build systems among a target's artifacts.
pub(crate) fn external_links(
    config: &Config,
    artifacts: &[&Artifact],
) -> Result<Vec<String>, PageError> {
    artifacts
        .iter()
        .filter(|artifact| is_external_link(artifact))
        .map(|artifact| uri_link(config, &artifact.data))
        .collect()
}

/// `renderArtifactSummary()` and `getArtifactTypeName()`.
fn artifact_row(
    config: &Config,
    artifact: &Artifact,
    handles: &HashMap<Phid, super::Handle>,
) -> Result<ArtifactRow, PageError> {
    let (type_name, summary_html) = match artifact.kind.as_str() {
        "uri" => ("URI", uri_link(config, &artifact.data)?),
        // `HarbormasterFileArtifact`: the file's handle.
        "file" => {
            let phid = file_phid(artifact)?;
            ("File", super::handle(handles, &phid).link_html())
        }
        _ => return Err(PageError::Fallback("an artifact of another type")),
    };
    Ok(ArtifactRow {
        key: artifact.key.clone(),
        type_name,
        summary_html,
    })
}

/// The `filePHID` of a file artifact.
fn file_phid(artifact: &Artifact) -> Result<Phid, PageError> {
    match artifact.data.get("filePHID") {
        Some(Value::String(phid)) if phid.starts_with("PHID-FILE-") => Ok(Phid::new(phid.clone())),
        _ => Err(PageError::Fallback("a file artifact without file")),
    }
}

/// When the target was queued, started and completed.
fn when(target: &Target, now: i64) -> String {
    let relative = |duration| phab_app_people::profile::relative_time(duration, 2);
    let started = target.started.filter(|started| *started != 0);
    let mut when = Vec::new();
    if let Some(started) = started {
        if let Some(ended) = target.completed.filter(|completed| *completed != 0) {
            when.push(format!("Completed at {}", time::format_utc(ended)));
            let duration = ended - started;
            if duration == 0 {
                when.push("Built instantly".to_owned());
            } else {
                when.push(format!("Built for {}", relative(duration)));
            }
        } else {
            when.push(format!("Started at {}", time::format_utc(started)));
            let duration = now - started;
            if duration != 0 {
                when.push(format!("Running for {}", relative(duration)));
            }
        }
    } else {
        // PHP formats the start date, which there isn't.
        when.push(format!(
            "Queued at {}",
            time::format_utc(target.started.unwrap_or(0))
        ));
        let duration = now - target.dates.0;
        if duration != 0 {
            when.push(format!("Waiting for {}", relative(duration)));
        }
    }
    when.join(" \u{b7} ")
}

/// A log as `buildLog()` shows it: its last `limit` lines (all of them
/// for 0), and whether it is empty.
async fn log_view<S: WebState>(
    state: &S,
    log: &Log,
    limit: i64,
    base_uri: &str,
) -> Result<(LogView, bool), PageError> {
    let bytes = harbormaster::log_text(state.database(), log.id)
        .await
        .map_err(|error| match error {
            harbormaster::LogError::Chunk(_) => PageError::Fallback("a log PHP can't read"),
            harbormaster::LogError::Db(error) => error.into(),
        })?;
    let text =
        String::from_utf8(bytes).map_err(|_| PageError::Fallback("a log which is not UTF-8"))?;
    let mut lines: Vec<&str> = text
        .split("\r\n")
        .flat_map(|part| part.split(['\r', '\n']))
        .collect();
    let mut start = 1;
    if limit != 0 {
        let count = i64::try_from(lines.len()).unwrap_or(i64::MAX);
        start = count - limit;
        if start >= 1 {
            let keep = usize::try_from(limit).unwrap_or(usize::MAX);
            lines.drain(..lines.len() - keep);
        } else {
            start = 1;
        }
    }
    let is_empty = lines.len() == 1
        && lines[0]
            .trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
            .is_empty();
    let limits = [(25, "25"), (50, "50"), (100, "100"), (0, "Unlimited")]
        .into_iter()
        .map(|(count, label)| {
            let uri = (count != limit).then(|| format!("{base_uri}?l={count}"));
            (label.to_owned(), uri)
        })
        .collect();
    let view = LogView {
        title: format!(
            "Build Log {} ({} - {})",
            log.id,
            log.source.as_deref().unwrap_or(""),
            log.kind.as_deref().unwrap_or("")
        ),
        uri: format!("/harbormaster/log/view/{}/", log.id),
        lines: (start..)
            .zip(lines)
            .map(|(number, line)| (number, line.to_owned()))
            .collect(),
        limits,
    };
    Ok((view, is_empty))
}

/// `HarbormasterBuildTransaction` titles.
fn title(
    transaction: &harbormaster::Transaction,
    author: &str,
) -> Result<Option<String>, PageError> {
    let command = transaction
        .transaction_type
        .strip_prefix("message/")
        .and_then(Command::from_key)
        .ok_or(PageError::Fallback("a build transaction not ported"))?;
    Ok(Some(super::command_title(command, author, "build")))
}

/// The view of each target of the run.
#[allow(clippy::too_many_arguments)]
async fn target_views<S: WebState>(
    state: &S,
    scope: &Scope,
    build_id: i64,
    generation: i64,
    targets: &[Target],
    steps: &HashMap<String, Step>,
    limit: i64,
) -> Result<Vec<TargetView>, PageError> {
    let request = scope.request();
    let target_phids: Vec<Phid> = targets.iter().map(|target| target.phid.clone()).collect();
    // `HarbormasterBuildMessageQuery`: newest first.
    let mut messages = harbormaster::messages(state.database(), &target_phids, false).await?;
    messages.reverse();
    let mut artifacts = harbormaster::artifacts_of_targets(&request, &target_phids)
        .await
        .map_err(method_error)?;
    artifacts.sort_by(|left, right| left.key.cmp(&right.key));
    let mut phids: Vec<Phid> = messages
        .iter()
        .map(|message| message.author_phid.clone())
        .collect();
    for artifact in artifacts.iter().filter(|artifact| artifact.kind == "file") {
        phids.push(file_phid(artifact)?);
    }
    let handles = super::load_handles(state, scope, &phids).await?;
    let logs = harbormaster::logs_of_targets(&request, &target_phids)
        .await
        .map_err(method_error)?;
    let base_uri = format!("/harbormaster/build/{build_id}/{generation}/");

    let mut views = Vec::new();
    for target in targets {
        let target_artifacts: Vec<&Artifact> = artifacts
            .iter()
            .filter(|artifact| artifact.target_phid == target.phid)
            .collect();
        let step = steps.get(&target.step_phid);
        let description = match step.map(|step| step.description.as_str()) {
            Some(text) if !text.is_empty() => Some(
                String::from_utf8_lossy(
                    remarkup::render(state, &scope.viewer(), text, Mode::Html)
                        .await?
                        .as_bytes(),
                )
                .into_owned(),
            ),
            _ => None,
        };
        let mut variables = target.variables.clone();
        variables
            .entry("target.phid".to_owned())
            .or_insert_with(|| Value::String(target.phid.as_str().to_owned()));
        let mut variables = properties(&variables)?;
        variables.sort_by(|left, right| left.0.cmp(&right.0));
        let mut logs_shown = Vec::new();
        let mut empty_logs = Vec::new();
        for log in logs.iter().filter(|log| log.target_phid == target.phid) {
            let (view, is_empty) = log_view(state, log, limit, &base_uri).await?;
            if is_empty {
                empty_logs.push(view);
            } else {
                logs_shown.push(view);
            }
        }
        views.push(TargetView {
            id: target.id,
            phid: target.phid.as_str().to_owned(),
            name: harbormaster::target_name(target),
            links_html: external_links(state.config(), &target_artifacts)?,
            when: when(target, state.now()),
            status: status::target(&target.status),
            description,
            step_deleted: step.is_none(),
            details: properties(&target.details)?,
            variables,
            artifacts: target_artifacts
                .iter()
                .map(|artifact| artifact_row(state.config(), artifact, &handles))
                .collect::<Result<_, _>>()?,
            messages: messages
                .iter()
                .filter(|message| message.receiver_phid == target.phid)
                .map(|message| MessageRow {
                    id: message.id,
                    from_html: super::handle(&handles, &message.author_phid).short_link_html(),
                    kind: message.kind.clone(),
                    consumed: message.is_consumed,
                    date: time::format_utc(message.date_created),
                })
                .collect(),
            logs: logs_shown,
            empty_logs,
        });
    }
    Ok(views)
}

/// `needBuildSteps()`: the steps of the targets the viewer can see, by
/// PHID.
async fn steps_of_targets(
    scope: &Scope,
    targets: &[Target],
) -> Result<HashMap<String, Step>, PageError> {
    let step_phids: Vec<String> = targets
        .iter()
        .map(|target| target.step_phid.clone())
        .collect();
    Ok(harbormaster::steps_by_phid(&scope.request(), &step_phids)
        .await
        .map_err(method_error)?
        .into_iter()
        .map(|step| (step.phid.as_str().to_owned(), step))
        .collect())
}

/// `buildHistoryTable()`: the runs, newest first, when there are several.
fn history(min: i64, max: i64, shown: i64) -> Vec<(i64, bool)> {
    if min == max {
        return Vec::new();
    }
    (min..=max).rev().map(|run| (run, run == shown)).collect()
}

/// The ID in a path segment; PHP's route takes digits.
fn digits(segment: &str) -> Option<i64> {
    (!segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| segment.parse().ok())
        .flatten()
}

/// `GET /harbormaster/build/{id}/`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<BuildPage>, PageError> {
    render(state, viewer, &id, None, query, uri).await
}

/// `GET /harbormaster/build/{id}/{generation}/`.
pub(crate) async fn show_run<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((id, generation)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<BuildPage>, PageError> {
    render(state, viewer, &id, Some(&generation), query, uri).await
}

async fn render<S: WebState>(
    state: Arc<S>,
    viewer: WebUser,
    id: &str,
    generation: Option<&str>,
    query: Option<String>,
    uri: Uri,
) -> Result<Page<BuildPage>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    let id = digits(id).ok_or(PageError::NotFound)?;
    let generation = match generation {
        Some(generation) => Some(digits(generation).ok_or(PageError::NotFound)?),
        None => None,
    };
    let limit = line_limit(query.as_deref())?;
    let scope = Scope::new(state.as_ref(), &viewer);
    let request = scope.request();
    let build = match harbormaster::build(&request, id)
        .await
        .map_err(method_error)?
    {
        Lookup::Visible(build) => build,
        Lookup::Missing | Lookup::Denied => return Err(PageError::NotFound),
    };
    let plans = harbormaster::plans_by_phid(&request, std::slice::from_ref(&build.plan_phid))
        .await
        .map_err(method_error)?;
    let builds = [build];
    let states = super::build_states(state.as_ref(), &scope, &builds, &plans).await?;
    let build_state = &states[0];
    let build = &builds[0];

    let mut max_generation = build.generation;
    let min_generation = i64::from(max_generation != 0);
    if build_state.pending == Some("x-restarting") {
        max_generation += 1;
    }
    let generation = generation.unwrap_or(max_generation);
    if generation < min_generation || generation > max_generation {
        return Err(PageError::NotFound);
    }
    let history = history(min_generation, max_generation, generation);

    let actions = Command::ALL
        .into_iter()
        .map(|command| {
            Ok(Action {
                name: command.build_name(),
                icon: command.icon(),
                uri: format!("/harbormaster/build/{}/{id}/", command.key()),
                enabled: build_state.can_send(command)?,
            })
        })
        .collect::<Result<_, PageError>>()?;

    let targets = harbormaster::targets_of_builds(
        &request,
        std::slice::from_ref(&build.phid),
        Some(generation),
    )
    .await
    .map_err(method_error)?;
    let steps = steps_of_targets(&scope, &targets).await?;
    let targets = target_views(
        state.as_ref(),
        &scope,
        id,
        generation,
        &targets,
        &steps,
        limit,
    )
    .await?;

    let handles = super::load_handles(
        state.as_ref(),
        &scope,
        &[build.buildable_phid.clone(), build.plan_phid.clone()],
    )
    .await?;
    let timeline = super::timeline(
        state.as_ref(),
        &scope,
        TransactionTable::Build,
        &build.phid,
        title,
    )
    .await?;

    let title = format!("Build {id}");
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(BuildPage {
        shell: Shell::for_user(state.config(), &viewer, &title, current),
        id,
        status: status::build(build_state.pending_status()),
        older_run: generation < max_generation,
        buildable_html: super::handle(&handles, &build.buildable_phid).link_html(),
        plan_html: super::handle(&handles, &build.plan_phid).link_html(),
        history,
        targets,
        timeline,
        actions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_limits_like_php() {
        assert_eq!(line_limit(None).unwrap(), 25);
        assert_eq!(line_limit(Some("l=100")).unwrap(), 100);
        assert_eq!(line_limit(Some("l=abc")).unwrap(), 0);
        assert_eq!(line_limit(Some("l=12abc")).unwrap(), 12);
        assert!(line_limit(Some("l=-3")).is_err());
    }

    #[test]
    fn uri_protocols() {
        assert_eq!(protocol("https://x/").unwrap(), Some("https"));
        assert_eq!(protocol("x/y").unwrap(), None);
        assert!(protocol("mailto:a@b").is_err());
    }
}
