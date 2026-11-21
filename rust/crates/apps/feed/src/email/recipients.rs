//! `EmailRecipient`, `EmailReviewer` and `ResolveUsers`: who an event is
//! mailed to.

use phab_auth::User;
use phab_core::Phid;
use serde_json::{Value, json};

use super::store::{ReviewerRow, RevisionRow, Store};
use super::{Fail, Result, has_prefix};

/// `PhabricatorEmailNotificationsSetting::SETTINGKEY`.
const MAIL_SETTING: &str = "no-mail";
/// `PhabricatorEmailNotificationsSetting::VALUE_MOZILLA_MAIL`.
const MOZILLA_MAIL: &str = "2";
/// The default of `email.default` (`VALUE_SEND_MAIL`).
const DEFAULT_MAIL: &str = "0";

/// PHP's `$value != '2'` for a preference value.
fn is_mozilla_mail(value: &Value) -> Result<bool> {
    Ok(match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::String(text) => match text.trim().parse::<f64>() {
            // Numeric strings compare as numbers.
            Ok(number) if !text.is_empty() && is_numeric(text) => {
                (number - 2.0).abs() < f64::EPSILON
            }
            _ => text == MOZILLA_MAIL,
        },
        Value::Number(number) => number
            .as_f64()
            .is_some_and(|number| (number - 2.0).abs() < f64::EPSILON),
        _ => return Err(Fail::Unsupported("an unusual mail preference")),
    })
}

/// A PHP numeric string, as far as the mail setting's values go.
fn is_numeric(text: &str) -> bool {
    let text = text.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let text = text.trim_end_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    !digits.is_empty()
        && digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && digits.bytes().filter(|byte| *byte == b'.').count() <= 1
        && digits != "."
}

/// The offset of a time zone now, in seconds (`DateTimeZone::getOffset()`).
fn timezone_offset(name: &str, now: i64) -> Result<i64> {
    let zone = jiff::tz::TimeZone::get(name)
        .map_err(|_| Fail::Unsupported("a time zone Rust doesn't know"))?;
    let timestamp =
        jiff::Timestamp::from_second(now).map_err(|error| Fail::Internal(error.to_string()))?;
    Ok(i64::from(zone.to_offset(timestamp).seconds()))
}

/// `EmailRecipient::from()`: `None` for disabled users and those who don't
/// want Mozilla's mail.
pub(crate) async fn recipient(
    store: &mut Store<'_>,
    user: &User,
    actor_email: &str,
) -> Result<Option<Value>> {
    if user.is_disabled {
        return Ok(None);
    }
    let preferences = store.preferences(&user.phid).await?;
    let offset = match preferences.get("timezone") {
        Some(value) if phab_core::php::is_truthy(value) => {
            let name = value
                .as_str()
                .ok_or(Fail::Unsupported("a time zone which is not a string"))?;
            timezone_offset(name, store.now)?
        }
        _ => 0,
    };
    let mail = match preferences.get(MAIL_SETTING) {
        Some(value) => value.clone(),
        None => match store.global_preferences().await? {
            Some(global) if global.contains_key(MAIL_SETTING) => global[MAIL_SETTING].clone(),
            _ => store
                .config
                .value("email.default")
                .cloned()
                .unwrap_or_else(|| json!(DEFAULT_MAIL)),
        },
    };
    if !is_mozilla_mail(&mail)? {
        return Ok(None);
    }
    let email = store
        .primary_email(&user.phid)
        .await?
        .ok_or(Fail::Throw("a recipient without a primary email address"))?;
    let is_actor = email == actor_email;
    Ok(Some(json!({
        "email": email,
        "username": user.user_name,
        "timezoneOffset": offset,
        "isActor": is_actor,
    })))
}

