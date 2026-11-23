//! The Herald fields of revisions (`HeraldDifferentialRevisionAdapter`)
//! which come from the revision itself, as the edit leaves it: each
//! field's `getHeraldFieldValue()`.

use phab_conduit::MethodError;
use phab_core::Phid;
use phab_db::phpser::{Key, Value as Php};
use phab_db::{Database, edges, rows};

use phab_diff::{Changeset, Include};

use super::{Facts, TextField};
use crate::storage::Revision;

/// The fields [`values()`] provides.
pub(super) const FIELDS: &[&str] = &[
    "new-object",
    "rule",
    "projects",
    "always",
    "contentsource",
    "herald.acting-user",
    "differential.revision.title",
    "differential.revision.summary",
    "differential.revision.test-plan",
    "differential.revision.author",
    "differential.revision.author.projects",
    "differential.revision.reviewers",
    "differential.revision.repository",
    "differential.revision.repository.projects",
    "revision.status",
    "cc",
    "differential.revision.diff.affected",
    "differential.revision.diff.content",
    "differential.revision.diff.new",
    "differential.revision.diff.old",
];

/// `PhabricatorProjectMaterializedMemberEdgeType`.
const PROJECT_MATERIALIZED_MEMBER: i64 = 60;

/// What the fields read beyond the revision and its facts.
pub(super) struct Input<'a> {
    pub(super) actor: &'a str,
    /// Whether the edit creates the revision.
    pub(super) is_new: bool,
    /// The summary and test plan the edit sets.
    pub(super) texts: Vec<(TextField, &'a str)>,
    /// Whether the edit unsubscribes anyone.
    pub(super) removes_subscribers: bool,
    /// Who the edit subscribes, in the order of their edges.
    pub(super) subscribed: Vec<String>,
    /// The projects the edit tags, in the order of their edges.
    pub(super) tagged: Vec<String>,
}

fn phid_list<'a>(phids: impl IntoIterator<Item = &'a String>) -> Php {
    Php::list(phids.into_iter().map(|phid| Php::str(phid)))
}

fn optional(phid: Option<&Phid>) -> Php {
    phid.map_or(Php::Null, |phid| Php::str(phid.as_str()))
}

/// The values of the fields of `used`, those the rules read.
pub(super) async fn values(
    database: &Database,
    revision: &Revision,
    facts: &Facts,
    input: &Input<'_>,
    used: &[String],
) -> Result<Vec<(String, Php)>, MethodError> {
    let mut values = Vec::new();
    let mut diff: Option<Vec<(String, Changeset)>> = None;
    for field in used {
        if let Some(mask) = diff_field(field) {
            if diff.is_none() {
                diff = Some(changesets(database, revision).await?);
            }
            let value = diff_value(diff.as_deref().unwrap_or_default(), mask);
            values.push((field.clone(), value));
            continue;
        }
        let value = match field.as_str() {
            "new-object" => Php::Bool(input.is_new),
            "rule" => Php::Null,
            "always" => Php::Bool(true),
            "contentsource" => Php::str("conduit"),
            "herald.acting-user" => Php::str(input.actor),
            "projects" => phid_list(&facts.projects),
            "cc" => phid_list(&facts.subscribers),
            "differential.revision.title" => Php::str(&revision.title),
            "revision.status" => Php::str(&revision.status),
            "differential.revision.author" => optional(revision.author_phid.as_ref()),
            "differential.revision.repository" => optional(revision.repository_phid.as_ref()),
            "differential.revision.summary" => {
                Php::str(&text(database, revision, input, TextField::Summary).await?)
            }
            "differential.revision.test-plan" => {
                Php::str(&text(database, revision, input, TextField::TestPlan).await?)
            }
            // mpull() of the reviewers, keyed by PHID.
            "differential.revision.reviewers" => Php::Array(
                facts
                    .reviewers
                    .iter()
                    .map(|reviewer| {
                        (
                            Key::from(reviewer.reviewer.as_str()),
                            Php::str(&reviewer.reviewer),
                        )
                    })
                    .collect(),
            ),
            "differential.revision.author.projects" => author_projects(database, revision).await?,
            "differential.revision.repository.projects" => match &revision.repository_phid {
                Some(repository) => phid_list(
                    &edges::load_edges(
                        database,
                        std::slice::from_ref(repository),
                        &[edges::OBJECT_HAS_PROJECT],
                        None,
                    )
                    .await?
                    .into_iter()
                    .map(|edge| edge.dst.as_str().to_owned())
                    .collect::<Vec<_>>(),
                ),
                None => Php::list([]),
            },
            _ => continue,
        };
        values.push((field.clone(), value));
    }
    Ok(values)
}

/// A field of the active diff.
#[derive(Clone, Copy)]
enum DiffField {
    /// The affected paths.
    Paths,
    /// The content with these lines (`loadContentWithMask()`).
    Content(Include),
}

