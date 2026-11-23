//! Who a mail goes to: `PhabricatorMetaMTAMemberQuery` (recipient
//! expansion), `PhabricatorMetaMTAActorQuery` and the deliverability rules
//! of `PhabricatorMetaMTAMail::loadActors()`, plus the preferences they and
//! the message engine read.

use std::collections::HashMap;

use indexmap::{IndexMap, IndexSet};
use phab_core::{Config, Phid, hash};
use phab_db::{Database, DbError, edges, rows};
use serde_json::{Map, Value};

use crate::mail::Mail;
use crate::{Error, php};

/// `PhabricatorMetaMTAActor::REASON_*`.
pub mod reason {
    /// `REASON_UNLOADABLE`.
    pub const UNLOADABLE: &str = "unloadable";
    /// `REASON_UNMAILABLE`.
    pub const UNMAILABLE: &str = "unmailable";
    /// `REASON_NO_ADDRESS`.
    pub const NO_ADDRESS: &str = "noaddress";
    /// `REASON_DISABLED`.
    pub const DISABLED: &str = "disabled";
    /// `REASON_MAIL_DISABLED`.
    pub const MAIL_DISABLED: &str = "maildisabled";
    /// `REASON_RESPONSE`.
    pub const RESPONSE: &str = "response";
    /// `REASON_SELF`.
    pub const SELF: &str = "self";
    /// `REASON_MAILTAGS`.
    pub const MAILTAGS: &str = "mailtags";
    /// `REASON_BOT`.
    pub const BOT: &str = "bot";
    /// `REASON_FORCE`.
    pub const FORCE: &str = "force";
    /// `REASON_FORCE_HERALD`.
    pub const FORCE_HERALD: &str = "force-herald";
    /// `REASON_ROUTE_AS_NOTIFICATION`.
    pub const ROUTE_AS_NOTIFICATION: &str = "route-as-notification";
    /// `REASON_ROUTE_AS_MAIL`.
    pub const ROUTE_AS_MAIL: &str = "route-as-mail";
    /// `REASON_UNVERIFIED`.
    pub const UNVERIFIED: &str = "unverified";
    /// `REASON_MUTED`.
    pub const MUTED: &str = "muted";
    /// `REASON_MOZILLA_EMAILS`: the recipient gets Mozilla's own mail
    /// (`feed.for_email.*`) instead.
    pub const MOZILLA_EMAILS: &str = "mozilla-emails";
}

/// `PhabricatorUserPreferencesCacheType::KEY_PREFERENCES`.
const PREFERENCES_CACHE_KEY: &str = "user.preferences.v2";
/// `PhabricatorUserPreferencesCacheType::CACHETYPE`.
const PREFERENCES_CACHE_TYPE: &str = "preferences";

/// `PhabricatorEmailNotificationsSetting::SETTINGKEY` and its values.
const NO_MAIL: &str = "no-mail";
const VALUE_NO_MAIL: &str = "1";
const VALUE_MOZILLA_MAIL: &str = "2";

/// `PhabricatorMailRoutingRule::ROUTE_AS_NOTIFICATION`.
const ROUTE_AS_NOTIFICATION: &str = "route.notification";
/// `PhabricatorMailRoutingRule::ROUTE_AS_MAIL`.
const ROUTE_AS_MAIL: &str = "route.mail";

/// The delivery status of an actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// `STATUS_DELIVERABLE`.
    Deliverable,
    /// `STATUS_UNDELIVERABLE`.
    Undeliverable,
    /// `STATUS_EXTERNALLY_DELIVERABLE`: Mozilla's mail, not this one.
    ExternallyDeliverable,
}

/// `PhabricatorMetaMTAActor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// `getPHID()`.
    pub phid: String,
    /// `getName()`.
    pub name: Option<String>,
    /// `getEmailAddress()`.
    pub email: Option<String>,
    /// `getIsVerified()`.
    pub is_verified: bool,
    /// The status.
    pub status: Status,
    /// `getDeliverabilityReasons()`.
    pub reasons: Vec<&'static str>,
}

