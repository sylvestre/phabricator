//! `/B<id>`: a buildable, its builds and their targets
//! (`HarbormasterBuildableViewController`).

use std::sync::Arc;

use askama::Template;
use axum::http::Uri;
use phab_app_harbormaster::web::{self as harbormaster, Buildable, Lookup, Target};
use phab_core::{Phid, PhidType};
use phab_policy::{Capability, ObjectKind, PolicySubject};

use super::status::{self, Status};
use super::{Action, BuildState, Command, Scope, TransactionTable, method_error};
use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::timeline::Entry;
use crate::viewer::WebUser;

/// `HarbormasterLintPropertyView::setLimit()` on this page.
const LINT_LIMIT: usize = 10;
/// `HarbormasterUnitSummaryView::setLimit()` on this page.
const UNIT_LIMIT: usize = 5;

/// A target in the list of a build.
pub(crate) struct TargetItem {
    id: i64,
    name: String,
    status: Status,
}

/// A build in the list.
pub(crate) struct BuildItem {
    id: i64,
    name: String,
    status: Status,
    targets: Vec<TargetItem>,
}

/// A row of the lint table.
pub(crate) struct LintRow {
    pub(crate) severity: String,
    pub(crate) location: String,
    pub(crate) code: String,
    pub(crate) name: String,
}

#[derive(Template)]
#[template(path = "buildable.html")]
pub(crate) struct BuildablePage {
    shell: Shell,
    id: i64,
    status: Status,
    container_html: Option<String>,
    buildable_html: String,
    manual: bool,
    lint: Vec<LintRow>,
    unit: Option<super::unit::UnitSummary>,
    builds: Vec<BuildItem>,
    timeline: Vec<Entry>,
    actions: Vec<Action>,
}

