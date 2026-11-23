//! `PhabricatorApplicationTransactionPublishWorker::doWork()`: load the
//! object and the transactions, then `publishTransactions()`.

use std::collections::HashMap;

use phab_app_differential::storage;
use phab_core::{Config, Phid, PhidType, php};
use phab_db::sqlx::MySqlConnection;
use phab_db::{Database, DbError, edges, rows, worker};
use serde_json::{Map, Value, json};

use crate::Error;
use crate::feed::{self, Story};
use crate::mail::{self, Message};
use crate::state::State;
use crate::xaction::Xaction;

/// `PhabricatorMetaMTAMailHasRecipientEdgeType`.
const MAIL_HAS_RECIPIENT: i64 = 57;
/// `PhabricatorWorker::PRIORITY_INDEX`.
const PRIORITY_INDEX: i64 = 3500;
/// `PhabricatorUserNotificationCountCacheType::KEY_COUNT`.
const NOTIFICATION_COUNT_CACHE: &str = "user.notification.count.v1";
/// `HeraldWebhook::HOOKSTATUS_FIREHOSE`.
const FIREHOSE: &str = "firehose";

/// What the task does.
#[derive(Debug)]
pub enum Plan {
    /// `PhabricatorWorkerPermanentFailureException` with this message.
    Fail(String),
    /// Publish.
    Publish(Box<Publication>),
}

/// The rows `publishTransactions()` writes.
#[derive(Debug)]
pub struct Publication {
    object_phid: String,
    search_data: String,
    story: Option<Story>,
    mails: Vec<Message>,
}

impl Publication {
    /// How many mails it saves.
    pub fn mail_count(&self) -> usize {
        self.mails.len()
    }

    /// Whether it publishes a feed story.
    pub fn has_story(&self) -> bool {
        self.story.is_some()
    }
}

/// `idx($data, $key)` as a list of transaction PHIDs: falsy is none.
fn xaction_phids(data: &Map<String, Value>) -> Result<Vec<String>, Error> {
    let value = data.get("xactionPHIDs").unwrap_or(&Value::Null);
    if !php::is_truthy(value) {
        return Ok(Vec::new());
    }
    let bad = || Error::unsupported("transaction PHIDs which are not a list");
    let Value::Array(items) = value else {
        return Err(bad());
    };
    let mut phids: Vec<String> = Vec::new();
    for item in items {
        let phid = item.as_str().ok_or_else(bad)?;
        // `array_select_keys()` keeps each PHID once.
        if !phids.iter().any(|seen| seen == phid) {
            phids.push(phid.to_owned());
        }
    }
    Ok(phids)
}

/// `loadTransactions()`: the transactions in the task's order, with their
/// comments; `Err(Ok(message))` when PHP fails the task.
async fn load_transactions(
    database: &Database,
    object_phid: &str,
    phids: &[String],
) -> Result<Result<Vec<Xaction>, String>, Error> {
    if phids.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    let wanted: Vec<Phid> = phids.iter().map(Phid::new).collect();
    let mut loaded: HashMap<String, storage::Transaction> =
        storage::load_transactions(database, &wanted)
            .await?
            .into_iter()
            .map(|row| (row.phid.as_str().to_owned(), row))
            .collect();
    let missing: Vec<&str> = phids
        .iter()
        .map(String::as_str)
        .filter(|phid| !loaded.contains_key(*phid))
        .collect();
    if !missing.is_empty() {
        return Ok(Err(format!(
            "Unable to load transactions: {}.",
            missing.join(", ")
        )));
    }
    let comment_phids: Vec<Phid> = loaded
        .values()
        .filter_map(|row| row.comment_phid.clone())
        .collect();
    let mut comments: HashMap<String, storage::Comment> =
        storage::load_comments(database, &comment_phids)
            .await?
            .into_iter()
            .map(|comment| (comment.phid.as_str().to_owned(), comment))
            .collect();
    let mut xactions = Vec::new();
    for phid in phids {
        let row = loaded.remove(phid).expect("checked above");
        // The mail reloads them for each recipient, under the object and
        // their own policies.
        if row.object_phid.as_str() != object_phid {
            return Err(Error::unsupported("a transaction of another object"));
        }
        if row.view_policy != "public" {
            return Err(Error::unsupported("a transaction which is not public"));
        }
        let comment = row
            .comment_phid
            .as_ref()
            .and_then(|phid| comments.remove(phid.as_str()));
        if comment
            .as_ref()
            .is_some_and(|comment| comment.view_policy != "public")
        {
            return Err(Error::unsupported("a comment which is not public"));
        }
        if row.transaction_type == "differential:inline" {
            return Err(Error::unsupported("inline comments"));
        }
        xactions.push(Xaction { row, comment });
    }
    Ok(Ok(xactions))
}