impl Actor {
    fn new(phid: &str) -> Self {
        Self {
            phid: phid.to_owned(),
            name: None,
            email: None,
            is_verified: false,
            status: Status::Deliverable,
            reasons: Vec::new(),
        }
    }

    fn set(&mut self, status: Status, reason: &'static str) {
        self.reasons.push(reason);
        self.status = status;
    }

    /// `isDeliverable()`.
    pub fn is_deliverable(&self) -> bool {
        self.status == Status::Deliverable
    }

    /// `isDeliverableByAnyMail()`: delivered by this mail or by Mozilla's.
    pub fn is_deliverable_by_any_mail(&self) -> bool {
        self.status != Status::Undeliverable
    }
}

/// A preferences object as `PhabricatorUserPreferences::getSettingValue()`
/// reads it: its own values, then those of the defaults attached to it (the
/// global preferences), then the setting's default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Preferences {
    values: Map<String, Value>,
    defaults: Option<Map<String, Value>>,
}

impl Preferences {
    /// `getSettingValue()`.
    pub fn value(&self, key: &str, config: &Config) -> Value {
        if let Some(value) = self.values.get(key) {
            return value.clone();
        }
        if let Some(value) = self
            .defaults
            .as_ref()
            .and_then(|defaults| defaults.get(key))
        {
            return value.clone();
        }
        setting_default(key, config)
    }
}

/// `getSettingDefaultValue()` of a setting, `null` for an unknown one.
fn setting_default(key: &str, config: &Config) -> Value {
    let context = phab_app_settings::setting::Context {
        config,
        viewer_name: "",
        now: 0,
    };
    phab_app_settings::setting::find(key)
        .map_or(Value::Null, |setting| setting.default_value(&context))
}

/// A user, as the actor and people queries read it.
#[derive(Debug, Clone)]
struct User {
    username: String,
    real_name: String,
    is_disabled: bool,
    is_system_agent: bool,
}

/// The recipients of a mail: how each PHID expands and every actor.
#[derive(Debug, Clone, Default)]
pub struct Recipients {
    expansion: HashMap<String, Vec<String>>,
    /// The actors by PHID, in the order PHP builds them.
    pub actors: IndexMap<String, Actor>,
    /// The preferences of the users among the actors
    /// (`needSyntheticPreferences()`).
    preferences: HashMap<String, Preferences>,
    /// The global preferences.
    global: Preferences,
}

impl Recipients {
    /// `expandRecipients()`.
    pub fn expand(&self, phids: &[String]) -> Vec<String> {
        let mut results = IndexSet::new();
        for phid in phids {
            for recipient in self.expansion.get(phid).into_iter().flatten() {
                results.insert(recipient.clone());
            }
        }
        results.into_iter().collect()
    }

    /// `getActor()`.
    pub fn actor(&self, phid: &str) -> Option<&Actor> {
        self.actors.get(phid)
    }

    /// `loadPreferences($target_phid)` of the mail: the target's
    /// preferences with `metamta.one-mail-per-recipient`, the global ones
    /// otherwise or when the target is not a user. An empty target (no
    /// "To" and no "Cc") gets a preferences object with no defaults
    /// attached.
    pub fn preferences_for(&self, target: Option<&str>, config: &Config) -> Preferences {
        if config.truthy("metamta.one-mail-per-recipient") {
            match target {
                // A synthetic object without a user, so without defaults.
                None | Some("" | "0") => return Preferences::default(),
                Some(target) => {
                    if let Some(preferences) = self.preferences.get(target) {
                        return preferences.clone();
                    }
                }
            }
        }
        self.global.clone()
    }
}

/// The PHIDs of `from`, "To" and "Cc" (`getAllActorPHIDs()`); an empty
/// sender is `""`.
fn all_actor_phids(from: &str, to: &[String], cc: &[String]) -> Vec<String> {
    std::iter::once(from.to_owned())
        .chain(to.iter().cloned())
        .chain(cc.iter().cloned())
        .collect()
}

