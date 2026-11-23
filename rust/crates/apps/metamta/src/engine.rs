//! `PhabricatorMailEmailEngine::newMessage()`: the message a mailer is
//! handed (`PhabricatorMailEmailMessage`).

use base64::Engine as _;
use md5::{Digest as _, Md5};
use phab_core::Config;
use serde_json::Value;

use crate::actors::{Actor, Preferences, Recipients};
use crate::address::EmailAddress;
use crate::mail::Mail;
use crate::{Error, SERVER_NAME, php};

/// The message of a mail without valid recipients.
pub const NO_RECIPIENTS: &str = "Message has no valid recipients: all To/CC are disabled, \
                                 invalid, or configured not to receive this mail.";
/// The message of a mail not sent because of `phabricator.silent`.
pub const SILENT: &str = "This software is running in silent mode. See `phabricator.silent` in \
                          the configuration to change this setting.";

/// The headers kept when the content must be encrypted (`filterHeaders()`).
const ENCRYPTED_HEADERS: &[&str] = &[
    "in-reply-to",
    "message-id",
    "precedence",
    "references",
    "thread-index",
    "thread-topic",
    "x-mail-transport-agent",
    "x-auto-response-suppress",
    "x-phabricator-sent-this-message",
    "x-phabricator-must-encrypt",
    "x-phabricator-mail-id",
    "x-phabricator-send-attempt",
];

/// `PhabricatorMailEmailMessage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailMessage {
    /// `getFromAddress()`.
    pub from: EmailAddress,
    /// `getReplyToAddress()`.
    pub reply_to: Option<EmailAddress>,
    /// `getToAddresses()`.
    pub to: Vec<EmailAddress>,
    /// `getCCAddresses()`.
    pub cc: Vec<EmailAddress>,
    /// `getSubject()`.
    pub subject: String,
    /// `getHeaders()`, as name and value.
    pub headers: Vec<(String, String)>,
    /// `getTextBody()`.
    pub text_body: String,
    /// `getHTMLBody()`.
    pub html_body: Option<String>,
}

/// The threading of a mail (`setThreadID()`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Thread {
    /// `<thread-id@domain>`.
    id: String,
    is_first: bool,
    parent: Option<String>,
}

/// Everything of the message but what depends on the mailer and the
/// attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composer {
    from: EmailAddress,
    reply_to: Option<EmailAddress>,
    to: Vec<EmailAddress>,
    cc: Vec<EmailAddress>,
    subject: String,
    /// `newEmailHeaders()` up to `X-Phabricator-Mail-ID`.
    headers: Vec<(String, String)>,
    thread: Option<Thread>,
    stamps: Option<String>,
    must_encrypt: bool,
    text_body: String,
    html_body: Option<String>,
}

/// What `newMessage()` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Composed {
    /// `null`, with the mail message set: nobody to send to, or silent
    /// mode. The mail is voided.
    Nothing,
    /// A message.
    Message(Box<Composer>),
}

impl Composer {
    /// The message for a mailer, `send_attempt` being the random
    /// `X-Phabricator-Send-Attempt` and `now` the time in `Thread-Index`.
    pub fn message(&self, supports_message_id: bool, send_attempt: &str, now: i64) -> EmailMessage {
        let mut headers = self.headers.clone();
        headers.push((
            "X-Phabricator-Send-Attempt".to_owned(),
            send_attempt.to_owned(),
        ));
        if let Some(thread) = &self.thread {
            if thread.is_first && supports_message_id {
                headers.push(("Message-ID".to_owned(), thread.id.clone()));
            } else {
                let mut in_reply_to = thread.id.clone();
                let mut references = thread.id.clone();
                if let Some(parent) = &thread.parent {
                    in_reply_to.clone_from(parent);
                    references.push(' ');
                    references.push_str(parent);
                }
                headers.push(("In-Reply-To".to_owned(), in_reply_to));
                headers.push(("References".to_owned(), references));
            }
            headers.push((
                "Thread-Index".to_owned(),
                thread_index(&thread.id, thread.is_first, now),
            ));
        }
        if let Some(stamps) = &self.stamps {
            headers.push(("X-Phabricator-Stamps".to_owned(), stamps.clone()));
        }
        if self.must_encrypt {
            headers.retain(|(name, _)| ENCRYPTED_HEADERS.contains(&name.to_lowercase().as_str()));
        }
        EmailMessage {
            from: self.from.clone(),
            reply_to: self.reply_to.clone(),
            to: self.to.clone(),
            cc: self.cc.clone(),
            subject: self.subject.clone(),
            headers,
            text_body: self.text_body.clone(),
            html_body: self.html_body.clone(),
        }
    }
}

