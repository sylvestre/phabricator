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

use phab_parity::Rules;
use serde::Deserialize;
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Error)]
enum ReplayError {
    #[error("environment variable {0} is not set")]
    MissingVariable(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("HTTP {status}, not JSON: {body}")]
    NotJson {
        status: reqwest::StatusCode,
        body: String,
    },
}

/// Which stack a call goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stack {
    Php,
    Rust,
}

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
    stack: Stack,
) -> Result<Value, ReplayError> {
    let mut params = scenario.params.clone();
    if let Some(token) = &scenario.token {
        let token = match token.strip_prefix('$') {
            Some(variable) => std::env::var(variable)
                .map_err(|_| ReplayError::MissingVariable(variable.to_owned()))?,
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
            form_urlencoded::byte_serialize(Value::Object(params).to_string().as_bytes())
                .collect::<String>()
        ));
    if stack == Stack::Php {
        request = request.header(phab_parity::SKIP_RUST_HEADER, "1");
    }
    let response = request.send().await?;
    let status = response.status();
    let body = response.text().await?;
    serde_json::from_str(&body).map_err(|_| ReplayError::NotJson { status, body })
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
                call(&client, php, scenario, Stack::Php).await,
                call(&client, rust, scenario, Stack::Rust).await,
            );
            match answers {
                (Ok(php_answer), Ok(rust_answer)) => {
                    let differences = phab_parity::compare(&php_answer, &rust_answer, &rules);
                    if differences.is_empty() {
                        println!("ok    {}", scenario.name);
                    } else {
                        failures += 1;
                        println!(
                            "DIFF  {}: {}",
                            scenario.name,
                            phab_parity::summarize(&differences)
                        );
                    }
                }
                (php_answer, rust_answer) => {
                    failures += 1;
                    let describe = |answer: Result<Value, ReplayError>| {
                        answer.map_or_else(|error| error.to_string(), |_| "ok".to_owned())
                    };
                    println!(
                        "ERROR {}: php: {}; rust: {}",
                        scenario.name,
                        describe(php_answer),
                        describe(rust_answer)
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