/// `PhabricatorMetaMTAMemberQuery`: projects expand to their members who
/// haven't silenced them, plus their watchers; everything else to itself.
/// Every PHID gets an entry.
pub async fn expansion_map(
    database: &Database,
    phids: &[String],
) -> Result<HashMap<String, Vec<String>>, Error> {
    let mut map = HashMap::new();
    let mut projects = Vec::new();
    for phid in phids {
        if php::phid_type(phid) == "PROJ" {
            projects.push(Phid::new(phid.clone()));
        } else {
            map.insert(phid.clone(), vec![phid.clone()]);
        }
    }
    if projects.is_empty() {
        return Ok(map);
    }
    let loaded = phab_app_project::storage::load_by_phid(database, &projects).await?;
    for phid in &projects {
        let Some(project) = loaded.iter().find(|project| project.phid == *phid) else {
            map.insert(phid.as_str().to_owned(), Vec::new());
            continue;
        };
        // Subprojects and milestones depend on how the project query links
        // the graph and borrows the parent's members; not ported.
        if project.parent_phid.is_some() {
            return Err(Error::unsupported("mail to a subproject or milestone"));
        }
        let source = std::slice::from_ref(phid);
        let members = edges::load_destination_phids(
            database,
            source,
            &[edges::PROJECT_MATERIALIZED_MEMBER],
            None,
        )
        .await?;
        let watchers = edges::load_destination_phids(
            database,
            source,
            &[phab_app_project::storage::OBJECT_HAS_WATCHER],
            None,
        )
        .await?;
        let silenced =
            edges::load_destination_phids(database, source, &[edges::PROJECT_SILENCED], None)
                .await?;
        let mut result: IndexSet<String> = members
            .iter()
            .filter(|member| !silenced.contains(member))
            .map(|member| member.as_str().to_owned())
            .collect();
        result.extend(watchers.iter().map(|watcher| watcher.as_str().to_owned()));
        map.insert(phid.as_str().to_owned(), result.into_iter().collect());
    }
    Ok(map)
}

async fn load_users(database: &Database, phids: &[&str]) -> Result<HashMap<String, User>, Error> {
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT phid, userName, realName, isDisabled, isSystemAgent FROM {} WHERE phid IN ({})",
        database.table("user", "user"),
        phab_db::placeholders(phids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().copied())
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?;
    let mut users = HashMap::new();
    for row in found {
        users.insert(
            rows::text(&row, "phid")?,
            User {
                username: rows::text(&row, "userName")?,
                real_name: rows::text(&row, "realName")?,
                is_disabled: rows::flag(&row, "isDisabled")?,
                is_system_agent: rows::flag(&row, "isSystemAgent")?,
            },
        );
    }
    Ok(users)
}

/// The primary addresses, the last row of a user winning (`mpull()`).
async fn load_primary_emails(
    database: &Database,
    phids: &[&str],
) -> Result<HashMap<String, (String, bool)>, Error> {
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT userPHID, address, isVerified FROM {} WHERE userPHID IN ({}) AND isPrimary = 1 \
         ORDER BY id",
        database.table("user", "user_email"),
        phab_db::placeholders(phids.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), phids.iter().copied())
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?;
    let mut emails = HashMap::new();
    for row in found {
        emails.insert(
            rows::text(&row, "userPHID")?,
            (
                rows::text(&row, "address")?,
                rows::flag(&row, "isVerified")?,
            ),
        );
    }
    Ok(emails)
}