/// A one-column query's single text value.
async fn text_value(
    database: &Database,
    sql: &str,
    binds: &[&str],
) -> Result<Option<Option<String>>, Error> {
    let row = phab_db::bind_all(phab_db::sqlx::query(sql), binds.iter().copied())
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?;
    row.map(|row| rows::opt_text(&row, "value").map_err(Error::from))
        .transpose()
}

/// `willPublish()`: the revision reloaded, with its repository, the
/// branch of its active diff and its projects.
async fn reload_revision(
    database: &Database,
    row: storage::Revision,
) -> Result<mail::Revision, Error> {
    // A repository the omnipotent viewer doesn't find is not attached.
    let mut repository = None;
    if let Some(phid) = &row.repository_phid {
        let sql = format!(
            "SELECT id, name, callsign FROM {} WHERE phid = ?",
            database.table("repository", "repository")
        );
        let found = phab_db::sqlx::query(&sql)
            .bind(phid.as_str())
            .fetch_optional(database.pool())
            .await
            .map_err(DbError::from)?;
        if let Some(found) = found {
            let callsign =
                rows::opt_text(&found, "callsign")?.filter(|callsign| !callsign.is_empty());
            let monogram = match callsign {
                Some(callsign) => format!("r{callsign}"),
                None => format!("R{}", rows::int(&found, "id")?),
            };
            repository = Some((monogram, rows::text(&found, "name")?));
        }
    }

    let mut branch = None;
    if row.status == "accepted" {
        // `needActiveDiffs()`: the newest diff of the revision.
        let sql = format!(
            "SELECT id, branch, bookmark FROM {} WHERE revisionID = ? ORDER BY id DESC LIMIT 1",
            database.table("differential", "differential_diff")
        );
        let diff = phab_db::sqlx::query(&sql)
            .bind(row.id)
            .fetch_optional(database.pool())
            .await
            .map_err(DbError::from)?
            .ok_or_else(|| Error::unsupported("an accepted revision without a diff"))?;
        let name = |column: &str| -> Result<String, Error> {
            Ok(rows::opt_text(&diff, column)?.unwrap_or_default())
        };
        let (branch_name, bookmark) = (name("branch")?, name("bookmark")?);
        branch = match (branch_name.is_empty(), bookmark.is_empty()) {
            (false, false) => Some(format!("{bookmark} (bookmark) on {branch_name} (branch)")),
            (true, false) => Some(format!("{bookmark} (bookmark)")),
            (false, true) => {
                let id = rows::int(&diff, "id")?.to_string();
                let sql = format!(
                    "SELECT data AS value FROM {} WHERE diffID = ? AND name = 'arc:onto'",
                    database.table("differential", "differential_diffproperty")
                );
                if text_value(database, &sql, &[&id]).await?.is_some() {
                    return Err(Error::unsupported("the target branch of a diff"));
                }
                Some(branch_name)
            }
            (true, true) => None,
        };
    }

    let projects = edges::load_destination_phids(
        database,
        std::slice::from_ref(&row.phid),
        &[edges::OBJECT_HAS_PROJECT],
        None,
    )
    .await?
    .into_iter()
    .map(Phid::into_string)
    .collect();
    Ok(mail::Revision {
        row,
        repository,
        branch,
        projects,
    })
}

/// Whether firehose webhooks exist (`queueWebhooks()` would call them).
async fn has_firehose_webhooks(database: &Database) -> Result<bool, Error> {
    let sql = format!(
        "SELECT phid AS value FROM {} WHERE status = ? LIMIT 1",
        database.table("herald", "herald_webhook")
    );
    Ok(text_value(database, &sql, &[FIREHOSE]).await?.is_some())
}

