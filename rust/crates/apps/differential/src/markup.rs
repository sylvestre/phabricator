//! The remarkup engine's options, which the pages and the editor share.

use std::collections::BTreeSet;

use phab_core::Config;

/// The engine options from the configuration
/// (`getMarkupEngineDefaultConfiguration()`), without syntax highlighting.
pub fn engine_config(config: &Config) -> phab_remarkup::Config {
    let allowed_protocols = config
        .object("uri.allowed-protocols")
        .map(|protocols| {
            protocols
                .iter()
                .filter(|(_, enabled)| phab_core::php::is_truthy(enabled))
                .map(|(protocol, _)| protocol.clone())
                .collect()
        })
        .unwrap_or_default();
    let string = |key: &str| config.nonempty_string(key).map(str::to_owned);
    phab_remarkup::Config {
        allowed_protocols,
        base_uri: string("phabricator.base-uri"),
        production_uri: string("phabricator.production-uri"),
        ignored_object_names: string("remarkup.ignored-object-names"),
        bugzilla_url: string("bugzilla.url"),
        self_hosts: self_hosts(config),
        youtube: config.truthy("remarkup.enable-embedded-youtube"),
        ..phab_remarkup::phabricator::default_config()
    }
}

/// `PhabricatorEnv::getSelfURIMap()`: the hosts of the base and production
/// URIs and of `phabricator.allowed-uris`, lowercased.
fn self_hosts(config: &Config) -> BTreeSet<String> {
    let mut uris: Vec<String> = ["phabricator.production-uri", "phabricator.base-uri"]
        .iter()
        .filter_map(|key| config.nonempty_string(key).map(str::to_owned))
        .collect();
    if let Some(allowed) = config
        .value("phabricator.allowed-uris")
        .and_then(|value| value.as_array())
    {
        uris.extend(
            allowed
                .iter()
                .filter_map(|uri| uri.as_str().map(str::to_owned)),
        );
    }
    uris.iter()
        .filter_map(|uri| {
            let (_, rest) = uri.split_once("://")?;
            let authority = rest.split(['/', '?', '#']).next()?;
            let host = authority.rsplit('@').next()?;
            let host = host.split(':').next()?;
            (!host.is_empty()).then(|| host.to_lowercase())
        })
        .collect()
}
