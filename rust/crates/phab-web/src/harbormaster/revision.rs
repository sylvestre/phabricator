//! What the revision page shows of its active diff's builds and checks:
//! the "Build Status" property `HarbormasterUIEventListener` adds to the
//! diff's properties, the Lint and Unit fields (`DifferentialLintField`,
//! `DifferentialUnitField`), the "Unit Tests" box
//! (`buildUnitMessagesView()`) and the warnings of the fields above the
//! comment form (`getWarningsForDetailView()`).

use phab_app_differential::details::{self, DiffStatus};
use phab_app_harbormaster::web::{self as harbormaster, Artifact, Buildable, Target};
use phab_core::{Config, Phid};

use super::status::{self, Status};
use super::unit::{self, UnitSummary};
use super::{Scope, method_error};
use crate::error::PageError;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `DifferentialLintField` shows this many messages.
const LINT_LIMIT: usize = 25;
/// The "Unit Tests" box shows this many results.
const UNIT_LIMIT: usize = 5;

/// A build of the diff's buildable.
pub(crate) struct BuildLine {
    pub(crate) link_html: String,
    pub(crate) status: Status,
    /// The links to external build systems, as HTML.
    pub(crate) links_html: Vec<String>,
}

/// The "Build Status" property.
pub(crate) struct BuildStatus {
    pub(crate) buildable_id: i64,
    pub(crate) status: Status,
    pub(crate) builds: Vec<BuildLine>,
}

/// A row of the Lint field's table.
pub(crate) struct LintRow {
    pub(crate) severity: String,
    pub(crate) location: String,
    /// The anchor of the line in the diff, for files of the diff.
    pub(crate) href: Option<String>,
    pub(crate) code: String,
    pub(crate) name: String,
}

/// The Lint field: the diff's lint status and messages.
pub(crate) struct LintField {
    pub(crate) status: Status,
    pub(crate) rows: Vec<LintRow>,
}

/// What the revision page shows of the active diff's checks.
#[derive(Default)]
pub(crate) struct DiffChecks {
    pub(crate) build_status: Option<BuildStatus>,
    pub(crate) lint: Option<LintField>,
    pub(crate) unit: Option<Status>,
    pub(crate) unit_tests: Option<UnitSummary>,
    /// The warnings above the comment form, in the order of the fields.
    pub(crate) warnings: Vec<&'static str>,
}

/// The fields whose warnings and values the page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Lint,
    Unit,
    Draft,
}

impl Field {
    fn key(self) -> &'static str {
        match self {
            Self::Lint => "differential:lint",
            Self::Unit => "differential:unit",
            Self::Draft => "differential:draft",
        }
    }
}

/// `PhabricatorCustomField::buildFieldList()` for these fields: those
/// `differential.fields` lists come first, in its order; a field is
/// enabled unless it says it is disabled.
fn enabled_fields(config: &Config) -> Vec<Field> {
    let all = [Field::Lint, Field::Unit, Field::Draft];
    let spec = config.object("differential.fields");
    let mut fields: Vec<Field> = spec
        .map(|spec| {
            spec.keys()
                .filter_map(|key| all.iter().copied().find(|field| field.key() == key))
                .collect()
        })
        .unwrap_or_default();
    for field in all {
        if !fields.contains(&field) {
            fields.push(field);
        }
    }
    fields.retain(|field| {
        !spec
            .and_then(|spec| spec.get(field.key()))
            .and_then(|entry| entry.get("disabled"))
            .is_some_and(phab_core::php::is_truthy)
    });
    fields
}

/// `DifferentialLintStatus` and `DifferentialUnitStatus`: a diff's
/// status, by kind ("Lint" or "Test").
fn check_status(value: i64, lint: bool) -> Status {
    let (lint_name, unit_name, icon, color) = match value {
        0 => ("No Lint Coverage", "No Test Coverage", "fa-ban", "grey"),
        1 => ("Lint Passed", "Tests Passed", "fa-check", "green"),
        2 => (
            "Lint Warnings",
            "Test Warnings",
            "fa-exclamation-triangle",
            "yellow",
        ),
        3 => ("Lint Errors", "Test Failures", "fa-times", "red"),
        4 => ("Lint Skipped", "Tests Skipped", "fa-fast-forward", "blue"),
        6 => (
            "Lint Not Applicable",
            "Tests Not Applicable",
            "fa-code",
            "grey",
        ),
        _ => {
            let kind = if lint { "Lint" } else { "Unit" };
            return Status {
                name: format!("Unknown {kind} Status (\"{value}\")"),
                icon: "",
                color: "",
            };
        }
    };
    Status {
        name: if lint { lint_name } else { unit_name }.to_owned(),
        icon,
        color,
    }
}

