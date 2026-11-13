//! What a user's profile page shows (`PhabricatorPeopleProfileViewController`
//! and the user custom fields).

use phab_core::Phid;
use phab_db::{Database, DbError, rows};

/// `PhabricatorPeopleIconSet`: key, Font Awesome icon and name. The first
/// one is the default.
const ICONS: &[(&str, &str, &str)] = &[
    ("person", "fa-user", "User"),
    ("engineering", "fa-code", "Engineering"),
    ("operations", "fa-space-shuttle", "Operations"),
    ("resources", "fa-heart", "Resources"),
    ("camera", "fa-camera-retro", "Design"),
    ("music", "fa-headphones", "Musician"),
    ("spy", "fa-user-secret", "Spy"),
    ("android", "fa-android", "Bot"),
    ("relationships", "fa-glass", "Relationships"),
    ("administration", "fa-fax", "Administration"),
    ("security", "fa-shield", "Security"),
    ("logistics", "fa-truck", "Logistics"),
    ("research", "fa-flask", "Research"),
    ("analysis", "fa-bar-chart-o", "Analysis"),
    ("executive", "fa-angle-double-up", "Executive"),
    ("animal", "fa-paw", "Animal"),
];

/// A `user_profile` row; users without one get the defaults
/// (`PhabricatorUser::loadUserProfile()`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserProfile {
    /// The title the user set, possibly empty.
    pub title: String,
    /// The blurb, in Remarkup.
    pub blurb: String,
    /// The icon key.
    pub icon: String,
}

impl UserProfile {
    /// `getDisplayTitle()`: the title, or the name of the icon.
    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            icon_name(&self.icon).unwrap_or("")
        } else {
            &self.title
        }
    }

    /// The Font Awesome class of the icon (`getIconIcon()`).
    pub fn icon_class(&self) -> Option<&'static str> {
        ICONS
            .iter()
            .find(|(key, _, _)| *key == self.icon)
            .map(|(_, icon, _)| *icon)
    }
}

/// `PhabricatorPeopleIconSet::getIconName()`.
pub fn icon_name(key: &str) -> Option<&'static str> {
    ICONS
        .iter()
        .find(|(icon, _, _)| *icon == key)
        .map(|(_, _, name)| *name)
}

/// The profile of a user, with the default icon when there is no row.
pub async fn load_profile(database: &Database, user_phid: &Phid) -> Result<UserProfile, DbError> {
    let sql = format!(
        "SELECT title, blurb, icon FROM {} WHERE userPHID = ?",
        database.table("user", "user_profile")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(user_phid.as_str())
        .fetch_optional(database.pool())
        .await?;
    let Some(row) = row else {
        return Ok(UserProfile {
            icon: ICONS[0].0.to_owned(),
            ..UserProfile::default()
        });
    };
    Ok(UserProfile {
        title: rows::text(&row, "title")?,
        blurb: rows::text(&row, "blurb")?,
        icon: rows::text(&row, "icon")?,
    })
}

/// When the account was created (`getDateCreated()`).
pub async fn date_created(database: &Database, user_phid: &Phid) -> Result<Option<i64>, DbError> {
    let sql = format!(
        "SELECT dateCreated FROM {} WHERE phid = ?",
        database.table("user", "user")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(user_phid.as_str())
        .fetch_optional(database.pool())
        .await?;
    row.map(|row| rows::int(&row, "dateCreated")).transpose()
}

/// `PhabricatorUserRolesField`: the roles shown on the profile.
pub fn roles(user: &phab_auth::User) -> Vec<&'static str> {
    [
        (user.is_admin, "Administrator"),
        (user.is_disabled, "Disabled"),
        (!user.is_approved, "Not Approved"),
        (user.is_system_agent, "Bot"),
        (user.is_mailing_list, "Mailing List"),
    ]
    .into_iter()
    .filter_map(|(has, role)| has.then_some(role))
    .collect()
}

/// `phutil_format_relative_time_detailed()`: `"2 w, 3 d"`, `"now"`, or
/// with `" ago"` for negative durations.
pub fn relative_time(duration: i64, levels: usize) -> String {
    if duration == 0 {
        return "now".to_owned();
    }
    let levels = levels.clamp(1, 5);
    let mut remainder = duration.unsigned_abs();
    let mut parts = Vec::new();
    while parts.len() < levels && (parts.is_empty() || remainder > 0) {
        let (text, rest) = format_units(remainder);
        parts.push(text);
        remainder = rest;
    }
    let mut text = parts.join(", ");
    if duration < 0 {
        text.push_str(" ago");
    }
    text
}

/// `phutil_format_units_generic()` with the time scales: the largest unit
/// and the remainder, in seconds.
fn format_units(seconds: u64) -> (String, u64) {
    const SCALES: [u64; 4] = [60, 60, 24, 7];
    const LABELS: [&str; 5] = ["s", "m", "h", "d", "w"];
    let mut value = seconds;
    let mut remainder = 0;
    let mut accumulated = 1;
    let mut unit = 0;
    for scale in SCALES {
        if value < scale {
            break;
        }
        remainder += (value % scale) * accumulated;
        value /= scale;
        accumulated *= scale;
        unit += 1;
    }
    (format!("{value} {}", LABELS[unit]), remainder)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values come from PHP's phutil_format_relative_time_detailed().
    #[test]
    fn relative_times_match_php() {
        for (duration, expected) in [
            (1, "1 s"),
            (59, "59 s"),
            (61, "1 m, 1 s"),
            (3600, "1 h"),
            (90_061, "1 d, 1 h"),
            (694_861, "1 w, 1 d"),
            (63_072_000, "104 w, 2 d"),
            (0, "now"),
            (-61, "1 m, 1 s ago"),
        ] {
            assert_eq!(relative_time(duration, 2), expected, "{duration}");
        }
    }

    #[test]
    fn titles_fall_back_to_the_icon_name() {
        let profile = UserProfile {
            icon: "engineering".to_owned(),
            ..UserProfile::default()
        };
        assert_eq!(profile.display_title(), "Engineering");
        assert_eq!(profile.icon_class(), Some("fa-code"));
    }
}
