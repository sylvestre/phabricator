//! `diffusion.filecontentquery` (`DiffusionGitFileContentQuery`): a file's
//! content at a commit, stored as a temporary file
//! ([`crate::file_future`]).
//!
//! Ported for Git: every parameter. Left to PHP: what
//! [`crate::file_future`] leaves to it, and values of unusual types.

use std::sync::Arc;

use phab_app_files::mime::MimeCommand;
use phab_conduit::{ApiRequest, MethodFuture};

use crate::drequest::DiffusionRequest;
use crate::file_future::{self, Limits};
use crate::query::QueryMethod;

/// `diffusion.filecontentquery`.
pub struct FileContentQuery {
    mime: Option<Arc<MimeCommand>>,
}

impl FileContentQuery {
    /// The method, detecting MIME types with `mime`.
    pub fn new(mime: Option<Arc<MimeCommand>>) -> Self {
        Self { mime }
    }
}

impl QueryMethod for FileContentQuery {
    fn name(&self) -> &'static str {
        "diffusion.filecontentquery"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "path",
            "commit",
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
            let limits = Limits::from_request(request)?;
            let path = drequest.path()?;
            let commit = drequest.commit(request).await?;
            // `cat-file blob -- %s` of `commit:path`, after `--`: no
            // selector check.
            let argv = vec![
                "cat-file".to_owned(),
                "blob".to_owned(),
                "--".to_owned(),
                format!("{commit}:{path}"),
            ];
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
