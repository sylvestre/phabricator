//! The feed story of an edit: `publishFeedStory()` of
//! `PhabricatorApplicationTransactionEditor` and what
//! `PhabricatorFeedStoryPublisher::publish()` makes of it, before the
//! chronological key is drawn.

use indexmap::IndexSet;
use phab_app_metamta::actors;
use phab_core::{Config, Phid, php};
use phab_db::Database;
use serde_json::{Map, Value, json};

use crate::Error;
use crate::state::State;
use crate::xaction::{self, Xaction};

/// `getFeedStoryType()`.
pub(crate) const STORY_TYPE: &str = "PhabricatorApplicationTransactionFeedStory";

/// `PhabricatorEmailTagsSetting::VALUE_IGNORE`.
const MAIL_TAG_IGNORE: i64 = 2;

/// A story to publish.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Story {
    /// `storyData`.
    pub(crate) data: Value,
    /// `authorPHID`.
    pub(crate) author: String,
    /// The objects referencing it (`feed_storyreference`), each once.
    pub(crate) related: Vec<String>,
    /// The primary object of the notifications.
    pub(crate) primary: String,
    /// `feed_storynotification`: the users notified, and whether the mail
    /// they get marks it read.
    pub(crate) notifications: Vec<(String, bool)>,
}

/// What the story needs.
pub(crate) struct Context<'a> {
    pub(crate) database: &'a Database,
    pub(crate) config: &'a Config,
    pub(crate) object_phid: &'a str,
    pub(crate) xactions: &'a [Xaction],
    pub(crate) state: &'a State,
    /// `getActingAsPHID()`.
    pub(crate) actor: &'a str,
}

/// `getFeedStoryData()`: the object and the transactions by strength,
/// keyed by PHID.
fn story_data(object_phid: &str, xactions: &[&Xaction]) -> Result<Value, Error> {
    let mut phids = Map::new();
    for index in xaction::by_strength(xactions)? {
        let phid = xactions[index].row.phid.as_str();
        phids.insert(phid.to_owned(), json!(phid));
    }
    Ok(json!({
        "objectPHID": object_phid,
        "transactionPHIDs": php::round_trip(&Value::Object(phids)),
    }))
}

/// `publishFeedStory()`, `mailed` the recipients of the mail; `None` when
/// every transaction is hidden from feed and notifications.
pub(crate) async fn build(cx: &Context<'_>, mailed: &[String]) -> Result<Option<Story>, Error> {
    let mut shown = Vec::new();
    for xaction in cx.xactions {
        if !xaction.hidden_for_feed()? {
            shown.push(xaction);
        }
    }
    if shown.is_empty() {
        return Ok(None);
    }
    let related = if cx.state.flag("feedRelatedPHIDs") {
        let mut unique = IndexSet::new();
        unique.extend(cx.state.list("feedRelatedPHIDs")?);
        unique.into_iter().collect()
    } else {
        Vec::new()
    };
    let muted = cx.state.lenient_list("mailMutedPHIDs")?;
    let mut subscribed: IndexSet<String> = cx.state.list("feedNotifyPHIDs")?.into_iter().collect();
    subscribed.retain(|phid| !muted.contains(phid));
    let unexpandable = cx.state.lenient_list("mailUnexpandablePHIDs")?;

    let mut tags = Vec::new();
    for xaction in &shown {
        tags.push(xaction.mail_tags());
    }
    let notifications = if subscribed.is_empty() {
        Vec::new()
    } else {
        let subscribed: Vec<String> = subscribed.into_iter().collect();
        let users = filter_subscribed(cx, &subscribed, &unexpandable, &tags).await?;
        users
            .into_iter()
            .map(|phid| {
                let read = mailed.contains(&phid);
                (phid, read)
            })
            .collect()
    };
    Ok(Some(Story {
        data: story_data(cx.object_phid, &shown)?,
        author: cx.actor.to_owned(),
        related,
        primary: cx.object_phid.to_owned(),
        notifications,
    }))
}

/// `filterSubscribedPHIDs()`: the recipients expanded (without the
/// unexpandable ones expansion brings), without the author and those who
/// ignore every tag of the story.
async fn filter_subscribed(
    cx: &Context<'_>,
    phids: &[String],
    unexpandable: &[String],
    tags: &[&str],
) -> Result<Vec<String>, Error> {
    // `PhabricatorMetaMTAMemberQuery::executeExpansion()`.
    let map = actors::expansion_map(cx.database, phids).await?;
    let mut expanded: IndexSet<String> = IndexSet::new();
    for phid in phids {
        for member in map.get(phid).into_iter().flatten() {
            expanded.insert(member.clone());
        }
    }
    let expanded: Vec<String> = expanded
        .into_iter()
        .filter(|phid| phids.contains(phid) || !unexpandable.contains(phid))
        .collect();

    // Synthetic preferences for the users among them.
    let users: Vec<Phid> = expanded.iter().map(Phid::new).collect();
    let loaded: Vec<String> = phab_auth::user::load_users_by_phid(cx.database, &users)
        .await?
        .into_iter()
        .map(|user| user.phid.as_str().to_owned())
        .collect();
    let loaded_refs: Vec<&str> = loaded.iter().map(String::as_str).collect();
    let global = phab_app_settings::preferences::load_global(cx.database).await?;
    let preferences = actors::user_preferences(cx.database, &loaded_refs, global.as_ref()).await?;

    let mut keep: IndexSet<String> = IndexSet::new();
    for phid in expanded {
        if phid == cx.actor {
            continue;
        }
        if !tags.is_empty()
            && let Some(preferences) = preferences.get(&phid)
        {
            let mail_tags = preferences.value(phab_app_settings::setting::MAIL_TAGS, cx.config);
            let mut notify = false;
            for tag in tags {
                let value = match &mail_tags {
                    Value::Object(map) => map.get(*tag),
                    Value::Array(list) => {
                        tag.parse::<usize>().ok().and_then(|index| list.get(index))
                    }
                    _ => {
                        return Err(Error::unsupported(
                            "mail tag settings which are not an array",
                        ));
                    }
                };
                let value = value.map_or(
                    phab_app_settings::mail_tags::VALUE_EMAIL,
                    phab_app_settings::preferences::int_cast,
                );
                if value != MAIL_TAG_IGNORE {
                    notify = true;
                    break;
                }
            }
            if !notify {
                continue;
            }
        }
        keep.insert(phid);
    }
    Ok(keep.into_iter().collect())
}
