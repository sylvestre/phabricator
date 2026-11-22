//! The build status of a revision's diff: the "Build Status" property
//! `HarbormasterUIEventListener` adds to the diff's properties, and the
//! build warnings of `DifferentialUnitField::getWarningsForDetailView()`.

use phab_app_harbormaster::web::{self as harbormaster, Artifact};
use phab_core::Phid;

use super::status::{self, Status};
use super::{Scope, method_error};
use crate::error::PageError;
use crate::state::WebState;
use crate::viewer::WebUser;

/// A build of the diff's buildable.
pub(crate) struct BuildLine {
    pub(crate) link_html: String,
    pub(crate) status: Status,
    /// The links to external build systems, as HTML.
    pub(crate) links_html: Vec<String>,
}

/// What the revision page shows of the diff's buildable.
pub(crate) struct BuildStatus {
    pub(crate) buildable_id: i64,
    pub(crate) status: Status,
    pub(crate) builds: Vec<BuildLine>,
    /// The warning above the comment form, while the diff builds or once
    /// it failed.
    pub(crate) warning: Option<&'static str>,
}

/// The automatic buildable of `diff` and its builds, as the viewer sees
/// them; `None` without one, or for viewers who can't use Harbormaster.
pub(crate) async fn build_status<S: WebState>(
    state: &S,
    viewer: &WebUser,
    diff: &Phid,
) -> Result<Option<BuildStatus>, PageError> {
    if !super::can_use(state, viewer, super::APPLICATION).await? {
        return Ok(None);
    }
    let scope = Scope::new(state, viewer);
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
    let target_phids: Vec<Phid> = targets.iter().map(|target| target.phid.clone()).collect();
    let artifacts: Vec<Artifact> = harbormaster::artifacts_of_targets(&request, &target_phids)
        .await
        .map_err(method_error)?
        .into_iter()
        .filter(|artifact| artifact.kind == "uri")
        .collect();
    let handles = super::load_handles(state, &scope, &build_phids).await?;
    let mut build_lines = Vec::new();
    for build in &builds {
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
    let warning = match buildable.status.as_str() {
        "building" => {
            Some("These changes have not finished building yet and may have build failures.")
        }
        "failed" => Some("These changes have failed to build."),
        _ => None,
    };
    Ok(Some(BuildStatus {
        buildable_id: buildable.id,
        status: status::buildable(&buildable.status),
        builds: build_lines,
        warning,
    }))
}