/// The warnings of a lint or unit status: none below "warnings" and for
/// "not applicable".
fn status_warning(
    value: i64,
    skipped: &'static str,
    problems: &'static str,
) -> Option<&'static str> {
    match value {
        ..2 | 6 => None,
        4 => Some(skipped),
        _ => Some(problems),
    }
}

/// The automatic buildable of `diff` the viewer can see, and the targets
/// of its builds' current runs.
async fn buildable_targets<S: WebState>(
    state: &S,
    viewer: &WebUser,
    scope: &Scope,
    diff: &Phid,
) -> Result<Option<(Buildable, Vec<harbormaster::Build>, Vec<Target>)>, PageError> {
    if !super::can_use(state, viewer, super::APPLICATION).await? {
        return Ok(None);
    }
    let request = scope.request();
    let buildables =
        harbormaster::buildables_of_objects(&request, std::slice::from_ref(diff), Some(false))
            .await
            .map_err(method_error)?;
    // `executeOne()`: more than one is an exception in PHP.
    if buildables.len() > 1 {
        return Err(PageError::Fallback("a diff with several buildables"));
    }
    let Some(buildable) = buildables.into_iter().next() else {
        return Ok(None);
    };
    let builds =
        harbormaster::builds_of_buildables(&request, std::slice::from_ref(&buildable.phid))
            .await
            .map_err(method_error)?;
    let build_phids: Vec<Phid> = builds.iter().map(|build| build.phid.clone()).collect();
    // `needTargets()`: chronological, of each build's generation.
    let mut targets = harbormaster::targets_of_builds(&request, &build_phids, None)
        .await
        .map_err(method_error)?;
    targets.reverse();
    targets.retain(|target| {
        builds
            .iter()
            .any(|build| build.phid == target.build_phid && build.generation == target.generation)
    });
    Ok(Some((buildable, builds, targets)))
}

/// The "Build Status" property.
async fn build_status<S: WebState>(
    state: &S,
    scope: &Scope,
    buildable: &Buildable,
    builds: &[harbormaster::Build],
    targets: &[Target],
) -> Result<BuildStatus, PageError> {
    let target_phids: Vec<Phid> = targets.iter().map(|target| target.phid.clone()).collect();
    let artifacts: Vec<Artifact> =
        harbormaster::artifacts_of_targets(&scope.request(), &target_phids)
            .await
            .map_err(method_error)?
            .into_iter()
            .filter(|artifact| artifact.kind == "uri")
            .collect();
    let build_phids: Vec<Phid> = builds.iter().map(|build| build.phid.clone()).collect();
    let handles = super::load_handles(state, scope, &build_phids).await?;
    let mut build_lines = Vec::new();
    for build in builds {
        let mut links = Vec::new();
        for target in targets
            .iter()
            .filter(|target| target.build_phid == build.phid)
        {
            let of_target: Vec<&Artifact> = artifacts
                .iter()
                .filter(|artifact| artifact.target_phid == target.phid)
                .collect();
            links.extend(super::build::external_links(state.config(), &of_target)?);
        }
        build_lines.push(BuildLine {
            link_html: super::handle(&handles, &build.phid).link_html(),
            // The stored status, without pending commands.
            status: status::build(&build.status),
            links_html: links,
        });
    }
    Ok(BuildStatus {
        buildable_id: buildable.id,
        status: status::buildable(&buildable.status),
        builds: build_lines,
    })
}

/// The Lint field (`DifferentialHarbormasterField`): the status, and the
/// targets' messages, linked to their lines in the diff.
async fn lint_field<S: WebState>(
    state: &S,
    diff: DiffStatus,
    target_phids: &[Phid],
) -> Result<LintField, PageError> {
    let database = state.database();
    // `LIMIT 25`, without an order: the first rows.
    let mut messages = harbormaster::lint_messages(database, target_phids).await?;
    messages.truncate(LINT_LIMIT);
    if messages.is_empty() && details::has_diff_property(database, diff.id, "arc:lint").await? {
        return Err(PageError::Fallback("legacy lint messages"));
    }
    messages.sort_by_cached_key(|message| {
        status::lint_sort_key(
            &message.severity,
            &message.path,
            message.line,
            &message.code,
        )
    });
    let changesets = if messages.is_empty() {
        Vec::new()
    } else {
        phab_diff::storage::load_changesets(database, diff.id)
            .await
            .map_err(super::internal)?
    };
    let rows = messages
        .into_iter()
        .map(|message| {
            let href = changesets
                .iter()
                .rev()
                .find(|changeset| changeset.filename == message.path)
                .map(|changeset| {
                    format!("#C{}NL{}", changeset.id, message.line.unwrap_or(0).max(1))
                });
            LintRow {
                severity: status::lint_severity(&message.severity).to_owned(),
                location: format!(
                    "{}:{}",
                    message.path,
                    message
                        .line
                        .map(|line| line.to_string())
                        .unwrap_or_default()
                ),
                href,
                code: message.code,
                name: message.name,
            }
        })
        .collect();
    Ok(LintField {
        status: check_status(diff.lint, true),
        rows,
    })
}

