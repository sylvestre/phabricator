//! `PhabricatorFerretFulltextEngineExtension::indexFulltextObject()`: the
//! rows of an object's `*_fdocument`, `*_ffield` and `*_fngrams` tables.
//!
//! [`build`] makes the corpora and ngrams from the abstract document's
//! fields, without the database; [`write`] reads the common ngrams and
//! updates the stored rows as PHP does, in one transaction. PHP inlines the
//! values of its longer statements in chunks of a megabyte; Rust binds
//! them in chunks of rows, which writes the same rows in the same order.

use std::collections::{HashMap, HashSet};

use phab_db::sqlx::{self, MySqlConnection, Row};
use phab_db::{Database, DbError};

use crate::text::{self, Unsupported};

/// `PhabricatorSearchDocumentFieldType`.
pub mod field {
    /// `FIELD_TITLE`.
    pub const TITLE: &str = "titl";
    /// `FIELD_BODY`.
    pub const BODY: &str = "body";
    /// `FIELD_COMMENT`.
    pub const COMMENT: &str = "cmnt";
    /// `FIELD_ALL`.
    pub const ALL: &str = "full";
    /// `FIELD_CORE`.
    pub const CORE: &str = "core";
}

/// Rows per statement when Rust splits a long list.
const CHUNK: usize = 4096;

/// A `PhabricatorFerretEngine`: where an object type's index lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Engine {
    /// The database (`establishConnection('w')` of the object).
    pub database: &'static str,
    /// `getApplicationName()`.
    pub application: &'static str,
    /// `getScopeName()`.
    pub scope: &'static str,
}

impl Engine {
    fn table(self, database: &Database, suffix: &str) -> String {
        database.table(
            self.database,
            &format!("{}_{}_{suffix}", self.application, self.scope),
        )
    }
}

/// One `*_ffield` row's corpora.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// `fieldKey`.
    pub key: String,
    /// `rawCorpus`.
    pub raw: String,
    /// `termCorpus`.
    pub term: String,
    /// `normalCorpus`.
    pub normal: String,
}

/// What the extension stores for a document, before the common ngrams are
/// removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Corpus {
    /// The fields, in the order their keys first appear.
    pub fields: Vec<Field>,
    /// The ngrams, sorted as PHP sorts them.
    pub ngrams: Vec<String>,
}

/// The `*_fdocument` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// `objectPHID`.
    pub object_phid: String,
    /// `isClosed`.
    pub is_closed: bool,
    /// `epochCreated`.
    pub epoch_created: i64,
    /// `epochModified`.
    pub epoch_modified: i64,
    /// `authorPHID`.
    pub author_phid: Option<String>,
    /// `ownerPHID`.
    pub owner_phid: Option<String>,
}

/// The corpora of a key, as `$ferret_corpus_map` collects them.
#[derive(Default)]
struct Collected {
    raw: Vec<String>,
    term: Vec<String>,
    normal: Vec<String>,
}

/// The fields and ngrams of a document whose fields are `fields`
/// (`getFieldData()`: key and corpus, in the order they were added).
pub fn build(fields: &[(String, String)]) -> Result<Corpus, Unsupported> {
    // Every field also counts for "all", and titles and bodies for "core".
    let mut virtual_fields: Vec<(&str, &str)> = Vec::new();
    for (key, raw) in fields {
        virtual_fields.push((key, raw));
        if key == field::TITLE || key == field::BODY {
            virtual_fields.push((field::CORE, raw));
        }
        virtual_fields.push((field::ALL, raw));
    }

    let mut keys: Vec<&str> = Vec::new();
    let mut collected: HashMap<&str, Collected> = HashMap::new();
    for (key, raw) in virtual_fields {
        if raw.is_empty() {
            continue;
        }
        let term = text::terms_corpus(raw)?;
        let normal = text::terms_corpus(&text::stem_corpus(raw)?)?;
        let entry = collected.entry(key).or_insert_with(|| {
            keys.push(key);
            Collected::default()
        });
        entry.raw.push(raw.to_owned());
        entry.term.push(term);
        entry.normal.push(normal);
    }

    let mut ferret_fields = Vec::with_capacity(keys.len());
    let mut sources: Vec<String> = Vec::new();
    for key in keys {
        let entry = &collected[key];
        let raw = entry.raw.join("\n");
        let normal = entry.normal.join("\n");
        let term = entry.term.join("\n");
        for corpus in [&raw, &normal, &term] {
            if !corpus.is_empty() {
                sources.push(corpus.clone());
            }
        }
        ferret_fields.push(Field {
            key: key.to_owned(),
            raw,
            term,
            normal,
        });
    }
    let ngrams = text::term_ngrams(&sources.join("\n"))?;
    Ok(Corpus {
        fields: ferret_fields,
        ngrams,
    })
}