/// The recipients among `users`, in order.
async fn recipients_of(
    store: &mut Store<'_>,
    users: &[User],
    actor_email: &str,
) -> Result<Vec<Value>> {
    let mut recipients = Vec::new();
    for user in users {
        if let Some(recipient) = recipient(store, user, actor_email).await? {
            recipients.push(recipient);
        }
    }
    Ok(recipients)
}

/// `PhabricatorReviewer`: a user, or a project with members and watchers.
pub(crate) enum Reviewer {
    /// `UserPhabricatorReviewer`.
    User(User),
    /// `GroupPhabricatorReviewer`.
    Group {
        name: String,
        users: Vec<User>,
        watchers: Vec<User>,
    },
}

impl Reviewer {
    /// `name()`.
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::User(user) => &user.user_name,
            Self::Group { name, .. } => name,
        }
    }

    /// `toRecipients()`.
    pub(crate) async fn recipients(
        &self,
        store: &mut Store<'_>,
        actor_email: &str,
    ) -> Result<Vec<Value>> {
        match self {
            Self::User(user) => recipients_of(store, std::slice::from_ref(user), actor_email).await,
            Self::Group { users, .. } => recipients_of(store, users, actor_email).await,
        }
    }

    /// `getWatchersAsRecipients()`.
    async fn watchers(&self, store: &mut Store<'_>, actor_email: &str) -> Result<Vec<Value>> {
        match self {
            Self::User(_) => Ok(Vec::new()),
            Self::Group { watchers, .. } => recipients_of(store, watchers, actor_email).await,
        }
    }
}

/// `PhabricatorUserStore::fetchProject()`, which throws for a missing
/// project.
async fn project_users(
    store: &mut Store<'_>,
    phid: &Phid,
) -> Result<(String, Vec<User>, Vec<User>)> {
    let project = store
        .project(phid)
        .await?
        .ok_or(Fail::Throw("a project which doesn't exist"))?;
    let users = store.users(&project.members).await?;
    let watchers = store.users(&project.watchers).await?;
    Ok((project.name, users, watchers))
}

/// `PhabricatorUserStore::findReviewerByPHID()`.
pub(crate) async fn find_reviewer(store: &mut Store<'_>, phid: &Phid) -> Result<Reviewer> {
    if has_prefix(phid, "PHID-PROJ") {
        let (name, users, watchers) = project_users(store, phid).await?;
        Ok(Reviewer::Group {
            name,
            users,
            watchers,
        })
    } else if has_prefix(phid, "PHID-OPKG") {
        Ok(Reviewer::Group {
            name: String::new(),
            users: Vec::new(),
            watchers: Vec::new(),
        })
    } else {
        Ok(Reviewer::User(store.user(phid).await?))
    }
}

/// `EmailReviewer::translateStatus()`.
pub(crate) fn translate_status(status: &str) -> &'static str {
    match status {
        "accepted" => "accepted",
        "rejected" => "requested-changes",
        "blocking" => "blocking",
        _ => "unreviewed",
    }
}

/// A reviewer status as PHP compares it: a string, or what a JSON value
/// casts to.
pub(crate) fn status_text(value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        _ => Err(Fail::Unsupported("a reviewer status which is not a string")),
    }
}

/// `EmailReviewer::isActionable()`.
pub(crate) fn is_actionable(all_statuses: &[String], reviewer: &ReviewerRow) -> bool {
    let all_unreviewed = all_statuses.iter().all(|status| status == "added");
    let status = translate_status(&reviewer.status);
    // Not an accept which stands, and blocking, responded, or any review
    // is enough.
    let accepted = status == "accepted" && !reviewer.voided;
    let waiting = status != "unreviewed" || all_unreviewed;
    !accepted && waiting
}

/// `ResolveUsers`.
pub(crate) struct ResolveUsers<'r> {
    pub(crate) revision: &'r RevisionRow,
    pub(crate) actor_email: String,
}