/// The cached preferences of each user (`fillUserCaches()` of a people
/// query with `needUserSettings()`). PHP builds and writes the cache of a
/// user who has none, which Rust doesn't: such mail is declined.
async fn load_settings_caches(
    database: &Database,
    phids: &[&str],
) -> Result<HashMap<String, Value>, Error> {
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT userPHID, cacheKey, cacheData, cacheType FROM {} WHERE cacheIndex = ? AND \
         userPHID IN ({})",
        database.table("user", "user_cache"),
        phab_db::placeholders(phids.len())
    );
    let query =
        phab_db::sqlx::query(&sql).bind(hash::digest_for_index(PREFERENCES_CACHE_KEY.as_bytes()));
    let found = phab_db::bind_all(query, phids.iter().copied())
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?;
    let mut caches = HashMap::new();
    for row in found {
        if rows::text(&row, "cacheKey")? != PREFERENCES_CACHE_KEY
            || rows::text(&row, "cacheType")? != PREFERENCES_CACHE_TYPE
        {
            return Err(Error::unsupported("a user cache row PHP would discard"));
        }
        let Some(data) = rows::opt_text(&row, "cacheData")? else {
            return Err(Error::unsupported("an empty user preferences cache"));
        };
        let value: Value = serde_json::from_str(&data)
            .map_err(|_| Error::unsupported("a user preferences cache which is not JSON"))?;
        caches.insert(rows::text(&row, "userPHID")?, value);
    }
    for phid in phids {
        if !caches.contains_key(*phid) {
            return Err(Error::unsupported(
                "a recipient without a cached preferences (PHP would build and write it)",
            ));
        }
    }
    Ok(caches)
}

/// `getUserName()` of the actor query, by `metamta.user-address-format`.
fn user_name(user: &User, config: &Config) -> String {
    let real_name = Some(&user.real_name).filter(|name| !name.is_empty());
    match (config.string("metamta.user-address-format"), real_name) {
        (Some("short"), _) | (_, None) => user.username.clone(),
        (Some("real"), Some(real_name)) => real_name.clone(),
        (_, Some(real_name)) => format!("{} ({real_name})", user.username),
    }
}

/// The routing rule of each PHID (`getRoutingRuleMap()`), as PHP builds it.
pub fn routing_rule_map(mail: &Mail) -> Result<IndexMap<String, (Value, Value)>, Error> {
    let mut map: IndexMap<String, (Value, Value)> = IndexMap::new();
    let Some(routing) = mail.param("routing") else {
        return Ok(map);
    };
    let bad = || Error::unsupported("mail routing rules PHP would fail on");
    for route in php::values(routing).ok_or_else(bad)? {
        let Value::Object(route) = route else {
            return Err(bad());
        };
        let rule = route.get("routingRule").ok_or_else(bad)?;
        let reason = route.get("reasonPHID").ok_or_else(bad)?;
        let phids = match route.get("phids").ok_or_else(bad)? {
            Value::Null => vec!["default".to_owned()],
            phids => php::strings(phids).ok_or_else(bad)?,
        };
        let Some(rule_name) = rule.as_str() else {
            return Err(bad());
        };
        for phid in phids {
            let is_stronger = match map.get(&phid) {
                None => true,
                Some((current, _)) => {
                    strength(rule_name) > strength(current.as_str().unwrap_or(""))
                }
            };
            if is_stronger {
                map.insert(phid, (rule.clone(), reason.clone()));
            }
        }
    }
    Ok(map)
}

/// `PhabricatorMailRoutingRule::getRuleStrength()`.
fn strength(rule: &str) -> i64 {
    match rule {
        ROUTE_AS_NOTIFICATION => 1,
        ROUTE_AS_MAIL => 2,
        _ => 0,
    }
}

/// The parts of a mail the recipients depend on.
#[derive(Debug, Clone)]
pub struct Parties {
    /// `from`, `""` when missing.
    pub from: String,
    /// `getToPHIDs()`.
    pub to: Vec<String>,
    /// `getCcPHIDs()`.
    pub cc: Vec<String>,
}