/// `SELECT ngram FROM <common> WHERE ngram IN (...)`, in chunks.
async fn load_common(
    database: &Database,
    engine: Engine,
    ngrams: &[String],
) -> Result<HashSet<Vec<u8>>, DbError> {
    let table = engine.table(database, "fngrams_common");
    let mut common = HashSet::new();
    for chunk in ngrams.chunks(CHUNK) {
        let sql = format!(
            "SELECT ngram FROM {table} WHERE ngram IN ({})",
            phab_db::placeholders(chunk.len())
        );
        let rows = phab_db::bind_all(sqlx::query(&sql), chunk.iter().map(String::as_str))
            .fetch_all(database.pool())
            .await?;
        for row in rows {
            common.insert(bytes(&row, "ngram")?);
        }
    }
    Ok(common)
}

/// A text column as PHP reads it: its bytes.
fn bytes(row: &sqlx::mysql::MySqlRow, column: &str) -> Result<Vec<u8>, DbError> {
    if let Ok(value) = row.try_get::<Vec<u8>, _>(column) {
        return Ok(value);
    }
    row.try_get::<String, _>(column)
        .map(String::into_bytes)
        .map_err(|_| DbError::ColumnType {
            column: column.to_owned(),
        })
}

/// `rtrim($ngram, ' ')`: what a `CHAR` column gives back.
fn trimmed(ngram: &[u8]) -> &[u8] {
    let end = ngram
        .iter()
        .rposition(|byte| *byte != b' ')
        .map_or(0, |end| end + 1);
    &ngram[..end]
}

/// Writes the index of `document`: the rows of a new document, or the
/// changes to an indexed one.
pub async fn write(
    database: &Database,
    engine: Engine,
    document: &Document,
    corpus: &Corpus,
) -> Result<(), DbError> {
    let mut ngrams: Vec<&String> = corpus.ngrams.iter().collect();
    if !ngrams.is_empty() {
        let common = load_common(database, engine, &corpus.ngrams).await?;
        // MySQL drops the trailing spaces of `CHAR` values.
        ngrams.retain(|ngram| {
            !common.contains(ngram.as_bytes()) && !common.contains(trimmed(ngram.as_bytes()))
        });
    }

    let mut transaction = database.pool().begin().await?;
    let connection = &mut *transaction;
    let documents = engine.table(database, "fdocument");
    let old = sqlx::query(&format!("SELECT id FROM {documents} WHERE objectPHID = ?"))
        .bind(&document.object_phid)
        .fetch_optional(&mut *connection)
        .await?;
    let (document_id, is_new) = if let Some(row) = old {
        let id = phab_db::rows::int(&row, "id")?;
        sqlx::query(&format!(
            "UPDATE {documents} SET isClosed = ?, epochCreated = ?, epochModified = ?, \
                 authorPHID = ?, ownerPHID = ? WHERE id = ?"
        ))
        .bind(i64::from(document.is_closed))
        .bind(document.epoch_created)
        .bind(document.epoch_modified)
        .bind(document.author_phid.as_deref())
        .bind(document.owner_phid.as_deref())
        .bind(id)
        .execute(&mut *connection)
        .await?;
        (id, false)
    } else {
        let id = sqlx::query(&format!(
            "INSERT INTO {documents} (objectPHID, isClosed, epochCreated, epochModified, \
                 authorPHID, ownerPHID) VALUES (?, ?, ?, ?, ?, ?)"
        ))
        .bind(&document.object_phid)
        .bind(i64::from(document.is_closed))
        .bind(document.epoch_created)
        .bind(document.epoch_modified)
        .bind(document.author_phid.as_deref())
        .bind(document.owner_phid.as_deref())
        .execute(&mut *connection)
        .await?
        .last_insert_id();
        (
            i64::try_from(id).map_err(|_| DbError::ColumnType {
                column: "id".to_owned(),
            })?,
            true,
        )
    };
    write_fields(
        connection,
        database,
        engine,
        (document_id, is_new),
        &corpus.fields,
    )
    .await?;
    write_ngrams(connection, database, engine, (document_id, is_new), &ngrams).await?;
    transaction.commit().await?;
    Ok(())
}

