//! `PhabricatorTestWorker`, the worker `PhabricatorWorkerTestCase` runs:
//! its data says how long to take and how to end.

use std::time::Duration;

use serde_json::Value;

use crate::handler::{Failure, Handler, Work, WorkFuture};
use crate::task::ActiveTask;

/// `PhabricatorTestWorker`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestWorker;

/// `idx($this->getTaskData(), $key)`. `idx()` takes an `array`, so data
/// which is not one throws a `TypeError`.
fn idx<'a>(data: &'a Value, key: &str) -> Result<Option<&'a Value>, Failure> {
    match data {
        Value::Object(map) => Ok(map.get(key).filter(|value| !value.is_null())),
        // A JSON list decodes to a PHP list, whose keys are integers.
        Value::Array(items) => Ok(key
            .parse::<usize>()
            .ok()
            .and_then(|index| items.get(index))
            .filter(|value| !value.is_null())),
        _ => Err(Failure::Temporary(
            "idx(): Argument #1 ($array) must be of type array".to_owned(),
        )),
    }
}

/// PHP's `(int)` of a value used in arithmetic or a comparison.
fn php_int(value: &Value) -> i64 {
    match value {
        Value::Bool(flag) => i64::from(*flag),
        Value::Number(number) => number.as_i64().unwrap_or_else(|| {
            // `(int)` of a float truncates toward zero.
            #[allow(clippy::cast_possible_truncation)]
            let truncated = number.as_f64().unwrap_or(0.0) as i64;
            truncated
        }),
        Value::String(text) => {
            let trimmed = text.trim_start();
            let end = trimmed
                .char_indices()
                .find(|&(index, character)| {
                    !(character.is_ascii_digit() || (index == 0 && matches!(character, '-' | '+')))
                })
                .map_or(trimmed.len(), |(index, _)| index);
            trimmed[..end].parse().unwrap_or(0)
        }
        Value::Null | Value::Array(_) | Value::Object(_) => 0,
    }
}

fn hook(task: &ActiveTask, key: &str) -> Result<Option<i64>, Failure> {
    Ok(idx(&task.data, key)?.map(php_int))
}

impl Handler for TestWorker {
    fn class(&self) -> &'static str {
        "PhabricatorTestWorker"
    }

    fn required_lease_time(&self, task: &ActiveTask) -> Result<Option<i64>, Failure> {
        hook(task, "getRequiredLeaseTime")
    }

    fn maximum_retry_count(&self, task: &ActiveTask) -> Result<Option<i64>, Failure> {
        hook(task, "getMaximumRetryCount")
    }

    fn wait_before_retry(&self, task: &ActiveTask) -> Result<Option<i64>, Failure> {
        hook(task, "getWaitBeforeRetry")
    }

    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a> {
        Box::pin(async move {
            let data = work.data();
            if let Some(duration) = idx(data, "duration")?
                && phab_core::php::is_truthy(duration)
            {
                let seconds = duration.as_f64().unwrap_or_else(|| {
                    #[allow(clippy::cast_precision_loss)]
                    let seconds = php_int(duration) as f64;
                    seconds
                });
                if seconds > 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
                }
            }
            match idx(data, "doWork")?.and_then(Value::as_str) {
                Some("fail-temporary") => Err(Failure::Temporary("Temporary failure!".to_owned())),
                Some("fail-permanent") => Err(Failure::Permanent("Permanent failure!".to_owned())),
                _ => Ok(()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_data_like_idx() {
        let data = json!({"a": 1, "b": null});
        assert_eq!(idx(&data, "a").unwrap(), Some(&json!(1)));
        assert_eq!(idx(&data, "b").unwrap(), None);
        assert_eq!(idx(&data, "c").unwrap(), None);
        assert_eq!(idx(&json!([]), "a").unwrap(), None);
        assert!(idx(&Value::Null, "a").is_err());
    }

    #[test]
    fn casts_like_php() {
        assert_eq!(php_int(&json!(-60)), -60);
        assert_eq!(php_int(&json!(2.9)), 2);
        assert_eq!(php_int(&json!("15 seconds")), 15);
        assert_eq!(php_int(&json!("-3")), -3);
        assert_eq!(php_int(&json!("x")), 0);
        assert_eq!(php_int(&json!(true)), 1);
    }
}
