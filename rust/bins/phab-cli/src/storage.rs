//! `storage status`: `PhabricatorStorageManagementStatusWorkflow`, the
//! patches of `PhabricatorSQLPatchList::buildAllPatches()` (in
//! `patches.json`, from `rust/scripts/generate-catalogs.php storage`) and
//! whether `meta_data.patch_status` records them.

use std::collections::HashMap;
use std::sync::OnceLock;

use phab_db::{Database, rows};
use serde::Deserialize;

use crate::Context;

/// A schema patch.
#[derive(Debug, Clone, Deserialize)]
pub struct Patch {
    /// `getFullKey()`, like `phabricator:db.audit`.
    pub key: String,
    /// `default` or `worker`.
    pub phase: String,
    /// `db`, `sql` or `php`.
    #[serde(rename = "type")]
    pub patch_type: String,
    /// The name, with `{root}` for the Phabricator root in file paths.
    pub name: String,
}

/// Every patch, in PHP's order.
pub fn patches() -> &'static [Patch] {
    static PATCHES: OnceLock<Vec<Patch>> = OnceLock::new();
    PATCHES.get_or_init(|| {
        serde_json::from_str(include_str!("../patches.json")).expect("patches.json is a list")
    })
}

/// `PhutilNumber`: digits grouped by thousands.
fn thousands(number: u64) -> String {
    let digits = number.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `PhutilConsoleTable` without its header: each column but the last
/// padded to its widest cell or title, separated by a space.
fn draw_table(titles: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = titles.iter().map(|title| title.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut output = String::new();
    for row in rows {
        let last = row.len().saturating_sub(1);
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                if index == last {
                    cell.clone()
                } else {
                    format!("{cell:<width$}", width = widths[index])
                }
            })
            .collect();
        output.push_str(&cells.join(" "));
        output.push('\n');
    }
    output
}

/// The applied patches and their durations, or `None` when the table
/// can't be read (`getAppliedPatches()` returns null).
async fn applied(database: &Database) -> Option<HashMap<String, Option<u64>>> {
    let sql = format!(
        "SELECT patch, duration FROM {}",
        database.table("meta_data", "patch_status")
    );
    let found = phab_db::sqlx::query(&sql)
        .fetch_all(database.pool())
        .await
        .ok()?;
    found
        .iter()
        .map(|row| {
            let patch = rows::text(row, "patch").ok()?;
            let duration = rows::opt_int(row, "duration")
                .ok()?
                .map(|duration| u64::try_from(duration).unwrap_or_default());
            Some((patch, duration))
        })
        .collect()
}

/// `PhabricatorDatabaseRef::getRefKey()` of the configured server.
fn ref_key(config: &phab_core::Config) -> String {
    let host = config.string("mysql.host").unwrap_or("localhost");
    let port = match config.value("mysql.port") {
        Some(serde_json::Value::String(port)) if !port.is_empty() => Some(port.clone()),
        Some(serde_json::Value::Number(port)) => Some(port.to_string()),
        _ => None,
    };
    match port {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// Runs `storage <workflow>`.
pub async fn run(context: &mut Context<'_>, arguments: &[String]) -> u8 {
    match arguments {
        [workflow] if workflow == "status" => status(context).await,
        [workflow, ..] if workflow != "status" => {
            context.left_to_php(&format!("storage {workflow}"), "bin/storage")
        }
        _ => context.usage_error("Usage: storage status"),
    }
}

async fn status(context: &mut Context<'_>) -> u8 {
    let (config, database) = match phab_worker::connect(&context.config_dir).await {
        Ok(connected) => connected,
        Err(error) => return context.error(&error),
    };
    let Some(applied) = applied(&database).await else {
        let _ = writeln!(
            context.out,
            "Database Not Initialized: Run ./bin/storage upgrade to initialize."
        );
        return 1;
    };
    let host = ref_key(&config);
    let root = context.root().display().to_string();
    let rows: Vec<Vec<String>> = patches()
        .iter()
        .map(|patch| {
            let (status, duration) = match applied.get(&patch.key) {
                Some(duration) => ("Applied", *duration),
                None => ("Not Applied", None),
            };
            vec![
                patch.key.clone(),
                patch.phase.clone(),
                host.clone(),
                status.to_owned(),
                duration.map_or_else(|| "-".to_owned(), |us| format!("{} us", thousands(us))),
                patch.patch_type.clone(),
                patch.name.replace("{root}", &root),
            ]
        })
        .collect();
    let titles = ["ID", "Phase", "Host", "Status", "Duration", "Type", "Name"];
    let _ = context.out.write_all(draw_table(&titles, &rows).as_bytes());
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_pad_all_but_the_last_column() {
        let rows = vec![
            vec!["a".to_owned(), "x".to_owned(), "last".to_owned()],
            vec!["abcdef".to_owned(), "yy".to_owned(), "z".to_owned()],
        ];
        assert_eq!(
            draw_table(&["ID", "Phase", "N"], &rows),
            "a      x     last\nabcdef yy    z\n"
        );
    }

    #[test]
    fn the_patch_list_loads() {
        assert!(patches().len() > 1000);
        assert_eq!(patches()[0].key, "phabricator:db.audit");
        assert_eq!(thousands(29_640), "29,640");
    }
}
