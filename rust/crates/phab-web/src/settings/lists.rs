//! The list panels: email addresses, authentication factors, Conduit API
//! tokens and sessions. Their actions (the links of the lists) and any
//! post are PHP's.

use axum::response::Response;
use phab_app_people::profile::relative_time;
use phab_app_settings::account;

use super::{Content, PanelRequest};
use crate::error::PageError;
use crate::state::WebState;
use crate::time;

/// `phabricator_date()`, in UTC like every date of the Rust pages.
fn date(epoch: i64) -> String {
    time::format_utc(epoch)[..10].to_owned()
}

/// Hands the request to PHP when it asks for one of the panel's actions.
fn require_list<S: WebState>(
    panel: &PanelRequest<'_, S>,
    actions: &[&'static str],
) -> Result<(), PageError> {
    if panel.request.is_post() {
        return Err(PageError::Fallback("an action of a settings panel"));
    }
    if actions.iter().any(|action| panel.request.exists(action)) {
        return Err(PageError::Fallback("an action of a settings panel"));
    }
    Ok(())
}

/// An address of the list.
pub(crate) struct EmailView {
    pub(super) address: String,
    pub(super) id: i64,
    pub(super) is_verified: bool,
    pub(super) is_primary: bool,
}

/// `PhabricatorEmailAddressesSettingsPanel`.
pub(crate) struct EmailsView {
    pub(super) action: String,
    pub(super) editable: bool,
    pub(super) addresses: Vec<EmailView>,
}

pub(super) async fn email_addresses<S: WebState>(
    panel: PanelRequest<'_, S>,
) -> Result<Response, PageError> {
    require_list(&panel, &["new", "delete", "verify", "primary"])?;
    let addresses =
        account::email_addresses(panel.state.database(), &panel.viewer.user.phid).await?;
    let view = EmailsView {
        action: panel.uri(),
        editable: panel.state.config().truthy("account.editable"),
        addresses: addresses
            .into_iter()
            .map(|address| EmailView {
                address: address.address,
                id: address.id,
                is_verified: address.is_verified,
                is_primary: address.is_primary,
            })
            .collect(),
    };
    Ok(panel.render(Content {
        emails: Some(view),
        ..Content::default()
    }))
}

/// A factor of the list.
pub(crate) struct FactorView {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) status: &'static str,
    pub(super) status_icon: &'static str,
    pub(super) provider: String,
    pub(super) details: String,
    pub(super) created: String,
    pub(super) highlighted: bool,
}

/// `PhabricatorMultiFactorSettingsPanel`.
pub(crate) struct FactorsView {
    pub(super) action: String,
    pub(super) can_add: bool,
    pub(super) factors: Vec<FactorView>,
}

pub(super) async fn factors<S: WebState>(
    panel: PanelRequest<'_, S>,
) -> Result<Response, PageError> {
    require_list(&panel, &["new", "providerPHID", "edit", "delete"])?;
    let (factors, can_add) =
        account::factors(panel.state.database(), &panel.viewer.user.phid).await?;
    let highlight = panel
        .request
        .str("id")
        .map(phab_app_settings::preferences::int_cast_str);
    let view = FactorsView {
        action: panel.uri(),
        can_add,
        factors: factors
            .into_iter()
            .map(|factor| FactorView {
                highlighted: highlight == Some(factor.id),
                id: factor.id,
                name: factor.name,
                status: factor.status.name(),
                status_icon: match factor.status {
                    account::ProviderStatus::Active => "fa-check green",
                    account::ProviderStatus::Deprecated => "fa-ban indigo",
                    account::ProviderStatus::Disabled => "fa-times grey",
                    account::ProviderStatus::Unknown => "fa-question",
                },
                provider: factor.provider_name,
                details: factor.details,
                created: time::format_utc(factor.created),
            })
            .collect(),
    };
    Ok(panel.render(Content {
        factors: Some(view),
        ..Content::default()
    }))
}

/// A token of the list.
pub(crate) struct TokenView {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) type_name: String,
    pub(super) created: String,
    pub(super) expires: String,
}

/// `PhabricatorConduitTokensSettingsPanel`.
pub(crate) struct TokensView {
    pub(super) user_phid: String,
    pub(super) tokens: Vec<TokenView>,
}

pub(super) async fn conduit_tokens<S: WebState>(
    panel: PanelRequest<'_, S>,
) -> Result<Response, PageError> {
    require_list(&panel, &[])?;
    let tokens = account::conduit_tokens(
        panel.state.database(),
        &panel.viewer.user.phid,
        panel.state.now(),
    )
    .await?;
    let view = TokensView {
        user_phid: panel.viewer.user.phid.as_str().to_owned(),
        tokens: tokens
            .into_iter()
            .map(|token| TokenView {
                id: token.id,
                name: token.name,
                type_name: token.type_name,
                created: time::format_utc(token.created),
                expires: token
                    .expires
                    .map_or_else(|| "Never".to_owned(), time::format_utc),
            })
            .collect(),
    };
    Ok(panel.render(Content {
        tokens: Some(view),
        ..Content::default()
    }))
}

/// A session of the list.
pub(crate) struct SessionView {
    pub(super) id: i64,
    pub(super) key_prefix: String,
    pub(super) session_type: String,
    pub(super) high_security: Option<String>,
    pub(super) created: String,
    pub(super) expires: String,
    pub(super) is_current: bool,
}

/// `PhabricatorSessionsSettingsPanel`.
pub(crate) struct SessionsView {
    pub(super) user_name: String,
    pub(super) in_high_security: bool,
    pub(super) sessions: Vec<SessionView>,
}

pub(super) async fn sessions<S: WebState>(
    panel: PanelRequest<'_, S>,
) -> Result<Response, PageError> {
    require_list(&panel, &[])?;
    let database = panel.state.database();
    let user = &panel.viewer.user.phid;
    let now = panel.state.now();
    let sessions = account::sessions(database, user).await?;
    let view = SessionsView {
        user_name: panel.viewer.user.user_name.clone(),
        in_high_security: panel
            .viewer
            .session
            .high_security_until
            .is_some_and(|until| until > now),
        sessions: sessions
            .into_iter()
            .map(|session| SessionView {
                is_current: session.id == panel.viewer.session.id,
                id: session.id,
                key_prefix: session.key_prefix,
                session_type: session.session_type,
                high_security: session
                    .high_security_until
                    .map(|until| until - now)
                    .filter(|remaining| *remaining > 0)
                    .map(|remaining| relative_time(remaining, 1)),
                created: time::format_utc(session.start),
                expires: date(session.expires),
            })
            .collect(),
    };
    Ok(panel.render(Content {
        sessions: Some(view),
        ..Content::default()
    }))
}
