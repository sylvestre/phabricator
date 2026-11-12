//! Replays Conduit calls against PHP and Rust and reports differences.
//!
//!     phab-replay <php-url> <rust-url> <scenarios.json>...
//!
//! A scenario file is a JSON list of calls:
//!
//!     [{"name": "whoami", "method": "user.whoami", "token": "api-...", "params": {}}]
//!
//! A token starting with `$` names an environment variable holding it.
//!
//! Calls to PHP carry `X-Phab-Skip-Rust`, so nginx sends them to PHP even
//! for routes the Rust service answers. Exits with 1 if any answer differs.

use std::process::ExitCode;

use phab_parity::{Rules, compare, summarize};
use serde::Deserialize;
use serde_json::{Map, Value};

pub const SKIP_RUST_HEADER: &str = "x-phab-skip-rust";

#[derive(Deserialize)]
struct Scenario {
    name: String,
    method: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    params: Map<String, Value>,
    /// Extra JSON Pointers to ignore for this call.
    #[serde(default)]
    ignore: Vec<String>,
}

async fn call(
    client: &reqwest::Client,
    base: &str,
    scenario: &Scenario,
    skip_rust: bool,
) -> Result<Value, String> {
    let mut params = scenario.params.clone();
    if let Some(token) = &scenario.token {
        let token = match token.strip_prefix('$') {
            Some(variable) => std::env::var(variable)
                .map_err(|_| format!("environment variable {variable} is not set"))?,
            None => token.clone(),
        };
        params.insert(
            "__conduit__".to_owned(),
            serde_json::json!({ "token": token }),
        );
    }
    let mut request = client
        .post(format!(
            "{}/api/{}",
            base.trim_end_matches('/'),
            scenario.method
        ))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "params={}",
            encode_component(&Value::Object(params).to_string())
        ));
    if skip_rust {
        request = request.header(SKIP_RUST_HEADER, "1");
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response.text().await.map_err(|error| error.to_string())?;
    serde_json::from_str(&body).map_err(|_| format!("HTTP {status}, not JSON: {body}"))
}

fn encode_component(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [php, rust, files @ ..] = arguments.as_slice() else {
        eprintln!("usage: phab-replay <php-url> <rust-url> <scenarios.json>...");
        return ExitCode::from(2);
    };

    let client = reqwest::Client::new();
    let mut failures = 0;
    for file in files {
        let scenarios: Vec<Scenario> = match std::fs::read_to_string(file)
            .map_err(|error| error.to_string())
            .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
        {
            Ok(scenarios) => scenarios,
            Err(error) => {
                eprintln!("{file}: {error}");
                return ExitCode::from(2);
            }
        };
        for scenario in &scenarios {
            let mut rules = Rules::for_method(&scenario.method);
            for path in &scenario.ignore {
                rules = rules.ignoring(path);
            }
            let answers = (
                call(&client, php, scenario, true).await,
                call(&client, rust, scenario, false).await,
            );
            match answers {
                (Ok(php_answer), Ok(rust_answer)) => {
                    let differences = compare(&php_answer, &rust_answer, &rules);
                    if differences.is_empty() {
                        println!("ok    {}", scenario.name);
                    } else {
                        failures += 1;
                        println!("DIFF  {}: {}", scenario.name, summarize(&differences));
                    }
                }
                (php_answer, rust_answer) => {
                    failures += 1;
                    println!(
                        "ERROR {}: php={:?} rust={:?}",
                        scenario.name,
                        php_answer.err(),
                        rust_answer.err()
                    );
                }
            }
        }
    }
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
