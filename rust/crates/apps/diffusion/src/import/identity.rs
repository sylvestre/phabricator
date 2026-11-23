//! `DiffusionRepositoryIdentityEngine::newResolvedIdentity()`: the
//! `repository_identity` row of an author or committer name, created or
//! updated with the user it resolves to (`DiffusionResolveUserQuery`).

use phab_core::hash::digest_for_index;
use phab_core::{Phid, PhidType};
use phab_db::{Database, DbError, rows};

use crate::import::{ImportError, now};

/// `PhabricatorRepositoryIdentityPHIDType`.
const IDENTITY: PhidType = PhidType::new(*b"RIDT");

/// `DiffusionIdentityUnassignedDatasource::FUNCTION_TOKEN`.
const UNASSIGNED: &str = "unassigned()";

/// The byte length of the `emailAddress` column (`sort255`).
const EMAIL_BYTES: usize = 255;

/// A row of `repository_identity`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `id`; `None` until saved.
    pub id: Option<i64>,
    /// `phid`.
    pub phid: String,
    /// `identityNameRaw`.
    pub name: String,
    /// `automaticGuessedUserPHID`.
    pub automatic_user_phid: Option<String>,
    /// `manuallySetUserPHID`.
    pub manual_user_phid: Option<String>,
    /// `currentEffectiveUserPHID`.
    pub effective_user_phid: Option<String>,
    /// `authorPHID`: the commit which first named it.
    pub author_phid: String,
    /// `dateCreated`.
    pub date_created: i64,
}

/// `PhutilEmailAddress`: the display name and the address
/// (`getAddress()`) of `raw`.
pub fn parse_address(raw: &str) -> (Option<String>, String) {
    // `getAddress()`: the local part alone when the domain is empty.
    let join = |local: &str, domain: &str| {
        if domain.is_empty() {
            local.to_owned()
        } else {
            format!("{local}@{domain}")
        }
    };
    let raw = crate::php::trim(raw);
    // Neither regular expression matches across a newline.
    if !raw.contains('\n') {
        // `/^(.*)<(.*?)>$/`: the greedy first group ends at the last `<`.
        if let Some(inner) = raw.strip_suffix('>')
            && let Some(open) = inner.rfind('<')
        {
            let display = inner[..open].trim_matches(['\'', '"', ' ']).to_owned();
            let inner = &inner[open + 1..];
            let address = match inner.split_once('@') {
                Some((local, domain)) => join(local, domain),
                None => inner.to_owned(),
            };
            return (Some(display), address);
        }
        // `/^(.*)@(.*)$/`: split at the last `@`.
        if let Some(at) = raw.rfind('@') {
            return (None, join(&raw[..at], &raw[at + 1..]));
        }
    }
    (None, raw.to_owned())
}

impl Identity {
    /// The identity of `name`, by `identityNameHash`.
    async fn load(database: &Database, name: &str) -> Result<Option<Self>, ImportError> {
        let row = sqlx::query(&format!(
            "SELECT id, phid, identityNameRaw, identityNameEncoding, automaticGuessedUserPHID, \
             manuallySetUserPHID, currentEffectiveUserPHID, authorPHID, dateCreated FROM {} \
             WHERE identityNameHash = ?",
            database.table("repository", "repository_identity")
        ))
        .bind(digest_for_index(name.as_bytes()))
        .fetch_optional(database.pool())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let raw: Vec<u8> = sqlx::Row::try_get(&row, "identityNameRaw").map_err(DbError::from)?;
        let encoding = rows::opt_text(&row, "identityNameEncoding")?;
        let (Ok(name), Some("utf8")) = (String::from_utf8(raw), encoding.as_deref()) else {
            return Err(ImportError::unsupported(
                "an identity whose name is not stored as UTF-8",
            ));
        };
        Ok(Some(Self {
            id: Some(rows::int(&row, "id")?),
            phid: rows::text(&row, "phid")?,
            name,
            automatic_user_phid: rows::opt_text(&row, "automaticGuessedUserPHID")?,
            manual_user_phid: rows::opt_text(&row, "manuallySetUserPHID")?,
            effective_user_phid: rows::opt_text(&row, "currentEffectiveUserPHID")?,
            author_phid: rows::text(&row, "authorPHID")?,
            date_created: rows::int(&row, "dateCreated")?,
        }))
    }

    /// Checks that the identity of `name`, if there is one, can be updated
    /// as PHP would (its name is stored as UTF-8).
    pub async fn check(database: &Database, name: &str) -> Result<(), ImportError> {
        Self::load(database, name).await.map(|_| ())
    }

    /// `newResolvedIdentity()`: loads or creates the identity of `name`
    /// (first named by `source_phid`), resolves its user and saves it.
    pub async fn resolve(
        database: &Database,
        name: &str,
        source_phid: &str,
    ) -> Result<Self, ImportError> {
        let mut identity = match Self::load(database, name).await? {
            Some(identity) => identity,
            None => Self {
                id: None,
                phid: Phid::generate(IDENTITY, None).into_string(),
                name: name.to_owned(),
                automatic_user_phid: None,
                manual_user_phid: None,
                effective_user_phid: None,
                author_phid: source_phid.to_owned(),
                date_created: now(),
            },
        };
        // A manual user which no longer exists is forgotten.
        if let Some(manual) = identity
            .manual_user_phid
            .clone()
            .filter(|phid| !phid.is_empty() && phid != UNASSIGNED)
            && user_phid(database, "phid", &manual).await?.is_none()
        {
            identity.manual_user_phid = None;
        }
        identity.automatic_user_phid = resolve_user(database, &identity.name).await?;
        identity.save(database).await?;
        Ok(identity)
    }

