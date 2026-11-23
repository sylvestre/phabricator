//! `PhabricatorMetaMTAMail::sendNow()` and `sendWithMailers()`.

use std::sync::{Arc, Mutex};

use phab_core::Config;
use phab_db::{Database, DbError};
use serde_json::{Map, Value, json};

use crate::actors::{self, Parties};
use crate::engine::{self, Composed, Composer, EmailMessage};
use crate::mail::{self, Mail};
use crate::mailer::{self, Adapter, MESSAGE_TYPE_EMAIL, Mailer};
use crate::{Error, phpmailer, smtp};

/// A message the test adapter accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// The mailer's key.
    pub mailer: String,
    /// The message.
    pub message: EmailMessage,
}

impl Recorded {
    /// `PhabricatorMailTestAdapter::getGuts()` after the send.
    pub fn guts(&self) -> Value {
        let message = &self.message;
        let mut guts = Map::new();
        guts.insert("from".to_owned(), json!(message.from.to_php_string()));
        if let Some(reply_to) = &message.reply_to {
            guts.insert("reply-to".to_owned(), json!(reply_to.to_php_string()));
        }
        let list = |addresses: &[crate::address::EmailAddress]| -> Value {
            addresses
                .iter()
                .map(|address| json!(address.to_php_string()))
                .collect()
        };
        guts.insert("tos".to_owned(), list(&message.to));
        guts.insert("ccs".to_owned(), list(&message.cc));
        if !message.subject.is_empty() {
            guts.insert("subject".to_owned(), json!(message.subject));
        }
        guts.insert(
            "headers".to_owned(),
            message
                .headers
                .iter()
                .map(|(name, value)| json!([name, value]))
                .collect(),
        );
        if !message.text_body.is_empty() {
            guts.insert("body".to_owned(), json!(message.text_body));
        }
        if let Some(html) = &message.html_body {
            guts.insert("html-body".to_owned(), json!(html));
        }
        guts.insert("attachments".to_owned(), json!([]));
        guts.insert("did-send".to_owned(), json!(true));
        Value::Object(guts)
    }
}

/// Where test mailers put what they send.
pub type Outbox = Arc<Mutex<Vec<Recorded>>>;

/// What sending a mail amounts to, decided without writing.
#[derive(Debug)]
pub enum Plan {
    /// No such mail: the task fails permanently.
    Missing,
    /// Not queued (`doWork()` returns): nothing is written.
    NotQueued,
    /// The mail is saved as it is, without sending (voided).
    Write(Mail),
    /// The mail is sent, then saved.
    Deliver(Box<Delivery>),
}

/// A mail to send.
#[derive(Debug)]
pub struct Delivery {
    /// The mail, with what the message engine stored.
    pub mail: Mail,
    composer: Composer,
    mailers: Vec<Mailer>,
    actors_sent: Value,
    routing_sent: Value,
    routing_map_sent: Value,
}

/// How sending ended, once the row is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// `sent` or `void`: the task succeeds.
    Done,
    /// Every mailer failed: the message is saved and the task fails with
    /// this message, to be retried.
    Failed(String),
}

/// Voids the mail, replacing its message if given.
fn void(mut mail: Mail, message: Option<String>) -> Plan {
    mail::STATUS_VOID.clone_into(&mut mail.status);
    if let Some(message) = message {
        mail.message = Some(message);
    }
    Plan::Write(mail)
}