impl Parties {
    /// Reads them from the mail parameters.
    pub fn of(mail: &Mail) -> Result<Self, Error> {
        let from = mail.string_param("from")?.unwrap_or_default().to_owned();
        let parties = Self {
            from,
            to: mail.strings_param("to")?,
            cc: mail.strings_param("cc")?,
        };
        // PHP keys arrays by these PHIDs; a decimal one would turn into an
        // integer key.
        for phid in parties.to.iter().chain(&parties.cc).chain([&parties.from]) {
            if !phid.is_empty() && phid.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(Error::unsupported("a numeric recipient"));
            }
        }
        Ok(parties)
    }
}

/// `loadAllActors()` with the rules of `loadActors()` applied.
pub async fn load(
    database: &Database,
    config: &Config,
    mail: &Mail,
    parties: &Parties,
) -> Result<Recipients, Error> {
    let all = all_actor_phids(&parties.from, &parties.to, &parties.cc);
    let expansion = expansion_map(database, &all).await?;
    let mut recipients = Recipients {
        expansion,
        ..Recipients::default()
    };
    let actor_phids: Vec<String> = recipients
        .expand(&all)
        .into_iter()
        .filter(|phid| !phid.is_empty() && phid != "0")
        .collect();

    // `PhabricatorMetaMTAActorQuery`.
    let user_phids: Vec<&str> = actor_phids
        .iter()
        .map(String::as_str)
        .filter(|phid| php::phid_type(phid) == "USER")
        .collect();
    let users = load_users(database, &user_phids).await?;
    let emails = load_primary_emails(database, &user_phids).await?;
    let loaded: Vec<&str> = user_phids
        .iter()
        .copied()
        .filter(|phid| users.contains_key(*phid))
        .collect();
    let caches = load_settings_caches(database, &loaded).await?;
    let mut actors: IndexMap<String, Actor> = IndexMap::new();
    for phid in &actor_phids {
        let mut actor = Actor::new(phid);
        if php::phid_type(phid) == "USER" {
            match users.get(phid) {
                None => actor.set(Status::Undeliverable, reason::UNLOADABLE),
                Some(user) => {
                    actor.name = Some(user_name(user, config));
                    if user.is_disabled {
                        actor.set(Status::Undeliverable, reason::DISABLED);
                    }
                    if user.is_system_agent {
                        actor.set(Status::Undeliverable, reason::BOT);
                    }
                }
            }
            match emails.get(phid) {
                None => actor.set(Status::Undeliverable, reason::NO_ADDRESS),
                Some((address, is_verified)) => {
                    actor.email = Some(address.clone());
                    actor.is_verified = *is_verified;
                }
            }
        } else {
            actor.set(Status::Undeliverable, reason::UNMAILABLE);
        }
        actors.insert(phid.clone(), actor);
    }

    let global_row = phab_app_settings::preferences::load_global(database).await?;
    recipients.preferences = user_preferences(database, &loaded, global_row.as_ref()).await?;
    recipients.global = Preferences {
        values: global_row.unwrap_or_default(),
        defaults: None,
    };

    if !actors.is_empty() {
        apply_rules(config, mail, &mut actors, &recipients, &caches)?;
    }
    recipients.actors = actors;
    Ok(recipients)
}

/// The preferences of these users (`PhabricatorUserPreferencesQuery` with
/// synthetic preferences: users without a row get an empty one), the
/// global preferences (`global`, from
/// [`phab_app_settings::preferences::load_global`]) attached as their
/// defaults. The PHIDs must be those of existing users.
pub async fn user_preferences(
    database: &Database,
    phids: &[&str],
    global: Option<&Map<String, Value>>,
) -> Result<HashMap<String, Preferences>, Error> {
    let mut preferences = HashMap::new();
    for phid in phids {
        let stored = phab_app_settings::preferences::load(database, &Phid::new(*phid)).await?;
        preferences.insert(
            (*phid).to_owned(),
            Preferences {
                values: stored.values,
                defaults: global.cloned(),
            },
        );
    }
    Ok(preferences)
}

