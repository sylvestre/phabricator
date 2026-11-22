//! Authentication providers, as `PhabricatorAuthProvider::getAllProviders()`
//! builds them from the `auth_providerconfig` rows.
//!
//! Only the provider classes of this install exist:
//! `PhabricatorBMOAuthProvider` (moz-extensions) and
//! `PhabricatorPasswordAuthProvider`. Rows of other classes are skipped, as
//! PHP skips providers which are not installed.

use std::fmt;

use phab_core::Phid;
use phab_db::{Database, rows};
use serde_json::{Map, Value};

use crate::{AuthError, Outcome};

/// The provider class of a configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// `PhabricatorBMOAuthProvider`: `OAuth2` against Bugzilla.
    Bmo,
    /// `PhabricatorPasswordAuthProvider`: username and password.
    Password,
}

impl ProviderKind {
    fn from_class(class: &str) -> Option<Self> {
        match class {
            "PhabricatorBMOAuthProvider" => Some(Self::Bmo),
            "PhabricatorPasswordAuthProvider" => Some(Self::Password),
            _ => None,
        }
    }

    /// `getAdapterType()` of the provider's adapter.
    pub fn adapter_type(self) -> &'static str {
        match self {
            Self::Bmo => "bmo",
            Self::Password => "password",
        }
    }

    /// `getAdapterDomain()` of the provider's adapter.
    pub fn adapter_domain(self) -> &'static str {
        match self {
            Self::Bmo => "bugzilla.mozilla.org",
            Self::Password => "self",
        }
    }

    /// `getProviderKey()`: `type:domain`, which login URIs carry.
    pub fn key(self) -> &'static str {
        match self {
            Self::Bmo => "bmo:bugzilla.mozilla.org",
            Self::Password => "password:self",
        }
    }

    /// `getProviderName()`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bmo => "BMO",
            Self::Password => "Username/Password",
        }
    }

    /// `getLoginOrder()`: passwords come first.
    pub fn login_order(self) -> String {
        match self {
            Self::Bmo => format!("500-{}", self.name()),
            Self::Password => format!("100-{}", self.name()),
        }
    }

    /// `getLoginURI()`.
    pub fn login_uri(self) -> String {
        format!("/auth/login/{}/", self.key())
    }
}

/// An OAuth client secret, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientSecret(String);

impl ClientSecret {
    /// Wraps a secret.
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The secret, to send to the provider.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ClientSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClientSecret(<redacted>)")
    }
}

/// A row of `auth_providerconfig` with an installed provider class.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    /// The configuration PHID; external accounts point to it.
    pub phid: Phid,
    /// The provider class.
    pub kind: ProviderKind,
    /// `isEnabled`.
    pub is_enabled: bool,
    /// `shouldAllowLogin`.
    pub allow_login: bool,
    /// `shouldAllowRegistration`, without the login check.
    allow_registration: bool,
    /// `shouldAllowLink`.
    pub allow_link: bool,
    /// `shouldAutoLogin`.
    pub auto_login: bool,
    /// `oauth:app:id` (`PhabricatorOAuth2AuthProvider::PROPERTY_APP_ID`).
    pub oauth_client_id: Option<String>,
    /// `oauth:app:secret`.
    pub oauth_client_secret: Option<ClientSecret>,
}

impl ProviderConfig {
    /// `shouldAllowRegistration()`: registration needs login too.
    pub fn allows_registration(&self) -> bool {
        self.allow_login && self.allow_registration
    }
}

/// A property as a string, like `getProperty()` read with `%s`.
fn property(properties: &Map<String, Value>, key: &str) -> Option<String> {
    match properties.get(key) {
        None | Some(Value::Null) => None,
        Some(value) => Some(phab_core::php::string_cast(value).into_owned()),
    }
}

/// `PhabricatorAuthProvider::getAllProviders()`: every configuration of an
/// installed class, in the query's order (newest first). Two of the same
/// class share a provider key, where PHP throws.
pub async fn load_providers(
    database: &Database,
) -> Result<Outcome<Vec<ProviderConfig>>, AuthError> {
    let sql = format!(
        "SELECT phid, providerClass, isEnabled, shouldAllowLogin, shouldAllowRegistration, \
           shouldAllowLink, shouldAutoLogin, properties FROM {} ORDER BY id DESC",
        database.table("auth", "auth_providerconfig")
    );
    let found = phab_db::sqlx::query(&sql)
        .fetch_all(database.pool())
        .await?;
    let mut providers: Vec<ProviderConfig> = Vec::new();
    for row in &found {
        let Some(kind) = ProviderKind::from_class(&rows::text(row, "providerClass")?) else {
            continue;
        };
        if providers.iter().any(|provider| provider.kind == kind) {
            return Ok(Outcome::Unsupported("two providers share a provider key"));
        }
        let properties = match rows::json(row, "properties")? {
            Value::Object(properties) => properties,
            _ => Map::new(),
        };
        providers.push(ProviderConfig {
            phid: Phid::new(rows::text(row, "phid")?),
            kind,
            is_enabled: rows::flag(row, "isEnabled")?,
            allow_login: rows::flag(row, "shouldAllowLogin")?,
            allow_registration: rows::flag(row, "shouldAllowRegistration")?,
            allow_link: rows::flag(row, "shouldAllowLink")?,
            auto_login: rows::flag(row, "shouldAutoLogin")?,
            oauth_client_id: property(&properties, "oauth:app:id"),
            oauth_client_secret: property(&properties, "oauth:app:secret").map(ClientSecret),
        });
    }
    Ok(Outcome::Done(providers))
}

/// `getEnabledProviderByKey()`.
pub fn enabled_by_key<'a>(
    providers: &'a [ProviderConfig],
    key: &str,
) -> Option<&'a ProviderConfig> {
    providers
        .iter()
        .find(|provider| provider.is_enabled && provider.kind.key() == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_uris_match_php() {
        assert_eq!(ProviderKind::Bmo.key(), "bmo:bugzilla.mozilla.org");
        assert_eq!(
            ProviderKind::Bmo.login_uri(),
            "/auth/login/bmo:bugzilla.mozilla.org/"
        );
        assert_eq!(
            ProviderKind::Password.login_uri(),
            "/auth/login/password:self/"
        );
        assert!(ProviderKind::Password.login_order() < ProviderKind::Bmo.login_order());
    }

    #[test]
    fn secrets_are_not_printed() {
        assert_eq!(
            format!("{:?}", ClientSecret::new("hunter2")),
            "ClientSecret(<redacted>)"
        );
    }
}