/// The "Unit Tests" box: shown when some result is neither a pass nor a
/// skip.
async fn unit_tests<S: WebState>(
    state: &S,
    scope: &Scope,
    diff: DiffStatus,
    buildable: &Buildable,
    target_phids: &[Phid],
) -> Result<Option<UnitSummary>, PageError> {
    let database = state.database();
    let messages = harbormaster::unit_messages(database, target_phids).await?;
    if messages.is_empty() {
        if details::has_diff_property(database, diff.id, "arc:unit").await? {
            return Err(PageError::Fallback("legacy unit test results"));
        }
        return Ok(None);
    }
    if messages
        .iter()
        .all(|message| matches!(message.result.as_str(), "pass" | "skip"))
    {
        return Ok(None);
    }
    Ok(Some(
        unit::summary(state, &scope.viewer(), buildable.id, messages, UNIT_LIMIT).await?,
    ))
}

/// The checks of the revision's active diff, as the viewer sees them.
pub(crate) async fn diff_checks<S: WebState>(
    state: &S,
    viewer: &WebUser,
    revision: &Phid,
    diff: &Phid,
) -> Result<DiffChecks, PageError> {
    let database = state.database();
    let Some(diff_status) = details::load_diff_status(database, diff).await? else {
        return Ok(DiffChecks::default());
    };
    let scope = Scope::new(state, viewer);
    let found = buildable_targets(state, viewer, &scope, diff).await?;
    let target_phids: Vec<Phid> = found
        .as_ref()
        .map(|(_, _, targets)| targets.iter().map(|target| target.phid.clone()).collect())
        .unwrap_or_default();
    let mut checks = DiffChecks::default();
    if let Some((buildable, builds, targets)) = &found {
        checks.build_status = Some(build_status(state, &scope, buildable, builds, targets).await?);
        checks.unit_tests =
            unit_tests(state, &scope, diff_status, buildable, &target_phids).await?;
    }
    for field in enabled_fields(state.config()) {
        match field {
            Field::Lint => {
                checks.lint = Some(lint_field(state, diff_status, &target_phids).await?);
                checks.warnings.extend(status_warning(
                    diff_status.lint,
                    "Lint was skipped when generating these changes.",
                    "These changes have lint problems.",
                ));
            }
            Field::Unit => {
                checks.unit = Some(check_status(diff_status.unit, false));
                match found
                    .as_ref()
                    .map(|(buildable, _, _)| buildable.status.as_str())
                {
                    Some("building") => checks.warnings.push(
                        "These changes have not finished building yet and may have build \
                         failures.",
                    ),
                    Some("failed") => checks.warnings.push("These changes have failed to build."),
                    _ => {}
                }
                checks.warnings.extend(status_warning(
                    diff_status.unit,
                    "Unit tests were skipped when generating these changes.",
                    "These changes have unit test problems.",
                ));
            }
            Field::Draft => {
                if !details::should_broadcast(database, revision).await? {
                    checks.warnings.push(
                        "This revision is currently a draft. You can leave comments, but no \
                         one will be notified until the revision is submitted for review.",
                    );
                }
            }
        }
    }
    Ok(checks)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn fields_follow_the_configuration() {
        assert_eq!(
            enabled_fields(&Config::default()),
            [Field::Lint, Field::Unit, Field::Draft]
        );
        let mut config = Config::default();
        if let serde_json::Value::Object(map) = json!({"differential.fields": {
            "differential:draft": {"disabled": false},
            "differential:unit": {"disabled": true},
        }}) {
            config.push_layer("test", map);
        }
        assert_eq!(enabled_fields(&config), [Field::Draft, Field::Lint]);
    }

    #[test]
    fn statuses_and_warnings() {
        assert_eq!(check_status(3, true).name, "Lint Errors");
        assert_eq!(check_status(3, false).name, "Test Failures");
        assert_eq!(check_status(5, false).name, "Unknown Unit Status (\"5\")");
        assert_eq!(status_warning(1, "s", "p"), None);
        assert_eq!(status_warning(6, "s", "p"), None);
        assert_eq!(status_warning(4, "s", "p"), Some("s"));
        assert_eq!(status_warning(3, "s", "p"), Some("p"));
    }
}
