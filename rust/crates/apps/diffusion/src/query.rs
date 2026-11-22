//! `DiffusionQueryConduitAPIMethod`: the methods which answer from a
//! repository's working copy, wrapped around [`QueryMethod`]s.

use phab_conduit::{ApiRequest, Application, Method, MethodFuture};

use crate::drequest::{DiffusionRequest, QUERY_ERRORS};

/// The Git half of a `DiffusionQueryConduitAPIMethod` (`getGitResult()`,
/// or `getResult()` where the method overrides it).
pub trait QueryMethod: Send + Sync {
    /// `getAPIMethodName()`.
    fn name(&self) -> &'static str;

    /// The parameters of `defineCustomParamTypes()`, then `callsign`,
    /// `repository` and `branch`.
    fn params(&self) -> &'static [&'static str];

    /// The result for a Git repository.
    fn git<'a>(
        &'a self,
        request: &'a ApiRequest<'a>,
        drequest: &'a DiffusionRequest,
    ) -> MethodFuture<'a>;
}

/// A [`QueryMethod`] as a Conduit method: the repository is resolved
/// first, as `newConduitCallProxyClient()` does.
pub struct Query<T>(pub T);

impl<T: QueryMethod> Method for Query<T> {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn params(&self) -> &'static [&'static str] {
        self.0.params()
    }

    fn allows_public(&self) -> bool {
        true
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        QUERY_ERRORS
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let drequest = DiffusionRequest::from_query(request).await?;
            self.0.git(request, &drequest).await
        })
    }
}
