//! Random Remarkup rendered by PHP (`tests/oracle/generate.php`), compared
//! with the Rust engine. Runs only with `PHAB_REMARKUP_ORACLE` set to the
//! generated file, as it needs PHP and libphutil.

mod common;

use phab_remarkup::Mode;

#[derive(serde::Deserialize)]
struct Case {
    input: String,
    html: Option<String>,
    text: Option<String>,
}

#[test]
fn random_documents_match_php() {
    let Some(path) = std::env::var_os("PHAB_REMARKUP_ORACLE") else {
        return;
    };
    let cases: Vec<Case> =
        serde_json::from_slice(&std::fs::read(path).expect("readable file")).expect("JSON cases");
    let engine = common::engine(common::config());
    let mut failures = Vec::new();
    for case in &cases {
        for (mode, expected) in [(Mode::Html, &case.html), (Mode::Text, &case.text)] {
            let Some(expected) = expected else {
                continue;
            };
            let actual = engine.markup(case.input.as_bytes(), mode);
            let actual = String::from_utf8_lossy(actual.output.as_bytes());
            if actual != *expected {
                failures.push(format!(
                    "{mode:?} {:?}\n--- php\n{expected}\n--- rust\n{actual}",
                    case.input
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} renderings differ:\n\n{}",
        failures.len(),
        cases.len() * 2,
        failures
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}
