//! The frame around every page: header, user menu, MOTD and the link back
//! to the old interface (`PhabricatorStandardPageView`, without its HTML).

use phab_core::Config;

use crate::viewer::WebUser;
use crate::{assets, opt_in};

/// The message of the day of moz-extensions `MozillaMOTD`, shown above
/// every page while `moz-motd.title` is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Motd {
    /// `moz-motd.title`.
    pub title: String,
    /// `moz-motd.message`. PHP passes it to `hsprintf()` as the format, so
    /// it is trusted HTML written by administrators.
    pub message_html: String,
}

impl Motd {
    /// The configured message, if any.
    pub fn from_config(config: &Config) -> Option<Self> {
        let title = config.nonempty_string("moz-motd.title")?;
        Some(Self {
            title: title.to_owned(),
            message_html: config.string("moz-motd.message").unwrap_or("").to_owned(),
        })
    }
}

/// The logged-in user, as the header shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuUser {
    /// The username.
    pub user_name: String,
    /// The real name, possibly empty.
    pub real_name: String,
}

/// What the page frame shows.
#[derive(Debug, Clone)]
pub struct Shell {
    /// The page title, without the site name.
    pub title: String,
    /// The logged-in user, if any.
    pub user: Option<MenuUser>,
    /// A CSRF token for the forms of the page.
    pub csrf_token: Option<String>,
    /// The message of the day.
    pub motd: Option<Motd>,
    /// Where the stylesheet is served.
    pub stylesheet: String,
    /// Where the "old interface" link goes.
    pub old_ui_uri: Option<String>,
}

impl Shell {
    /// The frame of a page shown to a logged-in user. `current_uri` is the
    /// path and query of the request, which the old interface link reopens.
    pub fn for_user(config: &Config, viewer: &WebUser, title: &str, current_uri: &str) -> Self {
        Self {
            title: title.to_owned(),
            user: Some(MenuUser {
                user_name: viewer.user.user_name.clone(),
                real_name: viewer.user.real_name.clone(),
            }),
            csrf_token: Some(viewer.csrf_token.clone()),
            motd: Motd::from_config(config),
            stylesheet: assets::uri("phab.css"),
            old_ui_uri: Some(opt_in::off_uri(current_uri)),
        }
    }

    /// A frame without a user, for error pages built before or without
    /// one.
    pub fn bare(title: &str) -> Self {
        Self {
            title: title.to_owned(),
            user: None,
            csrf_token: None,
            motd: None,
            stylesheet: assets::uri("phab.css"),
            old_ui_uri: None,
        }
    }
}
