//! Read side of `PhabricatorUserProfileImageCacheType`.
//!
//! PHP caches each user's profile image URI in `user_cache`, prefixed with a
//! version derived from the CDN URI, `cluster.instance` and the profile image
//! PHID. Rust only reads valid entries: building a missing one means loading
//! files and possibly composing an avatar, which PHP does.

use phab_auth::User;
use phab_core::{Config, hash};
use phab_db::phpser::Value;
use phab_db::{Database, DbError, phpser, rows};

pub const CACHE_KEY: &str = "user.profile.image.uri.v1";
pub const CACHE_TYPE: &str = "user.profile";

/// `PhabricatorUserProfileImageCacheType::getCacheVersion()`.
pub fn cache_version(
    config: &Config,
    user: &User,
) -> Result<String, phab_core::config::ConfigError> {
    let parts = Value::list([
        Value::str(&config.cdn_uri("/")?),
        Value::from(config.string("cluster.instance")),
        Value::from(
            user.profile_image_phid
                .as_ref()
                .map(phab_core::Phid::as_str),
        ),
    ]);
    Ok(hash::digest_for_index(&phpser::serialize(&parts)))
}

/// The cached URI if the cache holds a valid entry for `user`.
pub async fn cached_uri(
    database: &Database,
    config: &Config,
    user: &User,
) -> Result<Option<String>, DbError> {
    let sql = format!(
        "SELECT cacheData, cacheType FROM {} WHERE cacheIndex IN (?) AND userPHID IN (?)",
        database.table("user", "user_cache")
    );
    let rows = phab_db::sqlx::query(&sql)
        .bind(hash::digest_for_index(CACHE_KEY.as_bytes()))
        .bind(user.phid.as_str())
        .fetch_all(database.pool())
        .await?;
    let Ok(version) = cache_version(config, user) else {
        return Ok(None);
    };
    for row in rows {
        if rows::text(&row, "cacheType")? != CACHE_TYPE {
            continue;
        }
        let Some(data) = rows::opt_text(&row, "cacheData")? else {
            continue;
        };
        let mut parts = data.splitn(2, ',');
        let stored_version = parts.next().unwrap_or_default();
        if stored_version == version {
            // getValueFromStorage(): the last part, which is the whole value
            // when there is no comma.
            return Ok(Some(parts.next().unwrap_or(stored_version).to_owned()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use phab_core::Phid;
    use serde_json::json;

    use super::*;

    fn user(profile_image_phid: Option<&str>) -> User {
        User {
            id: 1,
            phid: Phid::new("PHID-USER-alice"),
            user_name: "alice".to_owned(),
            real_name: String::new(),
            profile_image_phid: profile_image_phid.map(Phid::new),
            is_system_agent: false,
            is_disabled: false,
            is_admin: false,
            is_email_verified: true,
            is_approved: true,
            is_mailing_list: false,
        }
    }

    #[test]
    fn versions_match_php() {
        let mut config = Config::default();
        config.push_layer(
            "test",
            serde_json::Map::from_iter([(
                "phabricator.base-uri".to_owned(),
                json!("http://phabricator.test/"),
            )]),
        );
        // PhabricatorHash::digestForIndex(phpser::serialize(
        //   array('http://phabricator.test/', null, null)))
        assert_eq!(cache_version(&config, &user(None)).unwrap(), "fQDBd9WAdWy_");
        // ... array('http://phabricator.test/', null, 'PHID-FILE-abc')
        assert_eq!(
            cache_version(&config, &user(Some("PHID-FILE-abc"))).unwrap(),
            "Mt_KY4tJ8.e9"
        );
    }
}