impl ResolveUsers<'_> {
    /// `resolveAuthorAsRecipient()`.
    pub(crate) async fn author(&self, store: &mut Store<'_>) -> Result<Option<Value>> {
        let author = self
            .revision
            .author_phid
            .as_ref()
            .ok_or(Fail::Throw("a revision without author"))?;
        let author = store.user(author).await?;
        recipient(store, &author, &self.actor_email).await
    }

    /// `resolveReviewersAsRecipients()`.
    async fn reviewers_as_recipients(&self, store: &mut Store<'_>) -> Result<Vec<Value>> {
        let mut recipients = Vec::new();
        for reviewer in self.revision.unresigned_reviewers() {
            let reviewer = find_reviewer(store, &reviewer.phid).await?;
            recipients.extend(reviewer.recipients(store, &self.actor_email).await?);
        }
        Ok(recipients)
    }

    /// `resolveReviewers()`: `EmailReviewer` dictionaries.
    pub(crate) async fn reviewers(
        &self,
        store: &mut Store<'_>,
        changed_to_need_review: bool,
    ) -> Result<Vec<Value>> {
        let all_statuses: Vec<String> = self
            .revision
            .unresigned_reviewers()
            .map(|reviewer| reviewer.status.clone())
            .collect();
        let mut reviewers = Vec::new();
        for raw in self.revision.unresigned_reviewers() {
            let actionable = is_actionable(&all_statuses, raw) && changed_to_need_review;
            let status = translate_status(&raw.status);
            let reviewer = find_reviewer(store, &raw.phid).await?;
            let recipients = reviewer.recipients(store, &self.actor_email).await?;
            reviewers.push(json!({
                "name": reviewer.name(),
                "isActionable": actionable,
                "status": status,
                "recipients": recipients,
            }));
        }
        Ok(reviewers)
    }

    /// `resolveSubscribersAsRecipients()`.
    pub(crate) async fn subscribers(&self, store: &mut Store<'_>) -> Result<Vec<Value>> {
        let subscribers = store
            .edge_destinations(&self.revision.phid, phab_db::edges::OBJECT_HAS_SUBSCRIBER)
            .await?;
        let mut users = Vec::new();
        for phid in &subscribers {
            if has_prefix(phid, "PHID-PROJ") {
                let (_, members, _) = project_users(store, phid).await?;
                users.extend(members);
            } else if !has_prefix(phid, "PHID-OPKG") {
                users.push(store.user(phid).await?);
            }
        }
        let mut recipients = recipients_of(store, &users, &self.actor_email).await?;
        for reviewer in self.revision.unresigned_reviewers() {
            let reviewer = find_reviewer(store, &reviewer.phid).await?;
            recipients.extend(reviewer.watchers(store, &self.actor_email).await?);
        }
        Ok(recipients)
    }

    /// `resolveAllPossibleRecipients()`.
    pub(crate) async fn all_possible(&self, store: &mut Store<'_>) -> Result<Vec<Value>> {
        let mut recipients = self.subscribers(store).await?;
        recipients.extend(self.reviewers_as_recipients(store).await?);
        if let Some(author) = self.author(store).await? {
            recipients.push(author);
        }
        Ok(recipients)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn compares_mail_settings_loosely() {
        for (value, expected) in [
            (json!("2"), true),
            (json!(2), true),
            (json!("2.0"), true),
            (json!(" 2"), true),
            (json!(true), true),
            (json!("0"), false),
            (json!(null), false),
            (json!("2a"), false),
            (json!(false), false),
        ] {
            assert_eq!(is_mozilla_mail(&value).unwrap(), expected, "{value}");
        }
    }

    #[test]
    fn computes_offsets() {
        // 2023-11-14, winter time.
        assert_eq!(
            timezone_offset("Europe/Paris", 1_700_000_000).unwrap(),
            3600
        );
        assert_eq!(
            timezone_offset("America/New_York", 1_700_000_000).unwrap(),
            -18000
        );
        assert_eq!(timezone_offset("UTC", 1_700_000_000).unwrap(), 0);
    }
}
