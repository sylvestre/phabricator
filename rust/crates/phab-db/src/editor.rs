//! Rows `PhabricatorApplicationTransactionEditor` writes for any object:
//! transactions and edges (`PhabricatorEdgeEditor`).
//!
//! The methods which write go through these, so every editor stores its
//! rows the same way; what goes in them is the method's business.

use phab_core::Phid;
use sqlx::MySqlConnection;

use crate::{Database, DbError, edges};

/// The content source of a Conduit call (`PhabricatorConduitContentSource`).
pub const CONDUIT_SOURCE: &str = r#"{"source":"conduit","params":[]}"#;
/// The content source of a web request (`PhabricatorWebContentSource`).
pub const WEB_SOURCE: &str = r#"{"source":"web","params":[]}"#;

/// A transaction without a comment, its values and metadata already
/// encoded as PHP's `json_encode()` does.
#[derive(Debug, Clone, Copy)]
pub struct TransactionRow<'a> {
    /// The transaction PHID.
    pub phid: &'a str,
    /// The acting user, who is also the edit policy.
    pub author: &'a str,
    /// The edited object.
    pub object: &'a str,
    /// `transactionType`.
    pub kind: &'a str,
    /// The encoded old value.
    pub old: &'a str,
    /// The encoded new value.
    pub new: &'a str,
    /// The encoded metadata.
    pub metadata: &'a str,
    /// The encoded content source.
    pub content_source: &'a str,
}

/// Inserts `row` in `table` (like `database.table("project",
/// "project_transaction")`), as `populateTransaction()` fills it: public,
/// editable by its author.
pub async fn insert_transaction(
    connection: &mut MySqlConnection,
    table: &str,
    row: TransactionRow<'_>,
    now: i64,
) -> Result<(), DbError> {
    sqlx::query(&format!(
        "INSERT INTO {table} (phid, authorPHID, objectPHID, viewPolicy, editPolicy, \
         commentPHID, commentVersion, transactionType, oldValue, newValue, contentSource, \
         metadata, dateCreated, dateModified) VALUES (?, ?, ?, 'public', ?, NULL, 0, ?, ?, ?, \
         ?, ?, ?, ?)"
    ))
    .bind(row.phid)
    .bind(row.author)
    .bind(row.object)
    .bind(row.author)
    .bind(row.kind)
    .bind(row.old)
    .bind(row.new)
    .bind(row.content_source)
    .bind(row.metadata)
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// The `edge` table holding the edges of `src`.
fn edge_table(database: &Database, src: &str) -> Result<String, DbError> {
    let phid_type = Phid::new(src).phid_type();
    let name = edges::edge_database(phid_type)
        .ok_or_else(|| DbError::UnknownEdgeSource(phid_type.to_string()))?;
    Ok(database.table(name, "edge"))
}

/// One edge row without data, numbered `seq` among the edges its source
/// gets in the same `PhabricatorEdgeEditor::save()`. The inverse is a row
/// of its own: callers write both.
pub async fn insert_edge(
    connection: &mut MySqlConnection,
    database: &Database,
    (src, edge_type, dst): (&str, i64, &str),
    seq: i64,
    now: i64,
) -> Result<(), DbError> {
    sqlx::query(&format!(
        "INSERT INTO {} (src, type, dst, dateCreated, seq, dataID) VALUES (?, ?, ?, ?, ?, NULL) \
         ON DUPLICATE KEY UPDATE dataID = VALUES(dataID)",
        edge_table(database, src)?
    ))
    .bind(src)
    .bind(edge_type)
    .bind(dst)
    .bind(now)
    .bind(seq)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// Deletes one edge row.
pub async fn delete_edge(
    connection: &mut MySqlConnection,
    database: &Database,
    (src, edge_type, dst): (&str, i64, &str),
) -> Result<(), DbError> {
    sqlx::query(&format!(
        "DELETE FROM {} WHERE src = ? AND type = ? AND dst = ?",
        edge_table(database, src)?
    ))
    .bind(src)
    .bind(edge_type)
    .bind(dst)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// The edges one `PhabricatorEdgeEditor::save()` writes for a source:
/// the removed edges and their inverses go first, then the added ones,
/// numbered in order, with their inverses (the first edge of each
/// destination, so number 0).
pub async fn save_edges(
    connection: &mut MySqlConnection,
    database: &Database,
    (src, edge_type, inverse): (&str, i64, i64),
    (removed, added): (&[String], &[String]),
    now: i64,
) -> Result<(), DbError> {
    for dst in removed {
        delete_edge(connection, database, (src, edge_type, dst)).await?;
        delete_edge(connection, database, (dst, inverse, src)).await?;
    }
    for (seq, dst) in (0..).zip(added) {
        insert_edge(connection, database, (src, edge_type, dst), seq, now).await?;
        insert_edge(connection, database, (dst, inverse, src), 0, now).await?;
    }
    Ok(())
}
