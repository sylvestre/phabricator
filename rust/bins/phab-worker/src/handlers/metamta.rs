//! `PhabricatorMetaMTAWorker`, which `PhabricatorMetaMTAMail::save()`
//! queues for each new mail with the mail ID as its data: it loads the
//! `metamta_mail` row and, while it is queued, sends it
//! (`PhabricatorMetaMTAMail::sendNow()`, [`phab_app_metamta::send`]).
//!
//! Mail Rust can't send exactly as PHP does is declined before the lease:
//! unported mailers (`ses`, ...) when there is a message to deliver,
//! attachments, error mail, subprojects among the recipients, recipients
//! without a cached preferences, truncated bodies.

use phab_app_metamta::Error;
use phab_app_metamta::send::{self, Outbox, Plan, Sent};
use phab_core::Config;
use phab_db::Database;
use serde_json::Value;

use crate::handler::{Claim, ClaimFuture, Failure, Handler, Unleased, Work, WorkFuture};
use crate::task::ActiveTask;

/// `PhabricatorMetaMTAWorker`.
#[derive(Debug, Clone, Default)]
pub struct MetaMTAWorker {
    outbox: Option<Outbox>,
}

impl MetaMTAWorker {
    /// A worker whose test mailers record what they send in `outbox`.
    pub fn with_outbox(outbox: Outbox) -> Self {
        Self {
            outbox: Some(outbox),
        }
    }
}

/// The mail ID `PhabricatorMetaMTAMail::load()` looks up: `None` for the
/// values it returns `null` for without a query (falsy, or neither an
/// integer nor a string of digits).
fn mail_id(data: &Value) -> Result<Option<i64>, Failure> {
    match data {
        Value::Number(number) => match number.as_i64() {
            Some(id) => Ok(Some(id).filter(|id| *id != 0)),
            // A float.
            None => Ok(None),
        },
        Value::String(text)
            if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            match text.parse::<i64>() {
                Ok(id) => Ok(Some(id).filter(|id| *id != 0)),
                Err(_) => Err(Failure::Decline("a mail ID beyond 64 bits".to_owned())),
            }
        }
        Value::Null | Value::Bool(_) | Value::String(_) => Ok(None),
        Value::Array(_) | Value::Object(_) => Err(Failure::Decline(
            "task data which is not a mail ID".to_owned(),
        )),
    }
}

/// The failure of a task whose mail does not load.
fn missing(data: &Value) -> Failure {
    Failure::Permanent(format!(
        "Unable to load mail message (with ID \"{}\") while preparing to deliver it.",
        phab_core::php::string_cast(data)
    ))
}

/// Plans the task; `Ok(None)` when there is no mail to load.
async fn plan(data: &Value, database: &Database, config: &Config) -> Result<Option<Plan>, Failure> {
    let Some(id) = mail_id(data)? else {
        return Ok(None);
    };
    match send::plan(database, config, id).await {
        Ok(plan) => Ok(Some(plan)),
        Err(Error::Unsupported(reason)) => Err(Failure::Decline(reason)),
        Err(Error::Db(error)) => Err(Failure::Temporary(error.to_string())),
    }
}

impl Handler for MetaMTAWorker {
    fn class(&self) -> &'static str {
        "PhabricatorMetaMTAWorker"
    }

    fn maximum_retry_count(&self, _task: &ActiveTask) -> Result<Option<i64>, Failure> {
        Ok(Some(250))
    }

    fn wait_before_retry(&self, task: &ActiveTask) -> Result<Option<i64>, Failure> {
        Ok(Some(task.failure_count * 15))
    }

    fn inspect<'a>(&'a self, task: &'a Unleased<'a>) -> ClaimFuture<'a> {
        Box::pin(async move {
            match plan(task.data, task.database, task.config).await {
                Ok(_) => Claim::Take,
                // A query failing now is left to PHP too.
                Err(failure) => Claim::Decline(failure.to_string()),
            }
        })
    }

    fn do_work<'a>(&'a self, work: &'a Work<'a>) -> WorkFuture<'a> {
        Box::pin(async move {
            let plan = plan(work.data(), work.database, work.config).await?;
            match plan {
                None | Some(Plan::Missing) => Err(missing(work.data())),
                Some(Plan::NotQueued) => Ok(()),
                Some(Plan::Write(mail)) => {
                    phab_app_metamta::mail::save(work.database, &mail, crate::time())
                        .await
                        .map_err(|error| Failure::Temporary(error.to_string()))
                }
                Some(Plan::Deliver(delivery)) => {
                    let sent = send::deliver(
                        work.database,
                        work.config,
                        *delivery,
                        self.outbox.as_ref(),
                        crate::time(),
                    )
                    .await
                    .map_err(|error| Failure::Temporary(error.to_string()))?;
                    match sent {
                        Sent::Done => Ok(()),
                        Sent::Failed(message) => Err(Failure::Temporary(message)),
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_mail_ids_like_lisk() {
        assert_eq!(mail_id(&json!(81)).unwrap(), Some(81));
        assert_eq!(mail_id(&json!("81")).unwrap(), Some(81));
        assert_eq!(mail_id(&json!(0)).unwrap(), None);
        assert_eq!(mail_id(&json!("0")).unwrap(), None);
        assert_eq!(mail_id(&json!("8a")).unwrap(), None);
        assert_eq!(mail_id(&json!(1.5)).unwrap(), None);
        assert_eq!(mail_id(&Value::Null).unwrap(), None);
        assert!(mail_id(&json!([81])).is_err());
    }
}
