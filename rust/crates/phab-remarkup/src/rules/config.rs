//! `PhabricatorConfigRemarkupRule`: `@{config:key}` names a configuration
//! option.

use crate::engine::Context;
use crate::generated::config_keys::CONFIG_KEYS;
use crate::markup::{self, Content, Markup};
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};
use crate::uri;

static CONFIG: Pattern = Pattern::new(r"@\{config:([^}]+)\}");

/// `PhabricatorConfigRemarkupRule`.
pub struct ConfigLink;

/// Whether `PhabricatorEnv::getEnvConfig()` knows the key: the options PHP
/// defines.
fn is_option(key: &[u8]) -> bool {
    std::str::from_utf8(key).is_ok_and(|key| CONFIG_KEYS.binary_search(&key).is_ok())
}

impl InlineRule for ConfigLink {
    fn priority(&self) -> f64 {
        199.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(&CONFIG, text.as_bytes(), |groups| {
            let key = groups.get(1);
            if !rule::is_flat_text(groups.get(0)) || !is_option(key) {
                return groups.get(0).to_vec();
            }
            if cx.is_text_mode() || cx.is_html_mail_mode() {
                return [b"\"", key, b"\""].concat();
            }
            let href = [b"/config/edit/", &uri::escape_uri(key)[..], b"/"].concat();
            cx.store_text(markup::tag(
                "a",
                &[("href", Some(&href)), ("target", Some(b"_blank"))],
                Content::One(&Markup::text(key)),
            ))
        }))
    }
}