/// Reads everything `doWork()` reads and builds what it writes.
pub async fn plan(
    database: &Database,
    config: &Config,
    data: &Value,
    now: i64,
) -> Result<Plan, Error> {
    // loadObject().
    let empty = Map::new();
    let data = match data {
        Value::Object(map) => map,
        // A list is an array without the keys.
        Value::Array(_) => &empty,
        _ => return Ok(Plan::Fail("Task has invalid task data.".to_owned())),
    };
    let object = data.get("objectPHID").unwrap_or(&Value::Null);
    if !php::is_truthy(object) {
        return Ok(Plan::Fail("Task has no object PHID!".to_owned()));
    }
    let object_phid = object
        .as_str()
        .ok_or_else(|| Error::unsupported("an object PHID which is not a string"))?;
    if Phid::new(object_phid).phid_type() != PhidType::REVISION {
        return Err(Error::unsupported(
            "publishing objects other than revisions",
        ));
    }
    let Some(revision) = storage::load_revisions(database, &[Phid::new(object_phid)])
        .await?
        .pop()
    else {
        return Ok(Plan::Fail(format!(
            "Unable to load object with PHID \"{object_phid}\"!"
        )));
    };

    // buildEditor(): the acting user, and loadWorkerState().
    let actor = data
        .get("actorPHID")
        .and_then(Value::as_str)
        .filter(|phid| !phid.is_empty())
        .ok_or_else(|| Error::unsupported("a task without an acting user"))?;
    let state = State::from_data(data)?;

    let xactions = match load_transactions(database, object_phid, &xaction_phids(data)?).await? {
        Ok(xactions) => xactions,
        Err(message) => return Ok(Plan::Fail(message)),
    };

    let revision = reload_revision(database, revision).await?;
    if has_firehose_webhooks(database).await? {
        return Err(Error::unsupported("firehose webhooks"));
    }

    let mails = if state.flag("mailShouldSend") {
        mail::build(&mail::Context {
            database,
            config,
            now,
            revision: &revision,
            xactions: &xactions,
            state: &state,
            actor,
        })
        .await?
    } else {
        Vec::new()
    };

    // PhabricatorSearchWorker::queueDocumentForIndexing().
    let transaction_phids: Map<String, Value> = xactions
        .iter()
        .map(|xaction| {
            let phid = xaction.row.phid.as_str();
            (phid.to_owned(), json!(phid))
        })
        .collect();
    let search_data = php::json_encode(&json!({
        "documentPHID": object_phid,
        "parameters": {
            "transactionPHIDs": php::round_trip(&Value::Object(transaction_phids)),
        },
        "strict": false,
    }));

    let story = if state.flag("feedShouldPublish") {
        let mut mailed: Vec<String> = Vec::new();
        for message in &mails {
            for phid in &message.mailed {
                if !mailed.contains(phid) {
                    mailed.push(phid.clone());
                }
            }
        }
        feed::build(
            &feed::Context {
                database,
                config,
                object_phid,
                xactions: &xactions,
                state: &state,
                actor,
            },
            &mailed,
        )
        .await?
    } else {
        None
    };

    Ok(Plan::Publish(Box::new(Publication {
        object_phid: object_phid.to_owned(),
        search_data,
        story,
        mails,
    })))
}

/// `generateChronologicalKey()`: the time in the upper 32 bits, random
/// ones below.
fn chronological_key(now: i64) -> u64 {
    let time = u64::try_from(now).unwrap_or_default();
    (time << 32) + u64::from(rand::random::<u32>())
}