/// Fails as [`Error::Unsupported`] unless every user of `phids` has the
/// settings cache a people query with `needUserSettings()` reads: PHP
/// builds and writes a missing one, which Rust doesn't.
pub async fn check_settings_caches(database: &Database, phids: &[&str]) -> Result<(), Error> {
    load_settings_caches(database, phids).await.map(|_| ())
}

/// The rules of `loadActors()` after the actor query, weakest first.
fn apply_rules(
    config: &Config,
    mail: &Mail,
    actors: &mut IndexMap<String, Actor>,
    recipients: &Recipients,
    caches: &HashMap<String, Value>,
) -> Result<(), Error> {
    if mail.flag("force") {
        for actor in actors.values_mut() {
            actor.set(Status::Deliverable, reason::FORCE);
        }
        return Ok(());
    }

    for phid in mail.strings_param("exclude")? {
        if let Some(actor) = actors.get_mut(&phid) {
            actor.set(Status::Undeliverable, reason::RESPONSE);
        }
    }

    let deliverable: Vec<String> = actors
        .values()
        .filter(|actor| actor.is_deliverable())
        .map(|actor| actor.phid.clone())
        .collect();

    for phid in mail.strings_param("muted")? {
        if let Some(actor) = actors.get_mut(&phid) {
            actor.set(Status::Undeliverable, reason::MUTED);
        }
    }

    // The sender's "Self Actions" setting, read from their settings cache
    // (`getUserSetting()`).
    let from = mail.string_param("from")?.unwrap_or_default();
    if let Some(from_actor) = actors.get_mut(from)
        && let Some(cache) = caches.get(from)
    {
        let exclude_self = match cache {
            Value::Object(settings) => settings.get("self-mail").cloned(),
            _ => None,
        }
        .unwrap_or_else(|| setting_default("self-mail", config));
        if phab_core::php::is_truthy(&exclude_self) {
            from_actor.set(Status::Undeliverable, reason::SELF);
        }
    }

    apply_mail_tags(config, mail, actors, recipients)?;

    let routing = routing_rule_map(mail)?;
    for phid in &deliverable {
        let rule = routing
            .get(phid)
            .or_else(|| routing.get("default"))
            .and_then(|(rule, _)| rule.as_str());
        let Some(actor) = actors.get_mut(phid) else {
            continue;
        };
        match rule {
            Some(ROUTE_AS_NOTIFICATION) => {
                actor.set(Status::Undeliverable, reason::ROUTE_AS_NOTIFICATION);
            }
            Some(ROUTE_AS_MAIL) => actor.set(Status::Deliverable, reason::ROUTE_AS_MAIL),
            _ => {}
        }
    }

    let force = mail.strings_param("herald-force-recipients")?;
    for phid in &deliverable {
        if force.contains(phid)
            && let Some(actor) = actors.get_mut(phid)
        {
            actor.set(Status::Deliverable, reason::FORCE_HERALD);
        }
    }

    // The "Email Notifications" setting runs last: Mozilla's mail, or none.
    for (phid, preferences) in &recipients.preferences {
        let setting = preferences.value(NO_MAIL, config);
        let Some(actor) = actors.get_mut(phid) else {
            continue;
        };
        if php::loose_eq_str(&setting, VALUE_MOZILLA_MAIL) {
            actor.set(Status::ExternallyDeliverable, reason::MOZILLA_EMAILS);
        } else if php::loose_eq_str(&setting, VALUE_NO_MAIL) {
            actor.set(Status::Undeliverable, reason::MAIL_DISABLED);
        }
    }

    for actor in actors.values_mut() {
        if !actor.is_verified {
            actor.set(Status::Undeliverable, reason::UNVERIFIED);
        }
    }
    Ok(())
}