/// Reads what `sendNow()` reads and decides what it would do.
pub async fn plan(database: &Database, config: &Config, id: i64) -> Result<Plan, Error> {
    let Some(mut mail) = mail::load(database, id).await? else {
        return Ok(Plan::Missing);
    };
    if mail.status != mail::STATUS_QUEUE {
        return Ok(Plan::NotQueued);
    }

    let message_type = mail
        .string_param("message.type")?
        .unwrap_or(MESSAGE_TYPE_EMAIL)
        .to_owned();
    let mut mailers = mailer::new_mailers(config, Some(&message_type))?;
    if let Some(keys) = mail
        .param("mailers.try")
        .filter(|keys| phab_core::php::is_truthy(keys))
    {
        let keys = crate::php::strings(keys)
            .ok_or_else(|| Error::unsupported("mailers.try which is not a list of keys"))?;
        mailers = mailer::select(mailers, &keys);
    }
    if mailers.is_empty() {
        let message = if mailer::new_mailers(config, None)?.is_empty() {
            "No mailers are configured."
        } else {
            "No configured mailers support sending outbound mail."
        };
        return Ok(void(mail, Some(message.to_owned())));
    }

    let parties = Parties::of(&mail)?;
    let recipients = actors::load(database, config, &mail, &parties).await?;
    let target = parties
        .to
        .first()
        .filter(|phid| phab_core::php::is_truthy_str(phid))
        .or_else(|| parties.cc.first());
    let preferences = recipients.preferences_for(target.map(String::as_str), config);

    if mail.flag("attachments") {
        // PHP attaches the files to the mail (edges) and sends them.
        return Err(Error::unsupported("mail with attachments"));
    }
    // Every mailer left sends email, the only message type.

    let composer = match engine::compose(&mut mail, &recipients, &preferences, config)? {
        Composed::Nothing => return Ok(void(mail, None)),
        Composed::Message(composer) => *composer,
    };
    for mailer in &mailers {
        match &mailer.adapter {
            Adapter::Unported(kind) => {
                return Err(Error::unsupported(format!(
                    "sending with the {kind} mailer"
                )));
            }
            Adapter::Smtp(_) => {
                // PHPMailer throws on addresses it rejects; Rust only knows
                // the ones it certainly accepts.
                let message = composer.message(mailer.supports_message_id(), "", 0);
                if let Err(error) = phpmailer::render(&message, "", "")
                    && error.starts_with("Invalid address")
                {
                    return Err(Error::unsupported("an address PHPMailer may reject"));
                }
            }
            Adapter::Test => {}
        }
    }

    Ok(Plan::Deliver(Box::new(Delivery {
        actors_sent: actors::delivered_actors(&recipients),
        routing_sent: mail.param("routing").cloned().unwrap_or(Value::Null),
        routing_map_sent: actors::delivered_routing_map(&mail)?,
        mail,
        composer,
        mailers,
    })))
}

/// `Date` of `PHPMailer`: `now` in the default time zone.
fn date(config: &Config, now: i64) -> String {
    let zone = jiff::tz::TimeZone::get(&phab_app_settings::setting::default_timezone(config))
        .unwrap_or(jiff::tz::TimeZone::UTC);
    let timestamp = jiff::Timestamp::from_second(now).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    let zoned = timestamp.to_zoned(zone);
    let local = zoned.strftime("%a, %-d %b %Y %H:%M:%S").to_string();
    phpmailer::rfc_date(&local, zoned.offset().seconds())
}

/// Sends a planned mail and writes the row as `sendWithMailers()` does.
/// Test mailers record what they send in `outbox`, if given; PHP's test
/// adapter keeps it in memory only.
pub async fn deliver(
    database: &Database,
    config: &Config,
    delivery: Delivery,
    outbox: Option<&Outbox>,
    now: i64,
) -> Result<Sent, DbError> {
    let Delivery {
        mut mail,
        composer,
        mailers,
        actors_sent,
        routing_sent,
        routing_map_sent,
    } = delivery;
    let mut failures = Vec::new();
    for mailer in &mailers {
        let attempt = phab_core::random::random_characters(16);
        let message = composer.message(mailer.supports_message_id(), &attempt, now);
        let result = match &mailer.adapter {
            Adapter::Test => {
                if let Some(outbox) = outbox {
                    outbox
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(Recorded {
                            mailer: mailer.key.clone(),
                            message,
                        });
                }
                Ok(())
            }
            Adapter::Smtp(options) => {
                let unique = hex::encode(rand::random::<[u8; 16]>());
                match phpmailer::render(&message, &date(config, now), &unique) {
                    Ok(rendered) => smtp::send(options, &rendered).await,
                    Err(error) => Err(error),
                }
            }
            // Declined by `plan()`.
            Adapter::Unported(kind) => Err(format!("the {kind} mailer is not ported")),
        };
        match result {
            Ok(()) => {
                mail.set_param("mailer.key", Value::String(mailer.key.clone()));
                mail.set_param("actors.sent", actors_sent);
                mail.set_param("routing.sent", routing_sent);
                mail.set_param("routingmap.sent", routing_map_sent);
                mail::STATUS_SENT.clone_into(&mut mail.status);
                mail::save(database, &mail, now).await?;
                return Ok(Sent::Done);
            }
            Err(error) => failures.push(error),
        }
    }
    // No mailer took it: the message is saved, the status left queued.
    mail.message = Some(failures.join("\n\n"));
    mail::save(database, &mail, now).await?;
    Ok(Sent::Failed(if failures.len() == 1 {
        failures.remove(0)
    } else {
        "Encountered multiple exceptions while transmitting mail.".to_owned()
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;

    #[test]
    fn dates_in_the_default_time_zone() {
        let mut config = Config::default();
        assert_eq!(
            date(&config, 1_700_000_000),
            "Tue, 14 Nov 2023 22:13:20 +0000"
        );
        let mut layer = Map::new();
        layer.insert("phabricator.timezone".to_owned(), json!("America/New_York"));
        config.push_layer("test", layer);
        assert_eq!(
            date(&config, 1_700_000_000),
            "Tue, 14 Nov 2023 17:13:20 -0500"
        );
    }
}
