//! `/source/<x>/manage/[<panel>/]` (`DiffusionRepositoryManagePanelsController`):
//! the management panels of a repository, read-only: Basics, Policies,
//! Storage, Limits, Branches, Staging Area and Symbols, with the links of
//! their curtains, whose forms and actions are PHP's.
//!
//! Left to PHP: the URIs panel (the builtin URIs), History (the
//! repository's transactions), repositories served by a cluster and
//! installs with Spaces.

use std::fmt::Write as _;
use std::path::Path;

use askama::Template;
use axum::response::{IntoResponse, Response};
use phab_app_differential::repository::{self, Repository};
use phab_core::Phid;
use phab_core::php::is_truthy;
use phab_db::rows;
use phab_remarkup::Mode;
use serde_json::Value;

use super::Context;
use super::list::{thousands, vcs_name};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::harbormaster::{self, internal};
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{remarkup, time};

/// `PhutilDaemonHandle::getHeartbeatEventFrequency()` times 30: daemons
/// not seen since are dead (`PhabricatorDaemonLogQuery`).
const DAEMON_DEAD_AFTER: i64 = 30 * 120;

/// A link of the side navigation.
pub(crate) struct NavItem {
    /// The label of the group the item starts, if any.
    group: Option<&'static str>,
    name: &'static str,
    icon: String,
    href: String,
    selected: bool,
}

/// An action of the curtain.
pub(crate) struct Action {
    name: &'static str,
    icon: &'static str,
    href: String,
    disabled: bool,
    /// A divider before the action.
    divider: bool,
}