/// The mail tags rule: recipients who elected to receive none of the
/// mail's tags.
fn apply_mail_tags(
    config: &Config,
    mail: &Mail,
    actors: &mut IndexMap<String, Actor>,
    recipients: &Recipients,
) -> Result<(), Error> {
    let tags = match mail.param("mailtags") {
        Some(tags) if phab_core::php::is_truthy(tags) => php::strings(tags)
            .ok_or_else(|| Error::unsupported("mail tags which are not strings"))?,
        _ => return Ok(()),
    };
    for (phid, preferences) in &recipients.preferences {
        let user_tags = preferences.value(phab_app_settings::setting::MAIL_TAGS, config);
        // `idx($user_mailtags, $tag, VALUE_EMAIL)`, cast to an integer.
        let lookup = |tag: &str| -> Result<i64, Error> {
            let value = match &user_tags {
                Value::Object(map) => map.get(tag),
                Value::Array(list) => tag.parse::<usize>().ok().and_then(|index| list.get(index)),
                _ => {
                    return Err(Error::unsupported(
                        "mail tag settings which are not an array",
                    ));
                }
            };
            Ok(value.map_or(
                phab_app_settings::mail_tags::VALUE_EMAIL,
                phab_app_settings::preferences::int_cast,
            ))
        };
        let mut send = false;
        for tag in &tags {
            if lookup(tag)? == phab_app_settings::mail_tags::VALUE_EMAIL {
                send = true;
                break;
            }
        }
        if !send && let Some(actor) = actors.get_mut(phid) {
            actor.set(Status::Undeliverable, reason::MAILTAGS);
        }
    }
    Ok(())
}

/// `actors.sent`: each actor's deliverability, as stored once the mail is
/// sent.
pub fn delivered_actors(recipients: &Recipients) -> Value {
    let mut map = Map::new();
    for actor in recipients.actors.values() {
        let mut entry = Map::new();
        entry.insert(
            "deliverable".to_owned(),
            Value::Bool(actor.is_deliverable()),
        );
        entry.insert(
            "reasons".to_owned(),
            Value::Array(
                actor
                    .reasons
                    .iter()
                    .map(|reason| Value::String((*reason).to_owned()))
                    .collect(),
            ),
        );
        map.insert(actor.phid.clone(), Value::Object(entry));
    }
    Value::Object(map)
}

/// `routingmap.sent`.
pub fn delivered_routing_map(mail: &Mail) -> Result<Value, Error> {
    let mut map = Map::new();
    for (phid, (rule, reason)) in routing_rule_map(mail)? {
        let mut entry = Map::new();
        entry.insert("rule".to_owned(), rule);
        entry.insert("reason".to_owned(), reason);
        map.insert(phid, Value::Object(entry));
    }
    Ok(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn mail(parameters: &Value) -> Mail {
        Mail {
            id: 1,
            phid: "PHID-MTAM-x".to_owned(),
            actor_phid: None,
            parameters: parameters.as_object().cloned().unwrap_or_default(),
            status: "queued".to_owned(),
            message: None,
            related_phid: None,
        }
    }

    #[test]
    fn keeps_the_strongest_routing_rule() {
        let mail = mail(&json!({"routing": [
            {"routingRule": "route.mail", "phids": ["PHID-USER-a"], "reasonPHID": "H1"},
            {"routingRule": "route.notification", "phids": ["PHID-USER-a"], "reasonPHID": "H2"},
            {"routingRule": "route.notification", "phids": null, "reasonPHID": "H3"},
        ]}));
        assert_eq!(
            delivered_routing_map(&mail).unwrap(),
            json!({
                "PHID-USER-a": {"rule": "route.mail", "reason": "H1"},
                "default": {"rule": "route.notification", "reason": "H3"},
            })
        );
    }

    #[test]
    fn formats_user_names() {
        let user = User {
            username: "alice".to_owned(),
            real_name: "Alice Liddell".to_owned(),
            is_disabled: false,
            is_system_agent: false,
        };
        let mut config = Config::default();
        assert_eq!(user_name(&user, &config), "alice (Alice Liddell)");
        let mut layer = Map::new();
        layer.insert("metamta.user-address-format".to_owned(), json!("real"));
        config.push_layer("test", layer);
        assert_eq!(user_name(&user, &config), "Alice Liddell");
    }
}