    /// `save()`: the effective user and address first.
    async fn save(&mut self, database: &Database) -> Result<(), DbError> {
        self.effective_user_phid = match self.manual_user_phid.as_deref() {
            Some(manual) if !manual.is_empty() => (manual != UNASSIGNED).then(|| manual.to_owned()),
            _ => self.automatic_user_phid.clone(),
        };
        let (_, address) = parse_address(&self.name);
        let email = (address.len() <= EMAIL_BYTES).then_some(address);
        let table = database.table("repository", "repository_identity");
        let now = now();
        if let Some(id) = self.id {
            sqlx::query(&format!(
                "UPDATE {table} SET phid = ?, dateCreated = ?, dateModified = ?, \
                 automaticGuessedUserPHID = ?, manuallySetUserPHID = ?, \
                 currentEffectiveUserPHID = ?, identityNameHash = ?, identityNameRaw = ?, \
                 identityNameEncoding = 'utf8', authorPHID = ?, emailAddress = ? WHERE id = ?"
            ))
            .bind(&self.phid)
            .bind(self.date_created)
            .bind(now)
            .bind(&self.automatic_user_phid)
            .bind(&self.manual_user_phid)
            .bind(&self.effective_user_phid)
            .bind(digest_for_index(self.name.as_bytes()))
            .bind(self.name.as_bytes())
            .bind(&self.author_phid)
            .bind(email)
            .bind(id)
            .execute(database.pool())
            .await?;
        } else {
            let id = sqlx::query(&format!(
                "INSERT INTO {table} (phid, dateCreated, dateModified, automaticGuessedUserPHID, \
                 manuallySetUserPHID, currentEffectiveUserPHID, identityNameHash, \
                 identityNameRaw, identityNameEncoding, authorPHID, emailAddress) VALUES \
                 (?, ?, ?, ?, ?, ?, ?, ?, 'utf8', ?, ?)"
            ))
            .bind(&self.phid)
            .bind(self.date_created)
            .bind(now)
            .bind(&self.automatic_user_phid)
            .bind(&self.manual_user_phid)
            .bind(&self.effective_user_phid)
            .bind(digest_for_index(self.name.as_bytes()))
            .bind(self.name.as_bytes())
            .bind(&self.author_phid)
            .bind(email)
            .execute(database.pool())
            .await?
            .last_insert_id();
            self.id = i64::try_from(id).ok();
        }
        Ok(())
    }
}

/// The PHID of the user whose `column` is `value` (`loadOneWhere()`).
async fn user_phid(
    database: &Database,
    column: &str,
    value: &str,
) -> Result<Option<String>, DbError> {
    let found = sqlx::query(&format!(
        "SELECT phid FROM {} WHERE {column} = ?",
        database.table("user", "user")
    ))
    .bind(value)
    .fetch_all(database.pool())
    .await?;
    found.first().map(|row| rows::text(row, "phid")).transpose()
}

/// `findUserByRealName()`: real names are not unique, so only one match
/// counts.
async fn user_by_real_name(database: &Database, name: &str) -> Result<Option<String>, DbError> {
    let found = sqlx::query(&format!(
        "SELECT phid FROM {} WHERE realName = ?",
        database.table("user", "user")
    ))
    .bind(name)
    .fetch_all(database.pool())
    .await?;
    match found.as_slice() {
        [row] => Ok(Some(rows::text(row, "phid")?)),
        _ => Ok(None),
    }
}

/// `PhabricatorUser::loadOneWithEmailAddress()`.
async fn user_by_email(database: &Database, address: &str) -> Result<Option<String>, DbError> {
    let found = sqlx::query(&format!(
        "SELECT userPHID FROM {} WHERE address = ?",
        database.table("user", "user_email")
    ))
    .bind(address)
    .fetch_all(database.pool())
    .await?;
    match found.first() {
        Some(row) => user_phid(database, "phid", &rows::text(row, "userPHID")?).await,
        None => Ok(None),
    }
}

/// `DiffusionResolveUserQuery`: the user a name like `Alice <a@b.c>`
/// names, by user name, address or real name, whole then in parts.
pub async fn resolve_user(database: &Database, name: &str) -> Result<Option<String>, DbError> {
    if name.is_empty() {
        return Ok(None);
    }
    if let Some(phid) = user_phid(database, "userName", name).await? {
        return Ok(Some(phid));
    }
    if let Some(phid) = user_by_email(database, name).await? {
        return Ok(Some(phid));
    }
    if let Some(phid) = user_by_real_name(database, name).await? {
        return Ok(Some(phid));
    }
    let (display, address) = parse_address(name);
    if let Some(phid) = user_by_email(database, &address).await? {
        return Ok(Some(phid));
    }
    if let Some(display) = display.filter(|display| phab_core::php::is_truthy_str(display)) {
        if let Some(phid) = user_phid(database, "userName", &display).await? {
            return Ok(Some(phid));
        }
        if let Some(phid) = user_by_real_name(database, &display).await? {
            return Ok(Some(phid));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_addresses_like_php() {
        assert_eq!(
            parse_address("alice <alice@example.com>"),
            (Some("alice".to_owned()), "alice@example.com".to_owned())
        );
        assert_eq!(
            parse_address(" \"Bob B\" <b@x> "),
            (Some("Bob B".to_owned()), "b@x".to_owned())
        );
        assert_eq!(parse_address("plain@x"), (None, "plain@x".to_owned()));
        assert_eq!(parse_address(""), (None, String::new()));
    }
}
