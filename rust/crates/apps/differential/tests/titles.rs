//! Feed titles against PHP's: `tests/oracle/titles.php` rendered the cases
//! of `tests/oracle/titles.json` into `tests/oracle/titles.expected`.

use phab_app_differential::{Transaction, title};
use phab_core::Phid;
use serde_json::Value;

#[derive(serde::Deserialize)]
struct Case {
    #[serde(rename = "type")]
    kind: String,
    old: Value,
    new: Value,
    metadata: Value,
    author: String,
}

#[derive(serde::Deserialize)]
struct Expected {
    text: Option<String>,
}

/// The handles of `titles.php`, as text.
fn name(phid: &Phid) -> String {
    match phid.as_str() {
        "PHID-USER-alice" => "alice",
        "PHID-USER-bob" => "bob",
        "PHID-DREV-1" => "D1: Fix it",
        "PHID-PROJ-build" => "Build",
        "PHID-PROJ-two" => "Two",
        "PHID-USER-hidden" => "Restricted User",
        other => panic!("no handle for {other}"),
    }
    .to_owned()
}

#[test]
fn titles_match_php() {
    let directory = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle");
    let cases: Vec<Case> =
        serde_json::from_str(&std::fs::read_to_string(format!("{directory}/titles.json")).unwrap())
            .unwrap();
    let expected: Vec<Expected> = std::fs::read_to_string(format!("{directory}/titles.expected"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cases.len(), expected.len());
    let mut failures = Vec::new();
    for (case, expected) in cases.into_iter().zip(expected) {
        let transaction = Transaction {
            phid: Phid::new("PHID-XACT-DREV-1"),
            author_phid: Phid::new(&case.author),
            object_phid: Phid::new("PHID-DREV-1"),
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            comment_phid: None,
            transaction_type: case.kind.clone(),
            old_value: case.old,
            new_value: case.new,
            metadata: case.metadata,
        };
        let actual =
            title::feed_title(&transaction).map(|title| title.map(|title| title.to_text(name)));
        if actual != Ok(expected.text.clone()) {
            failures.push(format!(
                "{}: PHP {:?}, Rust {actual:?}",
                case.kind, expected.text
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
