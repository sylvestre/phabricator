//! `diffusion.rawdiffquery` (`DiffusionGitRawDiffQuery`): the diff of a
//! commit (or of two commits) at an optional path, stored as a temporary
//! file ([`crate::file_future`]).
//!
//! Ported for Git: every parameter. Left to PHP: what
//! [`crate::file_future`] leaves to it, and values of unusual types.

use std::sync::Arc;
use std::time::Duration;

use phab_app_files::mime::MimeCommand;
use phab_conduit::{ApiRequest, MethodError, MethodFuture};

use crate::drequest::DiffusionRequest;
use crate::file_future::{self, Limits};
use crate::query::QueryMethod;
use crate::{git, php};

/// `ArcanistGitAPI::GIT_MAGIC_ROOT_COMMIT`, the empty tree.
const GIT_MAGIC_ROOT_COMMIT: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// `DiffusionRawDiffQuery`'s default lines of context.
pub const DEFAULT_CONTEXT: i64 = 65535;

/// What `DiffusionRawDiffQuery` diffs.
pub struct RawDiff<'a> {
    /// The anchor commit (`getAnchorCommit()`).
    pub commit: &'a str,
    /// The commit to diff against; `None` for the commit's parent.
    pub against: Option<&'a str>,
    /// `getLinesOfContext()`.
    pub lines_of_context: i64,
    /// The path, `.` when empty.
    pub path: &'a str,
}

impl RawDiff<'_> {
    /// `newQueryFuture()`: the argument list of the diff, after finding
    /// the parent when there is nothing to diff against.
    pub async fn argv(&self, drequest: &DiffusionRequest) -> Result<Vec<String>, MethodError> {
        let commit = git::ref_selector(self.commit)?;
        let against = if let Some(against) = self.against {
            git::ref_selector(against)?
        } else {
            let parents = drequest
                .copy
                .execx(
                    vec![
                        "log".to_owned(),
                        "-n".to_owned(),
                        "1".to_owned(),
                        "--format=%P".to_owned(),
                        commit.clone(),
                        "--".to_owned(),
                    ],
                    None,
                )
                .await?;
            if php::trim(&php::text(parents)?).is_empty() {
                GIT_MAGIC_ROOT_COMMIT.to_owned()
            } else {
                format!("{commit}^")
            }
        };
        let path = if self.path.is_empty() { "." } else { self.path };
        Ok([
            "diff",
            "-M",
            "-C",
            "--no-ext-diff",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            &format!("-U{}", self.lines_of_context),
            &against,
            &commit,
            "--",
            path,
        ]
        .map(str::to_owned)
        .to_vec())
    }

    /// `executeInline()`: the diff, which must succeed.
    pub async fn execute_inline(
        &self,
        drequest: &DiffusionRequest,
        timeout: Duration,
    ) -> Result<Vec<u8>, MethodError> {
        let argv = self.argv(drequest).await?;
        file_future::run(drequest, argv, timeout).await
    }
}

/// `diffusion.rawdiffquery`.
pub struct RawDiffQuery {
    mime: Option<Arc<MimeCommand>>,
}

impl RawDiffQuery {
    /// The method, detecting MIME types with `mime`.
    pub fn new(mime: Option<Arc<MimeCommand>>) -> Self {
        Self { mime }
    }
}

impl QueryMethod for RawDiffQuery {
    fn name(&self) -> &'static str {
        "diffusion.rawdiffquery"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "commit",
            "path",
            "linesOfContext",
            "againstCommit",
            "timeout",
            "byteLimit",
            "callsign",
            "repository",
            "branch",
        ]
    }

    fn git<'a>(
        &'a self,
        request: &'a ApiRequest<'a>,
        drequest: &'a DiffusionRequest,
    ) -> MethodFuture<'a> {
        Box::pin(async move {
            let lines_of_context = match request.value("linesOfContext") {
                None | Some(serde_json::Value::Null) => DEFAULT_CONTEXT,
                value => php::int_cast(value)?,
            };
            let against = php::optional_string(request.value("againstCommit"))?;
            let limits = Limits::from_request(request)?;
            let path = drequest.path()?;
            let commit = drequest.stable_commit(request).await?;
            let diff = RawDiff {
                commit: &commit,
                against: against.as_deref(),
                lines_of_context,
                path: &path,
            };
            let argv = diff.argv(drequest).await?;
            let data = file_future::run(drequest, argv, limits.timeout).await?;
            let name = file_future::basename(&path);
            file_future::respond(
                request,
                drequest,
                self.mime.as_deref(),
                (name, &data),
                limits,
            )
            .await
        })
    }
}