/// `B123`'s ID; `B0123` is not a monogram.
pub(crate) fn parse_monogram(monogram: &str) -> Option<i64> {
    monogram
        .strip_prefix('B')
        .filter(|digits| {
            !digits.is_empty()
                && !digits.starts_with('0')
                && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
        .and_then(|digits| digits.parse().ok())
}

/// `PhabricatorPolicyFilter::hasCapability()` with `CAN_EDIT` on the
/// buildable, whose policies are those of the object built.
pub(crate) async fn can_edit<S: WebState>(
    state: &S,
    scope: &Scope,
    buildable: &Buildable,
) -> Result<bool, PageError> {
    let Some(policies) = &buildable.policies else {
        return Ok(false);
    };
    let subject = PolicySubject::new(Some(buildable.phid.clone()), ObjectKind::Other)
        .with_policy(Capability::VIEW, policies.view.clone())
        .with_policy(Capability::EDIT, policies.edit.clone());
    let viewer = scope.viewer();
    let facts = phab_policy::loader::load_facts(
        state.database(),
        state.config(),
        &viewer,
        &[&subject],
        &phab_policy::loader::NoExtendedObjects,
        state.now(),
    )
    .await
    .map_err(super::internal)?;
    let can_edit = phab_policy::filter::has_capability(&viewer, &subject, Capability::EDIT, &facts)
        .map_err(super::internal)?;
    // The author of a diff without revision can always edit it, which
    // the inherited policies don't say.
    if !can_edit && buildable.object_phid.phid_type() == PhidType::DIFF {
        return Err(PageError::Fallback("the edit capability on a diff"));
    }
    Ok(can_edit)
}

/// The lint messages of the targets, as the table shows them: the first
/// `limit`, or all of them.
pub(crate) async fn lint<S: WebState>(
    state: &S,
    targets: &[Phid],
    limit: Option<usize>,
) -> Result<Vec<LintRow>, PageError> {
    let mut messages = harbormaster::lint_messages(state.database(), targets).await?;
    // `msort()` keeps the order of equal keys.
    messages.sort_by_cached_key(|message| {
        status::lint_sort_key(
            &message.severity,
            &message.path,
            message.line,
            &message.code,
        )
    });
    if let Some(limit) = limit {
        messages.truncate(limit);
    }
    Ok(messages
        .into_iter()
        .map(|message| LintRow {
            severity: status::lint_severity(&message.severity).to_owned(),
            location: format!(
                "{}:{}",
                message.path,
                message
                    .line
                    .map(|line| line.to_string())
                    .unwrap_or_default()
            ),
            code: message.code,
            name: message.name,
        })
        .collect())
}

/// `buildCurtainView()`: commands are enabled if they can be sent to at
/// least one build.
fn actions(id: i64, states: &[BuildState<'_>], can_edit: bool) -> Result<Vec<Action>, PageError> {
    let mut actions = Vec::new();
    for command in Command::ALL {
        let mut can_send = false;
        for build_state in states {
            if build_state.can_send(command)? {
                can_send = true;
                break;
            }
        }
        actions.push(Action {
            name: command.buildable_name(),
            icon: command.icon(),
            uri: format!("/harbormaster/buildable/{id}/{}/", command.key()),
            enabled: can_send && can_edit,
        });
    }
    Ok(actions)
}

/// The "Unit Tests" box, if the targets reported results.
async fn unit_tests<S: WebState>(
    state: &S,
    scope: &Scope,
    id: i64,
    targets: &[Phid],
) -> Result<Option<super::unit::UnitSummary>, PageError> {
    let messages = harbormaster::unit_messages(state.database(), targets).await?;
    if messages.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        super::unit::summary(state, &scope.viewer(), id, messages, Some(UNIT_LIMIT)).await?,
    ))
}

/// `HarbormasterBuildableTransaction` titles.
fn title(
    transaction: &harbormaster::Transaction,
    author: &str,
) -> Result<Option<String>, PageError> {
    if transaction.transaction_type != "harbormaster:buildable:command" {
        return Err(PageError::Fallback("a buildable transaction not ported"));
    }
    let command = transaction
        .new_value
        .as_str()
        .and_then(Command::from_key)
        .ok_or(PageError::Fallback("a buildable command not ported"))?;
    Ok(Some(super::command_title(command, author, "buildable")))
}

/// The builds of a buildable the viewer sees and the targets of their
/// current runs, chronological (`needBuilds()` and `needTargets()`).
pub(crate) async fn builds_and_targets(
    request: &phab_conduit::ApiRequest<'_>,
    buildable: &Buildable,
) -> Result<(Vec<harbormaster::Build>, Vec<Target>), PageError> {
    let builds = harbormaster::builds_of_buildables(request, std::slice::from_ref(&buildable.phid))
        .await
        .map_err(method_error)?;
    let build_phids: Vec<Phid> = builds.iter().map(|build| build.phid.clone()).collect();
    // `needBuildTargets()`: chronological, of each build's generation.
    let mut targets: Vec<Target> = harbormaster::targets_of_builds(request, &build_phids, None)
        .await
        .map_err(method_error)?;
    targets.reverse();
    targets.retain(|target| {
        builds
            .iter()
            .any(|build| build.phid == target.build_phid && build.generation == target.generation)
    });
    Ok((builds, targets))
}

/// `GET /B{id}`, from the route of monograms.
pub(crate) async fn show<S: WebState>(
    state: Arc<S>,
    viewer: WebUser,
    id: i64,
    uri: Uri,
) -> Result<Page<BuildablePage>, PageError> {
    viewer
        .require_application(&*state, super::APPLICATION)
        .await?;
    let scope = Scope::new(state.as_ref(), &viewer);
    let request = scope.request();
    let buildable = match harbormaster::buildable(&request, id)
        .await
        .map_err(method_error)?
    {
        Lookup::Visible(buildable) => buildable,
        Lookup::Missing | Lookup::Denied => return Err(PageError::NotFound),
    };

    let (builds, targets) = builds_and_targets(&request, &buildable).await?;
    let target_phids: Vec<Phid> = targets.iter().map(|target| target.phid.clone()).collect();
    let lint = lint(state.as_ref(), &target_phids, Some(LINT_LIMIT)).await?;
    let unit = unit_tests(state.as_ref(), &scope, id, &target_phids).await?;

    let plans = harbormaster::plans_by_phid(
        &request,
        &builds
            .iter()
            .map(|build| build.plan_phid.clone())
            .collect::<Vec<_>>(),
    )
    .await
    .map_err(method_error)?;
    let states = super::build_states(state.as_ref(), &scope, &builds, &plans).await?;
    let build_items = states
        .iter()
        .map(|build_state| {
            let build = build_state.build;
            BuildItem {
                id: build.id,
                name: super::build_name(build, &plans),
                status: status::build(build_state.pending_status()),
                targets: targets
                    .iter()
                    .filter(|target| target.build_phid == build.phid)
                    .map(|target| TargetItem {
                        id: target.id,
                        name: harbormaster::target_name(target),
                        status: status::target(&target.status),
                    })
                    .collect(),
            }
        })
        .collect();

    let can_edit = can_edit(state.as_ref(), &scope, &buildable).await?;
    let actions = actions(id, &states, can_edit)?;

    let mut phids = vec![buildable.object_phid.clone()];
    let container = buildable
        .container_phid
        .as_deref()
        .filter(|phid| !phid.is_empty())
        .map(Phid::new);
    phids.extend(container.iter().cloned());
    let handles = super::load_handles(state.as_ref(), &scope, &phids).await?;
    let timeline = super::timeline(
        state.as_ref(),
        &scope,
        TransactionTable::Buildable,
        &buildable.phid,
        title,
    )
    .await?;

    let title = format!("Buildable {id}");
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(BuildablePage {
        shell: Shell::for_user(state.config(), &viewer, &title, current),
        id,
        status: status::buildable(&buildable.status),
        container_html: container.map(|phid| super::handle(&handles, &phid).link_html()),
        buildable_html: super::handle(&handles, &buildable.object_phid).link_html(),
        manual: buildable.is_manual,
        lint,
        unit,
        builds: build_items,
        timeline,
        actions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monograms() {
        assert_eq!(parse_monogram("B12"), Some(12));
        assert_eq!(parse_monogram("B012"), None);
        assert_eq!(parse_monogram("B"), None);
        assert_eq!(parse_monogram("D12"), None);
    }
}