/// `PhabricatorFeedStoryPublisher::publish()`: the story, its references
/// and notifications, the notification counts to recount, and the
/// `FeedPublisherWorker` task.
async fn write_story(
    connection: &mut MySqlConnection,
    database: &Database,
    story: &Story,
    now: i64,
) -> Result<(), DbError> {
    let key = chronological_key(now);
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, chronologicalKey, storyType, storyData, authorPHID, \
         dateCreated, dateModified) VALUES (?, ?, ?, ?, ?, ?, ?)",
        database.table("feed", "feed_storydata")
    ))
    .bind(Phid::generate(PhidType::STORY, None).as_str())
    .bind(key)
    .bind(feed::STORY_TYPE)
    .bind(php::json_encode(&story.data))
    .bind(&story.author)
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await?;
    if !story.related.is_empty() {
        let sql = format!(
            "INSERT INTO {} (objectPHID, chronologicalKey) VALUES {}",
            database.table("feed", "feed_storyreference"),
            vec!["(?, ?)"; story.related.len()].join(", ")
        );
        let mut query = phab_db::sqlx::query(&sql);
        for phid in &story.related {
            query = query.bind(phid).bind(key);
        }
        query.execute(&mut *connection).await?;
    }
    if !story.notifications.is_empty() {
        let sql = format!(
            "INSERT INTO {} (primaryObjectPHID, userPHID, chronologicalKey, hasViewed) \
             VALUES {}",
            database.table("feed", "feed_storynotification"),
            vec!["(?, ?, ?, ?)"; story.notifications.len()].join(", ")
        );
        let mut query = phab_db::sqlx::query(&sql);
        for (user, read) in &story.notifications {
            query = query
                .bind(&story.primary)
                .bind(user)
                .bind(key)
                .bind(i64::from(*read));
        }
        query.execute(&mut *connection).await?;
        // PhabricatorUserCache::clearCaches().
        let sql = format!(
            "DELETE FROM {} WHERE cacheIndex = ? AND userPHID IN ({})",
            database.table("user", "user_cache"),
            phab_db::placeholders(story.notifications.len())
        );
        let query = phab_db::sqlx::query(&sql).bind(phab_core::hash::digest_for_index(
            NOTIFICATION_COUNT_CACHE.as_bytes(),
        ));
        phab_db::bind_all(
            query,
            story.notifications.iter().map(|(user, _)| user.as_str()),
        )
        .execute(&mut *connection)
        .await?;
    }
    worker::schedule_task(
        connection,
        database,
        worker::Task {
            class: "FeedPublisherWorker",
            data: &php::json_encode(&json!({"key": key})),
            priority: worker::PRIORITY_DEFAULT,
            object_phid: None,
        },
        now,
    )
    .await?;
    Ok(())
}

/// `PhabricatorMetaMTAMail::save()` of a new mail: the row, the recipient
/// edges, and the `PhabricatorMetaMTAWorker` task.
async fn write_mail(
    connection: &mut MySqlConnection,
    database: &Database,
    (message, object_phid): (&Message, &str),
    now: i64,
) -> Result<(), DbError> {
    let phid = Phid::generate(PhidType::MAIL, None);
    let id = phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, actorPHID, parameters, status, message, relatedPHID, \
         dateCreated, dateModified) VALUES (?, ?, ?, ?, NULL, ?, ?, ?)",
        database.table("metamta", "metamta_mail")
    ))
    .bind(phid.as_str())
    .bind(&message.actor_phid)
    .bind(phab_app_metamta::mail::encode_parameters(
        &message.parameters,
    ))
    .bind(phab_app_metamta::mail::STATUS_QUEUE)
    .bind(object_phid)
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await?
    .last_insert_id();
    for (seq, recipient) in (0..).zip(&message.recipients) {
        phab_db::editor::insert_edge(
            connection,
            database,
            (phid.as_str(), MAIL_HAS_RECIPIENT, recipient),
            seq,
            now,
        )
        .await?;
    }
    worker::schedule_task(
        connection,
        database,
        worker::Task {
            class: "PhabricatorMetaMTAWorker",
            data: &id.to_string(),
            priority: worker::PRIORITY_ALERTS,
            object_phid: None,
        },
        now,
    )
    .await?;
    Ok(())
}

/// Writes what [`plan`] built, in PHP's order: the search task, the feed
/// story, then each mail. One transaction holds them all, so a failure
/// leaves nothing to publish twice.
pub async fn write(
    database: &Database,
    publication: &Publication,
    now: i64,
) -> Result<(), DbError> {
    let mut transaction = database.pool().begin().await?;
    let connection = &mut *transaction;
    worker::schedule_task(
        connection,
        database,
        worker::Task {
            class: "PhabricatorSearchWorker",
            data: &publication.search_data,
            priority: PRIORITY_INDEX,
            object_phid: Some(&publication.object_phid),
        },
        now,
    )
    .await?;

    if let Some(story) = &publication.story {
        write_story(connection, database, story, now).await?;
    }
    for message in &publication.mails {
        write_mail(
            connection,
            database,
            (message, &publication.object_phid),
            now,
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(())
}