fn diff_field(field: &str) -> Option<DiffField> {
    let lines = |added, removed| {
        Some(DiffField::Content(Include {
            added,
            removed,
            context: false,
        }))
    };
    match field {
        "differential.revision.diff.affected" => Some(DiffField::Paths),
        "differential.revision.diff.content" => lines(true, true),
        "differential.revision.diff.new" => lines(true, false),
        "differential.revision.diff.old" => lines(false, true),
        _ => None,
    }
}

/// `loadAffectedPaths()`, or each changeset's hunks' kept lines, the hunks
/// joined by newlines, by path.
fn diff_value(changesets: &[(String, Changeset)], field: DiffField) -> Php {
    let DiffField::Content(mask) = field else {
        return Php::list(changesets.iter().map(|(path, _)| Php::str(path)));
    };
    Php::Array(
        changesets
            .iter()
            .map(|(path, changeset)| {
                let content: Vec<String> = changeset
                    .hunks
                    .iter()
                    .map(|hunk| {
                        hunk.content(mask)
                            .into_iter()
                            .map(|(_, line)| line)
                            .collect()
                    })
                    .collect();
                (Key::from(path.as_str()), Php::str(&content.join("\n")))
            })
            .collect(),
    )
}

/// The summary or test plan after the edit: empty for a new revision
/// which doesn't set it.
async fn text(
    database: &Database,
    revision: &Revision,
    input: &Input<'_>,
    field: TextField,
) -> Result<String, phab_db::DbError> {
    if let Some((_, text)) = input.texts.iter().find(|(edited, _)| *edited == field) {
        return Ok((*text).to_owned());
    }
    if input.is_new {
        return Ok(String::new());
    }
    let sql = format!(
        "SELECT {} AS text FROM {} WHERE id = ?",
        field.column(),
        database.table("differential", "differential_revision")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(revision.id)
        .fetch_one(database.pool())
        .await?;
    rows::text(&row, "text")
}

/// `PhabricatorProjectQuery::withMemberPHIDs()` of the author, by name,
/// keyed by project ID (`mpull()` keeps the query's keys).
async fn author_projects(
    database: &Database,
    revision: &Revision,
) -> Result<Php, phab_db::DbError> {
    let Some(author) = &revision.author_phid else {
        return Ok(Php::list([]));
    };
    let sql = format!(
        "SELECT project.id, project.phid FROM {} project JOIN {} e ON e.src = project.phid AND \
         e.type = ? WHERE e.dst = ? ORDER BY project.name",
        database.table("project", "project"),
        database.table("project", "edge")
    );
    let found = phab_db::sqlx::query(&sql)
        .bind(PROJECT_MATERIALIZED_MEMBER)
        .bind(author.as_str())
        .fetch_all(database.pool())
        .await?;
    let mut projects = Vec::new();
    for row in &found {
        projects.push((
            Key::Int(rows::int(row, "id")?),
            Php::str(&rows::text(row, "phid")?),
        ));
    }
    Ok(Php::Array(projects))
}

/// The changesets of the active diff, in ID order (`loadChangesets()`),
/// with their hunks, each with its absolute repository path
/// (`getAbsoluteRepositoryPathForChangeset()`).
async fn changesets(
    database: &Database,
    revision: &Revision,
) -> Result<Vec<(String, Changeset)>, MethodError> {
    let Some(diff) = &revision.active_diff_phid else {
        return Err(MethodError::Unsupported(
            "Herald diff fields of revisions without diffs",
        ));
    };
    let sql = format!(
        "SELECT id, sourceControlPath FROM {} WHERE phid = ?",
        database.table("differential", "differential_diff")
    );
    let Some(row) = phab_db::sqlx::query(&sql)
        .bind(diff.as_str())
        .fetch_optional(database.pool())
        .await?
    else {
        return Err(MethodError::Unsupported(
            "Herald diff fields of missing diffs",
        ));
    };
    // The diff's path is a URI's: only plain paths are ported.
    let base = match rows::opt_text(&row, "sourceControlPath")? {
        None => "/".to_owned(),
        Some(path) if path.is_empty() => "/".to_owned(),
        Some(path) if path.starts_with('/') && !path.contains(['?', '#']) => path,
        Some(_) => return Err(MethodError::Unsupported("Herald diff fields of URI paths")),
    };
    let mut changesets = phab_diff::storage::load_changesets(database, rows::int(&row, "id")?)
        .await
        .map_err(MethodError::internal)?;
    changesets.reverse();
    Ok(changesets
        .into_iter()
        .map(|changeset| {
            let file = changeset.filename.trim_start_matches('/');
            let path = if revision.repository_phid.is_some() {
                format!("{}/{file}", base.trim_end_matches('/'))
            } else {
                format!("/{file}")
            };
            (path, changeset)
        })
        .collect())
}