/// A box of a panel (`PHUIObjectBoxView`).
#[derive(Default)]
pub(crate) struct PanelBox {
    title: String,
    notice_html: Option<String>,
    properties: Vec<(&'static str, String)>,
    /// A section header and its text, after the properties.
    sections: Vec<(&'static str, String)>,
    text_html: Option<String>,
    table: Option<Table>,
}

/// An `AphrontTableView`.
pub(crate) struct Table {
    headers: Vec<&'static str>,
    rows: Vec<Vec<String>>,
    empty: &'static str,
}

#[derive(Template)]
#[template(path = "diffusion_manage.html")]
pub(crate) struct ManagePage {
    shell: crate::Shell,
    name: String,
    href: String,
    active: bool,
    help_href: String,
    nav: Vec<NavItem>,
    notice_html: Option<String>,
    boxes: Vec<PanelBox>,
    actions: Vec<Action>,
}

/// `PhabricatorEnv::getDoclink()`.
pub(crate) fn doclink(name: &str) -> String {
    let mut query = form_urlencoded::Serializer::new(String::new());
    query.append_pair("name", name);
    query.append_pair("type", "article");
    query.append_pair("jump", "1");
    format!(
        "https://secure.phabricator.com/diviner/find/?{}",
        query.finish()
    )
}

/// `<em>` of a text.
fn em(text: &str) -> String {
    format!("<em>{}</em>", escape(text))
}

/// A `PHUIStatusItemView`: its icon, color, target and note, as HTML.
fn status_item(icon: &str, color: &str, target_html: &str, note_html: Option<&str>) -> String {
    let mut html = format!(
        "<li><span class=\"fa {icon} remarkup-icon-{color}\" aria-hidden=\"true\"></span> \
         {target_html}"
    );
    if let Some(note) = note_html {
        let _ = write!(html, " <span class=\"muted\">{note}</span>");
    }
    html.push_str("</li>");
    html
}

/// A `PHUIStatusListView` of items.
fn status_list(items: &[String]) -> String {
    format!("<ul class=\"build-status\">{}</ul>", items.concat())
}

/// The keys of a detail which maps rules to `true`.
fn keys(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Object(map)) => map.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

/// The strings of a list detail.
fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(list)) => list
            .iter()
            .map(|item| match item {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect(),
        Some(Value::Object(map)) => map
            .values()
            .map(|item| {
                item.as_str()
                    .map_or_else(|| item.to_string(), str::to_owned)
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The detail as a positive integer, as `if ($limit)` reads it.
fn positive(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|number| *number != 0)
}

/// What the panels read of the repository beyond `Repository`.
struct Storage {
    local_path: String,
    date_created: i64,
    service: Option<String>,
}

async fn storage<S: WebState>(state: &S, repository: &Repository) -> Result<Storage, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT localPath, dateCreated, almanacServicePHID FROM {} WHERE id = ?",
        database.table("repository", "repository")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(repository.id)
        .fetch_one(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    Ok(Storage {
        local_path: rows::opt_text(&row, "localPath")?.unwrap_or_default(),
        date_created: rows::int(&row, "dateCreated")?,
        service: rows::opt_text(&row, "almanacServicePHID")?.filter(|phid| !phid.is_empty()),
    })
}

/// A `repository_statusmessage`.
struct StatusMessage {
    code: String,
    parameters: Value,
    epoch: i64,
}

/// `loadStatusMessages()`: the last message of each type.
async fn status_messages<S: WebState>(
    state: &S,
    repository: &Repository,
) -> Result<Vec<(String, StatusMessage)>, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT statusType, statusCode, parameters, epoch FROM {} WHERE repositoryID = ? \
         ORDER BY id",
        database.table("repository", "repository_statusmessage")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(repository.id)
        .fetch_all(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    let mut messages: Vec<(String, StatusMessage)> = Vec::new();
    for row in &found {
        let kind = rows::text(row, "statusType")?;
        let message = StatusMessage {
            code: rows::text(row, "statusCode")?,
            parameters: rows::json(row, "parameters").unwrap_or(Value::Null),
            epoch: rows::int(row, "epoch")?,
        };
        messages.retain(|(known, _)| *known != kind);
        messages.push((kind, message));
    }
    Ok(messages)
}

fn message<'a>(messages: &'a [(String, StatusMessage)], kind: &str) -> Option<&'a StatusMessage> {
    messages
        .iter()
        .find(|(known, _)| known == kind)
        .map(|(_, message)| message)
}

/// The `message` parameter of a status message.
fn message_text(message: &StatusMessage) -> String {
    match message.parameters.get("message") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// `loadUpdateInterval()`: how long the pull daemon waits between
/// updates of the repository.
async fn update_interval<S: WebState>(
    state: &S,
    repository: &Repository,
    storage: &Storage,
) -> Result<i64, PageError> {
    const MINIMUM: i64 = 15;
    let database = state.database();
    let sql = format!(
        "SELECT MAX(messageCount) error_count FROM {} WHERE repositoryID = ? \
         AND statusType IN ('init', 'fetch') AND statusCode IN ('error')",
        database.table("repository", "repository_statusmessage")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(repository.id)
        .fetch_one(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    let errors = rows::opt_int(&row, "error_count")?.unwrap_or(0);
    if errors > 0 {
        return Ok(MINIMUM * errors);
    }
    if repository.detail("importing").is_some_and(is_truthy) {
        return Ok(MINIMUM);
    }
    let window_start = state.now() + MINIMUM;
    let sql = format!(
        "SELECT epoch FROM {} WHERE repositoryID = ? AND epoch <= ? ORDER BY epoch DESC LIMIT 1",
        database.table("repository", "repository_commit")
    );
    let last = phab_db::sqlx::query(&sql)
        .bind(repository.id)
        .bind(window_start)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    let since = match last {
        Some(row) => window_start - rows::int(&row, "epoch")?,
        None => window_start - storage.date_created,
    };
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let wait = if since <= 3 * 86_400 {
        since as f64 / 600.0
    } else {
        since as f64 / 240.0
    }
    .min(6.0 * 3600.0)
    .max(MINIMUM as f64) as i64;
    Ok(wait)
}

/// `loadImportProgress()`.
async fn import_progress<S: WebState>(
    state: &S,
    repository: &Repository,
) -> Result<f64, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT importStatus, COUNT(*) N FROM {} WHERE repositoryID = ? GROUP BY importStatus",
        database.table("repository", "repository_commit")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(repository.id)
        .fetch_all(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    let (mut done, mut total) = (0_i64, 0_i64);
    for row in &found {
        let count = rows::int(row, "N")?;
        let status = rows::int(row, "importStatus")?;
        total += count * 3;
        for flag in [1, 2, 8] {
            if status & flag != 0 {
                done += count;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = if total == 0 {
        0.0
    } else {
        done as f64 / total as f64
    };
    Ok(ratio.min(0.9999))
}

/// Whether a daemon of the class is alive.
async fn daemon_alive<S: WebState>(state: &S, class: &str) -> Result<bool, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT 1 FROM {} WHERE daemon = ? AND status IN ('unknown', 'run', 'wait', 'exiting') \
         AND dateModified >= ? LIMIT 1",
        database.table("daemon", "daemon_log")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(class)
        .bind(state.now() - DAEMON_DEAD_AFTER)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .is_some())
}

/// `Filesystem::resolveBinary()`: the binary in the `PATH`.
fn resolve_binary(config: &phab_core::Config, binary: &str) -> Option<String> {
    let mut directories: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(str::to_owned)
        .collect();
    if let Some(Value::Array(append)) = config.value("environment.append-paths") {
        directories.extend(append.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    directories
        .into_iter()
        .filter(|directory| !directory.is_empty())
        .map(|directory| format!("{}/{binary}", directory.trim_end_matches('/')))
        .find(|path| Path::new(path).is_file())
}

/// `buildStateView()`.
fn state_box(repository: &Repository) -> PanelBox {
    let is_new = repository
        .detail("newly-initialized")
        .is_some_and(is_truthy);
    let tracked = repository.is_tracked();
    let importing = repository.detail("importing").is_some_and(is_truthy);
    let (icon, color, label, note) = if !tracked {
        if is_new {
            (
                "fa-ban",
                "yellow",
                "Not Activated Yet",
                Some("Complete Setup and Activate Repository"),
            )
        } else {
            ("fa-times", "red", "Not Active", Some("Repository Disabled"))
        }
    } else if importing {
        ("fa-hourglass", "yellow", "Importing...", None)
    } else {
        ("fa-check", "green", "Repository Active", None)
    };
    let active = status_list(&[status_item(icon, color, &escape(label), note)]);
    let (icon, color, label, note) = if repository.detail("herald-disabled").is_some_and(is_truthy)
    {
        (
            "fa-times",
            "red",
            "Not Publishing",
            Some("Publishing Disabled"),
        )
    } else if !tracked {
        (
            "fa-ban",
            "yellow",
            "Not Publishing",
            Some(if is_new {
                "Repository Not Active Yet"
            } else {
                "Repository Inactive"
            }),
        )
    } else if importing {
        (
            "fa-ban",
            "yellow",
            "Not Publishing",
            Some("Repository Importing"),
        )
    } else {
        ("fa-check", "green", "Publishing Active", None)
    };
    let publishing = status_list(&[status_item(icon, color, &escape(label), note)]);
    PanelBox {
        title: "State".to_owned(),
        properties: vec![("Active", active), ("Publishing", publishing)],
        ..PanelBox::default()
    }
}

/// `buildRepositoryStatus()`: the checks of the working copy.
#[allow(clippy::too_many_lines)]
async fn repository_status<S: WebState>(
    state: &S,
    repository: &Repository,
    storage: &Storage,
    messages: &[(String, StatusMessage)],
) -> Result<String, PageError> {
    let mut items = Vec::new();
    if !repository.is_tracked() {
        items.push(status_item(
            "fa-exclamation-circle",
            "bluegrey",
            "Repository Inactive",
            Some("Activate this repository to begin or resume import."),
        ));
        return Ok(status_list(&items));
    }
    let binary = match repository.vcs.as_str() {
        "git" => "git",
        "hg" => "hg",
        _ => "svn",
    };
    match resolve_binary(state.config(), binary) {
        Some(path) => items.push(status_item(
            "fa-check-circle",
            "green",
            &format!("Found Binary <tt>{binary}</tt>"),
            Some(&format!("<tt>{}</tt>", escape(&path))),
        )),
        None => items.push(status_item(
            "fa-exclamation-circle",
            "red",
            &format!("Missing Binary <tt>{binary}</tt>"),
            Some(
                "Unable to find this binary in the webserver's PATH. You may need to configure \
                 <a href=\"/config/edit/environment.append-paths/\">environment.append-paths</a>.",
            ),
        )),
    }
    let instructions = format!(
        "Use <tt>bin/phd start</tt> to start daemons. See <a href=\"{}\">Managing Daemons with \
         phd</a>.",
        escape(&doclink("Managing Daemons with phd"))
    );
    for (class, running, stopped) in [
        (
            "PhabricatorRepositoryPullLocalDaemon",
            "Pull Daemon Running",
            "Pull Daemon Not Running",
        ),
        (
            "PhabricatorTaskmasterDaemon",
            "Task Daemon Running",
            "Task Daemon Not Running",
        ),
    ] {
        if daemon_alive(state, class).await? {
            items.push(status_item("fa-check-circle", "green", running, None));
        } else {
            items.push(status_item(
                "fa-exclamation-circle",
                "red",
                stopped,
                Some(&instructions),
            ));
        }
    }

    // usesLocalWorkingCopy(): Git and Mercurial always do.
    let local_path = &storage.local_path;
    let parent = Path::new(local_path)
        .parent()
        .map(|parent| parent.display().to_string())
        .unwrap_or_default();
    let tt = |text: &str| format!("<tt>{}</tt>", escape(text));
    if Path::new(&parent).exists() {
        items.push(status_item(
            "fa-check-circle",
            "green",
            "Storage Directory OK",
            Some(&tt(&parent)),
        ));
    } else {
        items.push(status_item(
            "fa-exclamation-circle",
            "red",
            "No Storage Directory",
            Some(&format!(
                "Storage directory {} does not exist, or is not readable by the webserver. \
                 Create this directory or make it readable.",
                tt(&parent)
            )),
        ));
        return Ok(status_list(&items));
    }
    match message(messages, "init") {
        Some(init) if init.code == "error" => {
            items.push(status_item(
                "fa-exclamation-circle",
                "red",
                "Initialization Error",
                Some(&escape(&message_text(init))),
            ));
            return Ok(status_list(&items));
        }
        Some(init) if init.code == "okay" => {
            if Path::new(local_path).exists() {
                items.push(status_item(
                    "fa-check-circle",
                    "green",
                    "Working Copy OK",
                    Some(&tt(local_path)),
                ));
            } else {
                items.push(status_item(
                    "fa-exclamation-circle",
                    "red",
                    "Working Copy Error",
                    Some(&format!(
                        "Working copy {} has been deleted, or is not readable by the webserver. \
                         Make this directory readable. If it has been deleted, the daemons \
                         should restore it automatically.",
                        tt(local_path)
                    )),
                ));
                return Ok(status_list(&items));
            }
        }
        Some(_) => {
            items.push(status_item(
                "fa-clock-o",
                "green",
                "Initializing Working Copy",
                Some("Daemons are initializing the working copy."),
            ));
            return Ok(status_list(&items));
        }
        None => {
            items.push(status_item(
                "fa-clock-o",
                "orange",
                "No Working Copy Yet",
                Some("Waiting for daemons to build a working copy."),
            ));
            return Ok(status_list(&items));
        }
    }
    match message(messages, "fetch") {
        Some(fetch) if fetch.code == "error" => {
            let suggestion = message_text(fetch)
                .contains("Permission denied (publickey).")
                .then_some(
                    "Public Key Error: This error usually indicates that the keypair you have \
                     configured does not have permission to access the repository.",
                );
            items.push(status_item(
                "fa-exclamation-circle",
                "red",
                "Update Error",
                suggestion,
            ));
            return Ok(status_list(&items));
        }
        Some(fetch) if fetch.code == "okay" => {
            let ago = phab_app_people::profile::relative_time(state.now() - fetch.epoch, 2);
            items.push(status_item(
                "fa-check-circle",
                "green",
                "Updates OK",
                Some(&escape(&format!(
                    "Last updated {} ({ago} ago).",
                    time::format_utc(fetch.epoch)
                ))),
            ));
        }
        Some(_) => {}
        None => items.push(status_item(
            "fa-clock-o",
            "orange",
            "Waiting For Update",
            Some("Waiting for daemons to read updates."),
        )),
    }
    if repository.detail("importing").is_some_and(is_truthy) {
        let ratio = import_progress(state, repository).await?;
        items.push(status_item(
            "fa-clock-o",
            "green",
            "Importing",
            Some(&format!("{:.2}% Complete", 100.0 * ratio)),
        ));
    } else {
        items.push(status_item(
            "fa-check-circle",
            "green",
            "Fully Imported",
            None,
        ));
    }
    if message(messages, "needs-update").is_some() {
        items.push(status_item(
            "fa-chevron-circle-up",
            "indigo",
            "Prioritized",
            Some("This repository will be updated soon!"),
        ));
    }
    Ok(status_list(&items))
}

/// The Basics panel.
async fn basics<S: WebState>(
    state: &S,
    viewer: &WebUser,
    repository: &Repository,
    storage: &Storage,
    can_edit: bool,
) -> Result<(Option<String>, Vec<PanelBox>), PageError> {
    let is_new = repository
        .detail("newly-initialized")
        .is_some_and(is_truthy);
    let notice = is_new.then(|| {
        "<p>This newly created repository is not active yet. Configure policies, options, and \
         URIs. When ready, <strong>Activate</strong> the repository.</p><p>If activated now, \
         this repository will observe an existing remote repository and begin importing \
         changes.</p>"
            .to_owned()
    });
    let encoding = match repository.detail("encoding") {
        Some(Value::String(encoding)) if !encoding.is_empty() => escape(encoding),
        _ => em("Use Default (UTF-8)"),
    };
    // canAllowDangerousChanges() and canAllowEnormousChanges(): only hosted
    // repositories, which are PHP's.
    let properties = vec![
        ("Name", escape(&repository.name)),
        ("Type", escape(vcs_name(&repository.vcs))),
        (
            "Callsign",
            repository
                .callsign
                .as_deref()
                .map_or_else(|| em("No Callsign"), escape),
        ),
        (
            "Short Name",
            repository
                .slug
                .as_deref()
                .map_or_else(|| em("No Short Name"), escape),
        ),
        ("Encoding", encoding),
        ("Dangerous Changes", em("Not Preventable")),
        ("Enormous Changes", em("Not Preventable")),
    ];
    let mut boxes = vec![
        state_box(repository),
        PanelBox {
            title: "Properties".to_owned(),
            properties,
            ..PanelBox::default()
        },
    ];
    if let Some(Value::String(description)) = repository.detail("description")
        && !description.is_empty()
    {
        let markup =
            remarkup::render(state, &viewer.user.viewer(), description, Mode::Html).await?;
        boxes.push(PanelBox {
            title: "Description".to_owned(),
            text_html: Some(String::from_utf8_lossy(markup.as_bytes()).into_owned()),
            ..PanelBox::default()
        });
    }
    let messages = status_messages(state, repository).await?;
    let interval = update_interval(state, repository, storage).await?;
    let frequency = format!(
        "{} · <a href=\"{}\" target=\"_blank\">Learn More</a>",
        escape(&phab_app_people::profile::relative_time(interval, 2)),
        escape(&doclink("Diffusion User Guide: Repository Updates"))
    );
    let status = repository_status(state, repository, storage, &messages).await?;
    let mut status_box = PanelBox {
        title: "Working Copy Status".to_owned(),
        properties: vec![("Update Frequency", frequency), ("Status", status)],
        ..PanelBox::default()
    };
    if let Some(fetch) = message(&messages, "fetch")
        && fetch.code == "error"
    {
        let raw = if can_edit {
            escape(&message_text(fetch)).replace('\n', "<br />")
        } else {
            em(
                "You must be able to edit a repository to see raw error messages because they \
                 sometimes disclose sensitive information.",
            )
        };
        status_box.sections.push(("Raw Error", raw));
    }
    boxes.push(status_box);
    Ok((notice, boxes))
}

/// `renderPolicyDescriptions()` of one capability: the policy's name,
/// linked to its explanation.
fn policy_link(
    repository: &Repository,
    capability: &str,
    policy: &str,
    handles: &std::collections::HashMap<Phid, harbormaster::Handle>,
) -> Result<String, PageError> {
    let name = match policy {
        "public" => "Public (No Login Required)".to_owned(),
        "users" => "All Users".to_owned(),
        "admin" => "Administrators".to_owned(),
        "no-one" => "No One".to_owned(),
        _ if policy.starts_with("PHID-PLCY-") => "Custom Policy".to_owned(),
        _ if policy.starts_with("PHID-PROJ-") || policy.starts_with("PHID-USER-") => handles
            .get(&Phid::new(policy))
            .filter(|handle| handle.uri.is_some())
            .map(|handle| {
                if policy.starts_with("PHID-PROJ-") {
                    handle.name.clone()
                } else {
                    handle.full_name.clone()
                }
            })
            .ok_or(PageError::Fallback("a policy the viewer can't see"))?,
        _ => return Err(PageError::Fallback("an unusual policy")),
    };
    let mut html = format!(
        "<a href=\"/policy/explain/{}/{capability}/\">{}</a>",
        escape(repository.phid.as_str()),
        escape(&name)
    );
    if policy.starts_with("PHID-PROJ-") {
        html.push_str(" (Project)");
    }
    Ok(html)
}

/// The policies of the repository: view, edit and push.
fn policies_of(repository: &Repository) -> [(&'static str, String); 3] {
    let subject = repository.subject();
    let policy = |capability| subject.policy(capability).unwrap_or_default().to_owned();
    [
        ("view", policy(phab_policy::Capability::VIEW)),
        ("edit", policy(phab_policy::Capability::EDIT)),
        (
            "diffusion.push",
            policy(phab_app_differential::storage::PUSH),
        ),
    ]
}

/// The Policies panel.
async fn policies<S: WebState>(state: &S, context: &Context) -> Result<Vec<PanelBox>, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT 1 FROM {} LIMIT 1",
        database.table("spaces", "spaces_namespace")
    );
    if phab_db::sqlx::query(&sql)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?
        .is_some()
    {
        return Err(PageError::Fallback("an install with Spaces"));
    }
    let repository = &context.repository;
    let policies = policies_of(repository);
    let phids: Vec<Phid> = policies
        .iter()
        .filter(|(_, policy)| policy.starts_with("PHID-PROJ-") || policy.starts_with("PHID-USER-"))
        .map(|(_, policy)| Phid::new(policy.as_str()))
        .collect();
    let handles = harbormaster::load_handles(state, &context.scope, &phids).await?;
    let [view, edit, _] = &policies;
    Ok(vec![PanelBox {
        title: "Policies".to_owned(),
        properties: vec![
            (
                "Visible To",
                policy_link(repository, view.0, &view.1, &handles)?,
            ),
            (
                "Editable By",
                policy_link(repository, edit.0, &edit.1, &handles)?,
            ),
            ("Pushable By", em("Not a Hosted Repository")),
        ],
        ..PanelBox::default()
    }])
}

/// The panels of a Git repository, in their groups' order: key, label,
/// group label (on the first of a group) and `getManagementPanelIcon()`.
fn panels(
    repository: &Repository,
) -> Vec<(&'static str, &'static str, Option<&'static str>, String)> {
    let detail = |key: &str| repository.detail(key);
    let grey = |icon: &str, any: bool| {
        if any {
            icon.to_owned()
        } else {
            format!("{icon} grey")
        }
    };
    let [view, edit, push] = policies_of(repository);
    let default_policies = view.1 == "users"
        && edit.1 == "admin"
        && push.1 == "admin"
        && repository.subject().space.clone().flatten().is_none();
    let branches = detail("default-branch").is_some_and(is_truthy)
        || !strings(detail("fetch-rules")).is_empty()
        || !keys(detail("branch-filter")).is_empty()
        || !keys(detail("close-commits-filter")).is_empty();
    vec![
        ("basics", "Basics", None, "fa-code".to_owned()),
        (
            "policies",
            "Policies",
            None,
            grey("fa-lock", !default_policies),
        ),
        ("uris", "URIs", None, "fa-globe".to_owned()),
        ("storage", "Storage", None, "fa-download".to_owned()),
        (
            "limits",
            "Limits",
            None,
            grey("fa-signal", positive(detail("limit.filesize")).is_some()),
        ),
        ("branches", "Branches", None, grey("fa-code-fork", branches)),
        ("history", "History", None, "fa-list-ul".to_owned()),
        (
            "staging",
            "Staging Area",
            Some("Builds"),
            grey("fa-upload", detail("staging-uri").is_some_and(is_truthy)),
        ),
        (
            "symbols",
            "Symbols",
            Some("Integrations"),
            grey(
                "fa-link",
                !strings(detail("symbol-languages")).is_empty()
                    || !strings(detail("symbol-sources")).is_empty(),
            ),
        ),
    ]
}

/// The links of a panel's curtain.
#[allow(clippy::too_many_lines)]
fn curtain(repository: &Repository, panel: &str, can_edit: bool) -> Vec<Action> {
    let edit_page = |page: &str| format!("/diffusion/edit/{}/page/{page}/", repository.id);
    let path = |suffix: &str| format!("{}{suffix}", repository.uri());
    let action = |name, icon, href, disabled| Action {
        name,
        icon,
        href,
        disabled,
        divider: false,
    };
    match panel {
        "basics" => {
            let tracked = repository.is_tracked();
            let publishing = !repository.detail("herald-disabled").is_some_and(is_truthy);
            let mut actions = vec![
                action(
                    "Edit Basic Information",
                    "fa-pencil",
                    edit_page("basics"),
                    !can_edit,
                ),
                action(
                    "Edit Text Encoding",
                    "fa-text-width",
                    edit_page("encoding"),
                    !can_edit,
                ),
                action(
                    "Allow Dangerous Changes",
                    "fa-exclamation-triangle",
                    path("edit/dangerous/"),
                    true,
                ),
                action(
                    "Allow Enormous Changes",
                    "fa-exclamation-triangle",
                    path("edit/enormous/"),
                    true,
                ),
                action(
                    if tracked {
                        "Deactivate Repository"
                    } else {
                        "Activate Repository"
                    },
                    if tracked { "fa-ban" } else { "fa-check" },
                    path("edit/activate/"),
                    !can_edit,
                ),
                action(
                    if publishing {
                        "Disable Publishing"
                    } else {
                        "Enable Publishing"
                    },
                    if publishing { "fa-ban" } else { "fa-check" },
                    path("edit/publish/"),
                    !can_edit,
                ),
                action("Update Now", "fa-refresh", path("edit/update/"), !can_edit),
                action("Delete Repository", "fa-times", path("edit/delete/"), false),
            ];
            actions[4].divider = true;
            actions[7].divider = true;
            actions
        }
        "policies" => vec![action(
            "Edit Policies",
            "fa-pencil",
            edit_page("policies"),
            !can_edit,
        )],
        "storage" => vec![action(
            "Cluster Documentation",
            "fa-book",
            doclink("Cluster: Repositories"),
            false,
        )],
        "limits" => vec![action(
            "Edit Limits",
            "fa-pencil",
            edit_page("limits"),
            !can_edit,
        )],
        "branches" => vec![
            action(
                "Edit Branches",
                "fa-pencil",
                edit_page("branches"),
                !can_edit,
            ),
            action(
                "View Branches",
                "fa-code-fork",
                super::uri::generate(repository, "branches", super::uri::Target::default()),
                false,
            ),
        ],
        "staging" => vec![action(
            "Edit Staging",
            "fa-pencil",
            edit_page("staging"),
            !can_edit,
        )],
        "symbols" => vec![action(
            "Edit Symbols",
            "fa-pencil",
            edit_page("symbols"),
            !can_edit,
        )],
        _ => Vec::new(),
    }
}

/// The content of a panel.
#[allow(clippy::too_many_lines)]
async fn content<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: &Context,
    panel: &str,
    storage: &Storage,
    can_edit: bool,
) -> Result<(Option<String>, Vec<PanelBox>), PageError> {
    let repository = &context.repository;
    let detail = |key: &str| repository.detail(key);
    let one = |title: &str, properties| {
        Ok((
            None,
            vec![PanelBox {
                title: title.to_owned(),
                properties,
                ..PanelBox::default()
            }],
        ))
    };
    match panel {
        "basics" => basics(state, viewer, repository, storage, can_edit).await,
        "policies" => Ok((None, policies(state, context).await?)),
        "storage" => {
            // usesLocalWorkingCopy() is always true for Git.
            let path = escape(&storage.local_path);
            Ok((
                None,
                vec![
                    PanelBox {
                        title: "Storage".to_owned(),
                        properties: vec![("Storage Path", path), ("Storage Cluster", em("Local"))],
                        ..PanelBox::default()
                    },
                    PanelBox {
                        title: "Cluster Status".to_owned(),
                        table: Some(Table {
                            headers: vec![
                                "",
                                "Device",
                                "Version",
                                "Writing",
                                "Last Writer",
                                "Last Write At",
                            ],
                            rows: Vec::new(),
                            empty: "This is not a cluster repository.",
                        }),
                        ..PanelBox::default()
                    },
                ],
            ))
        }
        "limits" => {
            let filesize = positive(detail("limit.filesize")).map_or_else(
                || em("Unlimited"),
                |limit| format!("{} Bytes", thousands(limit)),
            );
            let copy = positive(detail("limit.copy")).map_or_else(
                || em("Default (900 Seconds)"),
                |limit| format!("{} Seconds", thousands(limit)),
            );
            let touch = positive(detail("limit.touch")).map_or_else(
                || em("Unlimited"),
                |limit| format!("{} Paths", thousands(limit)),
            );
            one(
                "Limits",
                vec![
                    ("Filesize Limit", filesize),
                    ("Clone/Fetch Timeout", copy),
                    ("Touched Paths Limit", touch),
                ],
            )
        }
        "branches" => {
            let default_branch = match detail("default-branch") {
                Some(Value::String(branch)) if !branch.is_empty() && branch != "0" => {
                    escape(branch)
                }
                _ => em(&repository.default_branch().unwrap_or_default()),
            };
            let fetch = strings(detail("fetch-rules"));
            let mut properties = vec![
                ("Default Branch", default_branch),
                (
                    "Fetch Refs",
                    if fetch.is_empty() {
                        em("Fetch All Refs")
                    } else {
                        escape(&fetch.join(", "))
                    },
                ),
            ];
            let track = keys(detail("branch-filter"));
            if !track.is_empty() {
                properties.push(("Track Only", escape(&track.join(", "))));
            }
            let permanent = if detail("herald-disabled").is_some_and(is_truthy) {
                em("Publishing Disabled")
            } else {
                let rules = keys(detail("close-commits-filter"));
                if rules.is_empty() {
                    em("All Branches")
                } else {
                    escape(&rules.join(", "))
                }
            };
            properties.push(("Permanent Refs", permanent));
            one("Branches", properties)
        }
        "staging" => {
            let uri = match detail("staging-uri") {
                Some(Value::String(uri)) if !uri.is_empty() && uri != "0" => escape(uri),
                _ => em("No Staging Area"),
            };
            one("Staging Area", vec![("Staging Area URI", uri)])
        }
        "symbols" => {
            let languages = strings(detail("symbol-languages"));
            let sources: Vec<Phid> = strings(detail("symbol-sources"))
                .into_iter()
                .map(Phid::new)
                .collect();
            let sources_html = if sources.is_empty() {
                em("This Repository Only")
            } else {
                let handles = harbormaster::load_handles(state, &context.scope, &sources).await?;
                sources
                    .iter()
                    .map(|phid| harbormaster::handle(&handles, phid).link_html())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            one(
                "Symbols",
                vec![
                    (
                        "Languages",
                        if languages.is_empty() {
                            em("Any")
                        } else {
                            escape(&languages.join(", "))
                        },
                    ),
                    ("Uses Symbols From", sources_html),
                ],
            )
        }
        "uris" => Err(PageError::Fallback("the URIs panel")),
        _ => Err(PageError::Fallback("the History panel")),
    }
}

/// `GET /source/<x>/manage/[<panel>/]`.
pub(crate) async fn page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: Context,
    panel: Option<&str>,
) -> Result<Response, PageError> {
    let repository = &context.repository;
    let panels = panels(repository);
    let selected = panel.unwrap_or("basics");
    let Some(current) = panels.iter().find(|(key, ..)| *key == selected) else {
        return Err(PageError::NotFound);
    };
    let storage = storage(state, repository).await?;
    if storage.service.is_some() {
        return Err(PageError::Fallback("a repository served by a cluster"));
    }
    let can_edit = repository::can_edit(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        repository,
        state.now(),
    )
    .await
    .map_err(internal)?;
    let (notice_html, boxes) =
        content(state, viewer, &context, selected, &storage, can_edit).await?;
    let base = format!("{}manage/", repository.uri());
    let nav = panels
        .iter()
        .map(|(key, name, group, icon)| NavItem {
            group: *group,
            name,
            icon: icon.clone(),
            href: format!("{base}{key}/"),
            selected: *key == selected,
        })
        .collect();
    let title = format!("{} · {}", current.1, repository.display_name());
    Ok(Page(ManagePage {
        shell: context.shell(state, viewer, &title),
        name: repository.display_name(),
        href: repository.uri(),
        active: repository.is_tracked(),
        help_href: doclink("Diffusion User Guide: Managing Repositories"),
        nav,
        notice_html,
        boxes,
        actions: curtain(repository, selected, can_edit),
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_documentation_like_php() {
        assert_eq!(
            doclink("Managing Daemons with phd"),
            "https://secure.phabricator.com/diviner/find/?name=Managing+Daemons+with+phd\
             &type=article&jump=1"
        );
    }

    #[test]
    fn reads_details() {
        assert_eq!(positive(Some(&serde_json::json!("12"))), Some(12));
        assert_eq!(positive(Some(&serde_json::json!(0))), None);
        assert_eq!(keys(Some(&serde_json::json!({"master": true}))), ["master"]);
    }
}
