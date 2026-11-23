//! The SMTP dialog of `PhabricatorMailSMTPAdapter` (`PHPMailer::SmtpSend()`
//! and `class.smtp.php`), over lettre's connection: the same commands, the
//! same reply codes checked, the same error messages.

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use lettre::transport::smtp::client::{AsyncSmtpConnection, TlsParameters};
use lettre::transport::smtp::extension::ClientId;

use crate::mailer::SmtpOptions;
use crate::phpmailer::{self, Rendered};

/// `PHPMailer::$Timeout`.
const TIMEOUT: Duration = Duration::from_secs(60);
/// `ServerHostname()` outside a web request.
const HELLO_NAME: &str = "localhost.localdomain";

const CONNECT_FAILED: &str = "SMTP Error: Could not connect to SMTP host.";
// `Lang('tls')` has no translation.
const TLS_FAILED: &str = "Language string failed to load: tls";
const AUTH_FAILED: &str = "SMTP Error: Could not authenticate.";
const DATA_FAILED: &str = "SMTP Error: Data not accepted.";

/// A command line, CRLF-terminated.
struct Line(String);

impl fmt::Display for Line {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}\r\n", self.0)
    }
}

/// Sends `command` and whether the reply has one of `codes`.
async fn expect(connection: &mut AsyncSmtpConnection, command: String, codes: &[&str]) -> bool {
    match connection.command(Line(command)).await {
        Ok(response) => codes.contains(&response.code().to_string().as_str()),
        Err(_) => false,
    }
}

/// The host and port `PHPMailer` connects to: the first of `;`-separated
/// hosts (it gives up after it), `host:port` overriding the port.
fn address(options: &SmtpOptions) -> (String, u16) {
    let hosts = options.host.as_deref().unwrap_or_default();
    let host = hosts.split(';').next().unwrap_or_default();
    let port = u16::try_from(options.port).unwrap_or(0);
    if let Some((name, number)) = host.rsplit_once(':')
        && !name.is_empty()
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return (name.to_owned(), number.parse().unwrap_or(0));
    }
    (host.to_owned(), port)
}

/// Sends a rendered message; `Err` is the message of the exception PHP
/// throws.
pub async fn send(options: &SmtpOptions, rendered: &Rendered) -> Result<(), String> {
    let (host, port) = address(options);
    let protocol = options.protocol.as_deref().map(str::to_lowercase);
    let hello = ClientId::Domain(HELLO_NAME.to_owned());
    let tls = || TlsParameters::new(host.clone()).map_err(|_| CONNECT_FAILED.to_owned());
    let implicit_tls = match protocol.as_deref() {
        Some("ssl") => Some(tls()?),
        _ => None,
    };
    let mut connection = AsyncSmtpConnection::connect_tokio1(
        (host.as_str(), port),
        Some(TIMEOUT),
        &hello,
        implicit_tls,
        None,
    )
    .await
    .map_err(|_| CONNECT_FAILED.to_owned())?;
    if protocol.as_deref() == Some("tls") {
        connection
            .starttls(tls().map_err(|_| TLS_FAILED.to_owned())?, &hello)
            .await
            .map_err(|_| TLS_FAILED.to_owned())?;
    }
    if let Some(user) = options.user.as_deref().filter(|user| !user.is_empty()) {
        let encode = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
        let password = options.password.as_deref().unwrap_or_default();
        let authenticated = expect(&mut connection, "AUTH LOGIN".to_owned(), &["334"]).await
            && expect(&mut connection, encode(user), &["334"]).await
            && expect(&mut connection, encode(password), &["235"]).await;
        if !authenticated {
            return Err(AUTH_FAILED.to_owned());
        }
    }
    if !expect(
        &mut connection,
        format!("MAIL FROM:<{}>", rendered.sender),
        &["250"],
    )
    .await
    {
        return Err(format!(
            "The following From address failed: {}",
            rendered.sender
        ));
    }
    let mut rejected = Vec::new();
    for recipient in &rendered.recipients {
        if !expect(
            &mut connection,
            format!("RCPT TO:<{recipient}>"),
            &["250", "251"],
        )
        .await
        {
            rejected.push(recipient.as_str());
        }
    }
    if !rejected.is_empty() {
        return Err(format!(
            "SMTP Error: The following recipients failed: {}",
            rejected.join(", ")
        ));
    }
    if !expect(&mut connection, "DATA".to_owned(), &["354"]).await {
        return Err(DATA_FAILED.to_owned());
    }
    // lettre dot-stuffs the lines and ends the data with ".\r\n", as
    // `Data()` does after its lines and an empty one.
    let accepted = match connection
        .message(&phpmailer::data_lines(&rendered.data))
        .await
    {
        Ok(response) => response.code().to_string() == "250",
        Err(_) => false,
    };
    if !accepted {
        return Err(DATA_FAILED.to_owned());
    }
    // PHPMailer leaves without QUIT (`SMTPKeepAlive` is off and nothing
    // closes the session): the connection is dropped.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connects_to_the_first_host() {
        let options = |host: &str| SmtpOptions {
            host: Some(host.to_owned()),
            port: 25,
            ..SmtpOptions::default()
        };
        assert_eq!(
            address(&options("mx.example.com")),
            ("mx.example.com".to_owned(), 25)
        );
        assert_eq!(
            address(&options("a.example.com:2525;b.example.com")),
            ("a.example.com".to_owned(), 2525)
        );
    }
}
