//! Read side of `PhabricatorEdgeQuery`.
//!
//! Edges are stored in an `edge` table in the database of the application
//! owning the *source* object.

use std::fmt::Write as _;

use phab_core::{Phid, PhidType};

use crate::{Database, DbError, bind_all, placeholders, rows};

pub const PROJECT_HAS_MEMBER: u32 = 13;
pub const MEMBER_OF_PROJECT: u32 = 14;
pub const OBJECT_HAS_SUBSCRIBER: u32 = 21;
pub const SUBSCRIBED_TO_OBJECT: u32 = 22;
pub const OBJECT_HAS_PROJECT: u32 = 41;
pub const PROJECT_MATERIALIZED_MEMBER: u32 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub src: Phid,
    pub edge_type: u32,
    pub dst: Phid,
}

/// The database holding edges whose source has type `phid_type`.
pub fn edge_database(phid_type: PhidType) -> Option<&'static str> {
    let database = match phid_type {
        PhidType::USER => "user",
        PhidType::PROJECT => "project",
        PhidType::REVISION | PhidType::DIFF => "differential",
        PhidType::REPOSITORY | PhidType::COMMIT => "repository",
        PhidType::FILE => "file",
        PhidType::SPACE => "spaces",
        PhidType::POLICY => "policy",
        _ => return None,
    };
    Some(database)
}

/// Edges from any of `sources` with one of `edge_types`, optionally limited
/// to `destinations`, newest first like PHP.
pub async fn load_edges(
    database: &Database,
    sources: &[Phid],
    edge_types: &[u32],
    destinations: Option<&[Phid]>,
) -> Result<Vec<Edge>, DbError> {
    if sources.is_empty() || edge_types.is_empty() || destinations.is_some_and(<[Phid]>::is_empty) {
        return Ok(Vec::new());
    }

    let mut by_database: Vec<(&str, Vec<&Phid>)> = Vec::new();
    for source in sources {
        let name = edge_database(source.phid_type())
            .ok_or_else(|| DbError::UnknownEdgeSource(source.phid_type().to_string()))?;
        match by_database
            .iter_mut()
            .find(|(existing, _)| *existing == name)
        {
            Some((_, group)) => group.push(source),
            None => by_database.push((name, vec![source])),
        }
    }

    let mut edges = Vec::new();
    for (name, group) in by_database {
        let mut sql = format!(
            "SELECT src, type, dst FROM {} WHERE src IN ({}) AND type IN ({})",
            database.table(name, "edge"),
            placeholders(group.len()),
            placeholders(edge_types.len()),
        );
        if let Some(destinations) = destinations {
            let _ = write!(sql, " AND dst IN ({})", placeholders(destinations.len()));
        }
        sql.push_str(" ORDER BY dateCreated DESC, seq DESC");

        let mut query = bind_all(sqlx::query(&sql), group.iter().map(|phid| phid.as_str()));
        query = bind_all(query, edge_types.iter().copied());
        if let Some(destinations) = destinations {
            query = bind_all(query, destinations.iter().map(Phid::as_str));
        }
        for row in query.fetch_all(database.pool()).await? {
            edges.push(Edge {
                src: Phid::new(rows::text(&row, "src")?),
                edge_type: u32::try_from(rows::int(&row, "type")?).unwrap_or(u32::MAX),
                dst: Phid::new(rows::text(&row, "dst")?),
            });
        }
    }
    Ok(edges)
}

/// Destination PHIDs of the edges selected like [`load_edges`].
pub async fn load_destination_phids(
    database: &Database,
    sources: &[Phid],
    edge_types: &[u32],
    destinations: Option<&[Phid]>,
) -> Result<Vec<Phid>, DbError> {
    let edges = load_edges(database, sources, edge_types, destinations).await?;
    let mut result: Vec<Phid> = Vec::with_capacity(edges.len());
    for edge in edges {
        if !result.contains(&edge.dst) {
            result.push(edge.dst);
        }
    }
    Ok(result)
}