/// `generateThreadIndex()`: the first 27 hex digits of the MD5 of the
/// thread ID, then a space and the time for replies.
fn thread_index(seed: &str, is_first: bool, now: i64) -> String {
    let digest = hex::encode(Md5::digest(seed.as_bytes()));
    let mut base = digest.as_bytes()[..27].to_vec();
    if !is_first {
        base.push(b' ');
        // `pack('N', time())`: 32 bits, big-endian.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "PHP packs the low 32 bits"
        )]
        base.extend_from_slice(&(now as u32).to_be_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(base)
}

/// The domain of a URI (`PhutilURI::getDomain()`).
fn uri_domain(uri: &str) -> &str {
    let Some((_, rest)) = uri.split_once("://") else {
        return "";
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    host.rsplit_once(':').map_or(host, |(host, port)| {
        if port.bytes().all(|byte| byte.is_ascii_digit()) {
            host
        } else {
            authority
        }
    })
}

/// `newMailDomain()`.
fn mail_domain(config: &Config) -> Result<String, Error> {
    if let Some(domain) = config.value("metamta.reply-handler-domain")
        && phab_core::php::is_truthy(domain)
    {
        return Ok(phab_core::php::string_cast(domain).into_owned());
    }
    let uri = config
        .uri("/")
        .map_err(|_| Error::unsupported("mail without phabricator.base-uri"))?;
    Ok(uri_domain(&uri).to_owned())
}

/// `newDefaultEmailAddress()`.
pub fn default_address(config: &Config) -> Result<EmailAddress, Error> {
    let raw = match config.value("metamta.default-address") {
        Some(value) if phab_core::php::is_truthy(value) => {
            phab_core::php::string_cast(value).into_owned()
        }
        _ => format!("noreply@{}", mail_domain(config)?),
    };
    let mut address = EmailAddress::parse(&raw);
    if address.display_name().is_empty() {
        address.display_name = Some(SERVER_NAME.to_owned());
    }
    Ok(address)
}

/// `PhabricatorEnv::getProductionURI()`.
fn production_uri(config: &Config, path: &str) -> Result<String, Error> {
    if !uri_domain(path).is_empty() {
        return Ok(path.to_owned());
    }
    let base = match config.value("phabricator.production-uri") {
        Some(Value::String(uri)) if phab_core::php::is_truthy_str(uri) => uri.clone(),
        _ => config
            .base_uri()
            .map_err(|_| Error::unsupported("mail without phabricator.base-uri"))?
            .to_owned(),
    };
    Ok(format!("{}{path}", base.trim_end_matches('/')))
}

fn bad(what: &str) -> Error {
    Error::unsupported(format!("mail with {what} PHP would fail on"))
}

/// The addresses of the deliverable actors among `phids`, expanded
/// (`newEmailAddressesFromActorPHIDs()`).
fn actor_addresses(recipients: &Recipients, phids: &[String]) -> Result<Vec<EmailAddress>, Error> {
    let mut addresses = Vec::new();
    for phid in recipients.expand(phids) {
        let Some(actor) = recipients.actor(&phid) else {
            continue;
        };
        if !actor.is_deliverable() {
            continue;
        }
        // Forced delivery reaches actors without an address.
        let email = actor
            .email
            .as_deref()
            .ok_or_else(|| Error::unsupported("forced mail to an actor without an address"))?;
        addresses.push(EmailAddress::parse(email));
    }
    Ok(addresses)
}

/// `getUniqueEmailAddresses()`.
fn unique(addresses: Vec<EmailAddress>, exclude: &[EmailAddress]) -> Vec<EmailAddress> {
    let mut seen: Vec<String> = exclude.iter().map(EmailAddress::address).collect();
    let mut kept = Vec::new();
    for address in addresses {
        let raw = address.address();
        if seen.contains(&raw) {
            continue;
        }
        seen.push(raw);
        kept.push(address);
    }
    kept
}

/// `newMessage()` up to the mailer: sets the mail message, `body.sent` and
/// the delivered headers as PHP does.
pub fn compose(
    mail: &mut Mail,
    recipients: &Recipients,
    preferences: &Preferences,
    config: &Config,
) -> Result<Composed, Error> {
    let must_encrypt = mail.flag("mustEncrypt");
    let from_phid = mail.string_param("from")?.unwrap_or_default().to_owned();
    let from_actor = recipients.actor(&from_phid);

    let from = from_address(mail, from_actor, config, must_encrypt)?;
    let reply_to = reply_to_address(mail, from_actor)?;

    let to_phids = mail.strings_param("to")?;
    let mut to = actor_addresses(recipients, &to_phids)?;
    for raw in mail.strings_param("raw-to")? {
        to.push(EmailAddress::parse(&raw));
    }
    let cc = actor_addresses(recipients, &mail.strings_param("cc")?)?;

    if to.is_empty() && cc.is_empty() {
        mail.message = Some(NO_RECIPIENTS.to_owned());
        return Ok(Composed::Nothing);
    }
    if mail.flag("is-error") {
        // Rate limited through `PhabricatorSystemActionEngine`.
        return Err(Error::unsupported("error mail"));
    }
    if to.is_empty() {
        to.push(default_address(config)?);
    }
    let to = unique(to, &[]);
    let cc = unique(cc, &to);

    let subject = subject(mail, preferences, config, must_encrypt)?;
    let headers = headers(mail)?;

    let thread = thread(mail, config)?;
    let stamps = stamps(mail)?;

    let raw_body = mail.param("body").cloned();
    let body = body(mail, raw_body.as_ref(), config, must_encrypt)?;
    let mut body_limit = match config.value("metamta.email-body-limit") {
        Some(Value::Number(limit)) if limit.is_i64() => limit.as_i64().unwrap_or_default(),
        _ => {
            return Err(Error::unsupported(
                "a metamta.email-body-limit which is not an integer",
            ));
        }
    };
    let length = i64::try_from(body.len()).unwrap_or(i64::MAX);
    if length > body_limit {
        // `PhutilUTF8StringTruncator`, not ported.
        return Err(Error::unsupported(
            "mail truncated at metamta.email-body-limit",
        ));
    }
    body_limit -= length;
    if raw_body.as_ref().and_then(Value::as_str) != Some(body.as_str()) {
        mail.set_param("body.sent", Value::String(body.clone()));
    }

    let send_html =
        !must_encrypt && php::loose_eq_str(&preferences.value("html-emails", config), "html");
    let mut html_body = None;
    if send_html
        && let Some(html) =
            php::nonempty_string(mail.param("html-body")).map_err(|_| bad("an HTML body"))?
        && i64::try_from(html.len()).unwrap_or(i64::MAX) <= body_limit
    {
        html_body = Some(html.to_owned());
    }

    // `flattenHeaders()` iterates the wrong list: both are always empty.
    mail.set_param("headers.unfiltered", Value::Array(Vec::new()));
    mail.set_param("headers.sent", Value::Array(Vec::new()));

    if config.truthy("phabricator.silent") {
        mail.message = Some(SILENT.to_owned());
        return Ok(Composed::Nothing);
    }

    Ok(Composed::Message(Box::new(Composer {
        from,
        reply_to,
        to,
        cc,
        subject,
        headers,
        thread,
        stamps,
        must_encrypt,
        text_body: body,
        html_body,
    })))
}

/// `newFromEmailAddress()`.
fn from_address(
    mail: &Mail,
    from_actor: Option<&Actor>,
    config: &Config,
    must_encrypt: bool,
) -> Result<EmailAddress, Error> {
    let mut from = default_address(config)?;
    if !must_encrypt {
        if let Some(raw_from) = mail
            .param("raw-from")
            .filter(|value| phab_core::php::is_truthy(value))
        {
            let parts = php::values(raw_from).ok_or_else(|| bad("a raw sender"))?;
            let (Some(Value::String(email)), Some(name)) = (parts.first(), parts.get(1)) else {
                return Err(bad("a raw sender"));
            };
            let name = php::string_cast(Some(name)).ok_or_else(|| bad("a raw sender"))?;
            from = EmailAddress::default();
            from.set_address(email);
            if !name.is_empty() {
                from.display_name = Some(name);
            }
        } else if let Some(actor) = from_actor {
            if config.truthy("metamta.can-send-as-user")
                && let Some(email) = &actor.email
            {
                from.set_address(email);
            }
            if let Some(name) = &actor.name {
                from.display_name = Some(name.clone());
            }
        }
    }
    Ok(from)
}

/// `newReplyToEmailAddress()`.
fn reply_to_address(
    mail: &Mail,
    from_actor: Option<&Actor>,
) -> Result<Option<EmailAddress>, Error> {
    let reply_to =
        match php::nonempty_string(mail.param("reply-to")).map_err(|_| bad("a reply-to"))? {
            None => None,
            Some(raw) => {
                let mut address = EmailAddress::parse(raw);
                if let Some(actor) = from_actor {
                    address.display_name.clone_from(&actor.name);
                }
                if address.display_name().is_empty() {
                    address.display_name = Some(SERVER_NAME.to_owned());
                }
                Some(address)
            }
        };
    Ok(reply_to)
}

/// The threading of the mail (`newEmailThreadingHeaders()`).
fn thread(mail: &Mail, config: &Config) -> Result<Option<Thread>, Error> {
    let thread = match php::nonempty_string(mail.param("thread-id")).map_err(|_| bad("a thread"))? {
        None => None,
        Some(id) => {
            let parent = match mail.param("parent-message-id") {
                Some(value) if phab_core::php::is_truthy(value) => Some(
                    value
                        .as_str()
                        .ok_or_else(|| bad("a parent message"))?
                        .to_owned(),
                ),
                _ => None,
            };
            Some(Thread {
                id: format!("<{id}@{}>", mail_domain(config)?),
                is_first: mail.flag("is-first-message"),
                parent,
            })
        }
    };
    Ok(thread)
}

/// `X-Phabricator-Stamps`.
fn stamps(mail: &Mail) -> Result<Option<String>, Error> {
    let stamps = match mail.param("stamps") {
        Some(stamps) if phab_core::php::is_truthy(stamps) => {
            let values = php::values(stamps).ok_or_else(|| bad("stamps"))?;
            let parts: Option<Vec<String>> = values
                .into_iter()
                .map(|value| php::string_cast(Some(value)))
                .collect();
            Some(parts.ok_or_else(|| bad("stamps"))?.join(" "))
        }
        _ => None,
    };
    Ok(stamps)
}

/// The text body, the notice of a mail which must be encrypted.
fn body(
    mail: &Mail,
    raw_body: Option<&Value>,
    config: &Config,
    must_encrypt: bool,
) -> Result<String, Error> {
    let body = if must_encrypt {
        let mut parts = Vec::new();
        let mut encrypt_uri = php::string_cast(mail.param("mustEncrypt.uri"))
            .ok_or_else(|| bad("an encryption URI"))?;
        if encrypt_uri.is_empty()
            && let Some(related) = mail
                .related_phid
                .as_deref()
                .filter(|phid| php_truthy_str(phid))
        {
            encrypt_uri = format!("/object/{related}/");
        }
        if !encrypt_uri.is_empty() {
            parts.push(
                "This secure message is notifying you of a change to this object:".to_owned(),
            );
            parts.push(production_uri(config, &encrypt_uri)?);
        }
        parts.push(
            "The content for this message can only be transmitted over a secure channel. To \
             view the message content, follow this link:"
                .to_owned(),
        );
        parts.push(production_uri(config, &mail.uri())?);
        parts.join("\n\n")
    } else {
        php::string_cast(raw_body).ok_or_else(|| bad("a body"))?
    };
    Ok(body)
}

fn php_truthy_str(text: &str) -> bool {
    phab_core::php::is_truthy_str(text)
}

/// `newEmailSubject()`.
fn subject(
    mail: &Mail,
    preferences: &Preferences,
    config: &Config,
    must_encrypt: bool,
) -> Result<String, Error> {
    let is_threaded = mail.flag("thread-id");
    let mut parts: Vec<String> = Vec::new();
    if is_threaded && php::loose_eq_str(&preferences.value("re-prefix", config), "re") {
        parts.push("Re:".to_owned());
    }
    let prefix =
        php::string_cast(mail.param("subject-prefix")).ok_or_else(|| bad("a subject prefix"))?;
    parts.push(php::trim(&prefix).to_owned());
    if must_encrypt {
        let encrypted =
            php::string_cast(mail.param("mustEncrypt.subject")).ok_or_else(|| bad("a subject"))?;
        parts.push(if encrypted.is_empty() {
            "Object Updated".to_owned()
        } else {
            encrypted
        });
    } else {
        let vary = php::nonempty_string(mail.param("vary-subject-prefix"))
            .map_err(|_| bad("a subject prefix"))?;
        if let Some(vary) = vary
            && php::loose_eq_str(&preferences.value("vary-subject", config), "vary")
        {
            parts.push(vary.to_owned());
        }
        if let Some(subject) =
            php::nonempty_string(mail.param("subject")).map_err(|_| bad("a subject"))?
        {
            parts.push(subject.to_owned());
        }
    }
    parts.retain(|part| !part.is_empty());
    Ok(parts.join(" "))
}

/// `newEmailHeaders()` without `X-Phabricator-Send-Attempt`.
fn headers(mail: &Mail) -> Result<Vec<(String, String)>, Error> {
    let mut headers = vec![
        (
            "X-Phabricator-Sent-This-Message".to_owned(),
            "Yes".to_owned(),
        ),
        ("X-Mail-Transport-Agent".to_owned(), "MetaMTA".to_owned()),
        ("X-Auto-Response-Suppress".to_owned(), "All".to_owned()),
    ];
    if let Some(tags) = mail
        .param("mailtags")
        .filter(|tags| phab_core::php::is_truthy(tags))
    {
        let tags = php::strings(tags).ok_or_else(|| bad("mail tags"))?;
        let value: Vec<String> = tags.iter().map(|tag| format!("<{tag}>")).collect();
        headers.push(("X-Phabricator-Mail-Tags".to_owned(), value.join(", ")));
    }
    if let Some(pairs) = mail.param("headers") {
        for pair in php::values(pairs).ok_or_else(|| bad("headers"))? {
            let parts = php::values(pair).ok_or_else(|| bad("headers"))?;
            let (Some(Value::String(name)), Some(value)) = (parts.first(), parts.get(1)) else {
                return Err(bad("headers"));
            };
            // NOTE of PHP: SES rejects headers with newlines.
            let value = php::string_cast(Some(value)).ok_or_else(|| bad("headers"))?;
            headers.push((name.clone(), value.replace('\n', " ")));
        }
    }
    if mail.flag("is-bulk") {
        headers.push(("Precedence".to_owned(), "bulk".to_owned()));
    }
    if mail.flag("mustEncrypt") {
        headers.push(("X-Phabricator-Must-Encrypt".to_owned(), "Yes".to_owned()));
    }
    if let Some(related) = mail
        .related_phid
        .as_deref()
        .filter(|phid| php_truthy_str(phid))
    {
        headers.push(("Thread-Topic".to_owned(), related.to_owned()));
    }
    headers.push(("X-Phabricator-Mail-ID".to_owned(), mail.id.to_string()));
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_threads_like_outlook() {
        // base64_encode(substr(md5('<t@x>'), 0, 27)).
        let first = thread_index("<t@x>", true, 0);
        let digest = hex::encode(Md5::digest(b"<t@x>"));
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&first)
                .unwrap(),
            digest.as_bytes()[..27]
        );
        let reply = thread_index("<t@x>", false, 0x0102_0304);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(reply)
            .unwrap();
        assert_eq!(&decoded[27..], b" \x01\x02\x03\x04");
    }

    #[test]
    fn reads_uri_domains() {
        assert_eq!(uri_domain("http://phab.example.com/"), "phab.example.com");
        assert_eq!(uri_domain("https://u@host:8080/x"), "host");
        assert_eq!(uri_domain("/D1"), "");
    }
}