/// A stored `*_ffield` row.
struct StoredField {
    id: i64,
    key: Vec<u8>,
    raw: Vec<u8>,
    term: Vec<u8>,
    normal: Vec<u8>,
}

/// `updateStoredFields()`.
async fn write_fields(
    connection: &mut MySqlConnection,
    database: &Database,
    engine: Engine,
    (document_id, is_new): (i64, bool),
    fields: &[Field],
) -> Result<(), DbError> {
    let table = engine.table(database, "ffield");
    let mut old: Vec<StoredField> = Vec::new();
    if !is_new {
        let rows = sqlx::query(&format!(
            "SELECT id, fieldKey, rawCorpus, termCorpus, normalCorpus FROM {table} \
             WHERE documentID = ?"
        ))
        .bind(document_id)
        .fetch_all(&mut *connection)
        .await?;
        for row in rows {
            let field = StoredField {
                id: phab_db::rows::int(&row, "id")?,
                key: bytes(&row, "fieldKey")?,
                raw: bytes(&row, "rawCorpus")?,
                term: bytes(&row, "termCorpus")?,
                normal: bytes(&row, "normalCorpus")?,
            };
            // `ipull(..., null, 'fieldKey')`: the last row of a key wins.
            match old.iter_mut().find(|known| known.key == field.key) {
                Some(known) => *known = field,
                None => old.push(field),
            }
        }
    }

    let removed: Vec<i64> = old
        .iter()
        .filter(|stored| {
            !fields
                .iter()
                .any(|field| field.key.as_bytes() == stored.key)
        })
        .map(|stored| stored.id)
        .collect();
    let mut updated: Vec<(i64, &Field)> = Vec::new();
    let mut inserted: Vec<&Field> = Vec::new();
    for field in fields {
        match old.iter().find(|stored| stored.key == field.key.as_bytes()) {
            None => inserted.push(field),
            Some(stored)
                if stored.raw == field.raw.as_bytes()
                    && stored.term == field.term.as_bytes()
                    && stored.normal == field.normal.as_bytes() => {}
            Some(stored) => updated.push((stored.id, field)),
        }
    }

    if !removed.is_empty() {
        let sql = format!(
            "DELETE FROM {table} WHERE id IN ({})",
            phab_db::placeholders(removed.len())
        );
        phab_db::bind_all(sqlx::query(&sql), removed.iter().copied())
            .execute(&mut *connection)
            .await?;
    }
    for (id, field) in updated {
        sqlx::query(&format!(
            "UPDATE {table} SET rawCorpus = ?, termCorpus = ?, normalCorpus = ? WHERE id = ?"
        ))
        .bind(&field.raw)
        .bind(&field.term)
        .bind(&field.normal)
        .bind(id)
        .execute(&mut *connection)
        .await?;
    }
    for field in inserted {
        sqlx::query(&format!(
            "INSERT INTO {table} (documentID, fieldKey, rawCorpus, termCorpus, normalCorpus) \
             VALUES (?, ?, ?, ?, ?)"
        ))
        .bind(document_id)
        .bind(&field.key)
        .bind(&field.raw)
        .bind(&field.term)
        .bind(&field.normal)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

/// `updateStoredNgrams()`.
async fn write_ngrams(
    connection: &mut MySqlConnection,
    database: &Database,
    engine: Engine,
    (document_id, is_new): (i64, bool),
    ngrams: &[&String],
) -> Result<(), DbError> {
    let table = engine.table(database, "fngrams");
    // `ipull($old, 'id', 'ngram')`: by ngram, the last ID.
    let mut old: Vec<(Vec<u8>, i64)> = Vec::new();
    if !is_new {
        let rows = sqlx::query(&format!(
            "SELECT id, ngram FROM {table} WHERE documentID = ? ORDER BY id"
        ))
        .bind(document_id)
        .fetch_all(&mut *connection)
        .await?;
        let mut positions: HashMap<Vec<u8>, usize> = HashMap::new();
        for row in rows {
            let ngram = bytes(&row, "ngram")?;
            let id = phab_db::rows::int(&row, "id")?;
            if let Some(&position) = positions.get(&ngram) {
                old[position].1 = id;
            } else {
                positions.insert(ngram.clone(), old.len());
                old.push((ngram, id));
            }
        }
    }

    let new: HashSet<&[u8]> = ngrams.iter().map(|ngram| ngram.as_bytes()).collect();
    let old_set: HashSet<&[u8]> = old.iter().map(|(ngram, _)| ngram.as_slice()).collect();
    let removed: Vec<i64> = old
        .iter()
        .filter(|(ngram, _)| {
            let mut untrimmed = ngram.clone();
            untrimmed.push(b' ');
            !new.contains(ngram.as_slice()) && !new.contains(untrimmed.as_slice())
        })
        .map(|(_, id)| *id)
        .collect();
    let inserted: Vec<&str> = ngrams
        .iter()
        .filter(|ngram| {
            !old_set.contains(ngram.as_bytes()) && !old_set.contains(trimmed(ngram.as_bytes()))
        })
        .map(|ngram| ngram.as_str())
        .collect();

    for chunk in removed.chunks(CHUNK) {
        let sql = format!(
            "DELETE FROM {table} WHERE id IN ({})",
            phab_db::placeholders(chunk.len())
        );
        phab_db::bind_all(sqlx::query(&sql), chunk.iter().copied())
            .execute(&mut *connection)
            .await?;
    }
    for chunk in inserted.chunks(CHUNK) {
        let sql = format!(
            "INSERT INTO {table} (documentID, ngram) VALUES {}",
            vec!["(?, ?)"; chunk.len()].join(", ")
        );
        let mut query = sqlx::query(&sql);
        for ngram in chunk {
            query = query.bind(document_id).bind(*ngram);
        }
        query.execute(&mut *connection).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, corpus)| ((*key).to_owned(), (*corpus).to_owned()))
            .collect()
    }

    #[test]
    fn copies_titles_and_bodies_to_core_and_all() {
        let corpus = build(&fields(&[
            ("titl", "Fix crash"),
            ("body", ""),
            ("plan", "Ran tests"),
            ("cmnt", "LGTM"),
            ("cmnt", "Thanks"),
        ]))
        .unwrap();
        let keys: Vec<&str> = corpus
            .fields
            .iter()
            .map(|field| field.key.as_str())
            .collect();
        assert_eq!(keys, ["titl", "core", "full", "plan", "cmnt"]);
        let all = &corpus.fields[2];
        assert_eq!(all.raw, "Fix crash\nRan tests\nLGTM\nThanks");
        assert_eq!(all.term, " Fix crash \n Ran tests \n LGTM \n Thanks ");
        assert_eq!(all.normal, " fix crash \n ran test \n lgtm \n thank ");
        assert_eq!(corpus.fields[4].raw, "LGTM\nThanks");
    }
}
