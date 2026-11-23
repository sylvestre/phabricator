//! The bytes `PhabricatorMailSMTPAdapter` sends: the message as
//! `externals/phpmailer/class.phpmailer.php` (`PHPMailer` 5.1, `CharSet`
//! `utf-8`, `Encoding` `base64`, `LE` `"\n"`) builds it, and the DATA lines
//! of `class.smtp.php`.

use std::fmt::Write as _;

use base64::Engine as _;

use crate::engine::EmailMessage;
use crate::php;

/// `CharSet`.
const CHARSET: &str = "utf-8";
/// The longest line `class.smtp.php` sends.
const MAX_LINE_LENGTH: usize = 998;

/// Where `EncodeHeader()` puts the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    /// A display name.
    Phrase,
    /// A header value.
    Text,
}

/// A message ready for SMTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// `MAIL FROM`.
    pub sender: String,
    /// `RCPT TO`, "To" then "Cc".
    pub recipients: Vec<String>,
    /// The header and body `PHPMailer` passes to `Data()`, lines ending in
    /// `"\n"`.
    pub data: Vec<u8>,
}

fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// `chunk_split($text, $length, "\n")`: a newline after every chunk,
/// including the last.
fn chunk_split(text: &str, length: usize) -> String {
    let mut output = String::with_capacity(text.len() + text.len() / length + 1);
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        output.push('\n');
        return output;
    }
    for chunk in bytes.chunks(length) {
        // Base64 is ASCII.
        output.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        output.push('\n');
    }
    output
}

/// `SecureHeader()`: line breaks removed, then trimmed.
fn secure_header(text: &str) -> String {
    php::trim(&text.replace(['\r', '\n'], "")).to_owned()
}

/// `trim(preg_replace('/[\r\n]+/', '', $name))`, how addresses take names.
fn clean_name(name: &str) -> String {
    php::trim(&name.replace(['\r', '\n'], "")).to_owned()
}

/// `addcslashes($text, "\0..\37\177\\\"")`.
fn addcslashes(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\x07' => output.push_str("\\a"),
            '\x08' => output.push_str("\\b"),
            '\t' => output.push_str("\\t"),
            '\n' => output.push_str("\\n"),
            '\x0b' => output.push_str("\\v"),
            '\x0c' => output.push_str("\\f"),
            '\r' => output.push_str("\\r"),
            '\0'..='\x1f' | '\x7f' => {
                let _ = write!(output, "\\{:03o}", u32::from(character));
            }
            '\\' | '"' => {
                output.push('\\');
                output.push(character);
            }
            character => output.push(character),
        }
    }
    output
}

/// `EncodeQ()`: bytes outside the safe set become `=XX`, spaces `_`.
fn encode_q(text: &str, position: Position) -> String {
    let mut output = String::with_capacity(text.len());
    for byte in text.bytes().filter(|byte| !matches!(byte, b'\r' | b'\n')) {
        let escape = match position {
            Position::Phrase => !(byte.is_ascii_alphanumeric() || b"!*+/ -".contains(&byte)),
            Position::Text => {
                matches!(byte, 0..=9 | 11 | 12 | 14..=31 | b'=' | b'?' | b'_' | 127..)
            }
        };
        if escape {
            let _ = write!(output, "={byte:02X}");
        } else if byte == b' ' {
            output.push('_');
        } else {
            output.push(char::from(byte));
        }
    }
    output
}

/// `hexdec()`: the hexadecimal digits of the text, others ignored.
fn hexdec(text: &[u8]) -> u32 {
    text.iter()
        .filter_map(|byte| char::from(*byte).to_digit(16))
        .fold(0, |value, digit| value * 16 + digit)
}

/// `substr($text, $start, $length)` for a start within or past the text.
fn substr(text: &[u8], start: usize, length: usize) -> &[u8] {
    let start = start.min(text.len());
    let end = start.saturating_add(length).min(text.len());
    &text[start..end]
}

/// `UTF8CharBoundary()`: where to cut Q-encoded text before `max_length`
/// without splitting an encoded character.
fn utf8_char_boundary(encoded: &[u8], mut max_length: usize) -> usize {
    let mut look_back = 3;
    loop {
        if look_back > max_length {
            return max_length;
        }
        let last_chunk = substr(encoded, max_length - look_back, look_back);
        let Some(position) = last_chunk.iter().position(|byte| *byte == b'=') else {
            return max_length;
        };
        let hex = substr(encoded, max_length - look_back + position + 1, 2);
        let value = hexdec(hex);
        if value < 128 {
            if position != 0 {
                max_length -= look_back - position;
            }
            return max_length;
        } else if value >= 192 {
            return max_length - (look_back - position);
        }
        look_back += 3;
    }
}

/// `WrapText($encoded, $length, true)` of Q-encoded text, which has no
/// spaces or line breaks: chunks joined with `"=\n"`, then a `"\n"`.
fn wrap_q(encoded: &str, length: usize) -> String {
    let mut word = encoded.as_bytes();
    let mut message = Vec::new();
    let mut buffer: &[u8] = word;
    if word.len() > length {
        while !word.is_empty() {
            let cut = utf8_char_boundary(word, length).min(word.len());
            let (part, rest) = word.split_at(cut);
            word = rest;
            if word.is_empty() {
                buffer = part;
            } else {
                message.extend_from_slice(part);
                message.extend_from_slice(b"=\n");
            }
        }
    }
    message.extend_from_slice(buffer);
    message.push(b'\n');
    // Q-encoded text is ASCII.
    String::from_utf8(message).unwrap_or_default()
}

/// `Base64EncodeWrapMB()`: base64 lines of whole characters.
fn base64_wrap_mb(text: &str) -> String {
    let characters: Vec<char> = text.chars().collect();
    // 75 bytes, less `=?utf-8?B?` and `?=`.
    let length = 63;
    #[expect(clippy::cast_precision_loss, reason = "lengths of header values")]
    let ratio = characters.len() as f64 / text.len() as f64;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "floor() of a small positive number"
    )]
    let average = (63.0 * ratio * 0.75).floor() as usize;
    let mut lines = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let mut look_back = 0;
        let (chunk, offset) = loop {
            let offset = average.saturating_sub(look_back);
            let end = (index + offset).min(characters.len());
            let chunk: String = characters[index..end].iter().collect();
            let chunk = base64(chunk.as_bytes());
            look_back += 1;
            if chunk.len() <= length || offset == 0 {
                break (chunk, offset);
            }
        };
        lines.push(chunk);
        if offset == 0 {
            break;
        }
        index += offset;
    }
    lines.join("\n")
}

/// `EncodeHeader()`.
fn encode_header(text: &str, position: Position) -> String {
    let bytes = text.as_bytes();
    let count = match position {
        Position::Phrase => {
            if !bytes.iter().any(|byte| *byte >= 0x80) {
                let encoded = addcslashes(text);
                let plain = bytes.iter().all(|byte| {
                    byte.is_ascii_alphanumeric() || b"!#$%&'*+/=?^_`{|}~ -".contains(byte)
                });
                return if encoded == text && plain {
                    encoded
                } else {
                    format!("\"{encoded}\"")
                };
            }
            bytes
                .iter()
                .filter(|byte| !matches!(byte, 0x20 | 0x21 | 0x23..=0x5b | 0x5d..=0x7e))
                .count()
        }
        Position::Text => bytes
            .iter()
            .filter(|byte| matches!(byte, 0..=8 | 11 | 12 | 14..=31 | 127..))
            .count(),
    };
    if count == 0 {
        return text.to_owned();
    }
    let max_length = 75 - 7 - CHARSET.len();
    #[expect(clippy::cast_precision_loss, reason = "lengths of header values")]
    let use_b = (bytes.len() as f64) / 3.0 < count as f64;
    let (encoding, encoded) = if use_b {
        let encoded = if text.len() > text.chars().count() {
            base64_wrap_mb(text)
        } else {
            let length = max_length - max_length % 4;
            php::trim(&chunk_split(&base64(bytes), length)).to_owned()
        };
        ('B', encoded)
    } else {
        let wrapped = wrap_q(&encode_q(text, position), max_length);
        ('Q', php::trim(&wrapped).replace("=\n", "\n"))
    };
    let lines: Vec<String> = encoded
        .split('\n')
        .map(|line| format!(" =?{CHARSET}?{encoding}?{line}?="))
        .collect();
    php::trim(&lines.join("\n")).to_owned()
}

/// `AddrFormat()`.
fn address_format(address: &str, name: &str) -> String {
    if php::is_empty_str(name) {
        secure_header(address)
    } else {
        format!(
            "{} <{}>",
            encode_header(&secure_header(name), Position::Phrase),
            secure_header(address)
        )
    }
}

/// `AddrAppend()`.
fn address_append(kind: &str, addresses: &[(String, String)]) -> String {
    let formatted: Vec<String> = addresses
        .iter()
        .map(|(address, name)| address_format(address, name))
        .collect();
    format!("{kind}: {}\n", formatted.join(", "))
}

/// Whether `filter_var($address, FILTER_VALIDATE_EMAIL)` certainly accepts
/// the address: a dot-atom local part and a host name with a top-level
/// domain starting with a letter. Other addresses (quoted local parts,
/// address literals, ...) are not ported.
pub fn is_valid_address(address: &str) -> bool {
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    let atext = |byte: u8| byte.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&byte);
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && local
            .split('.')
            .all(|atom| !atom.is_empty() && atom.bytes().all(atext));
    let labels: Vec<&str> = domain.split('.').collect();
    let domain_ok = labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() < 64
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
        && labels
            .last()
            .is_some_and(|tld| tld.as_bytes()[0].is_ascii_alphabetic());
    local_ok && domain_ok && address.len() < 255
}

/// `PHPMailer::RFCDate()` for the time and offset (in seconds) of the
/// default time zone.
pub fn rfc_date(local: &str, offset_seconds: i32) -> String {
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let offset = offset_seconds.abs();
    let offset = (offset / 3600) * 100 + (offset % 3600) / 60;
    format!("{local} {sign}{offset:04}")
}

/// What the SMTP adapter hands `PHPMailer`, as `PHPMailer` keeps it.
struct Envelope {
    from: String,
    from_name: String,
    reply_to: Vec<(String, String)>,
    to: Vec<(String, String)>,
    cc: Vec<(String, String)>,
    message_id: String,
    custom: Vec<(String, String)>,
}

/// `SetFrom($address, $name, false)`, `AddReplyTo()`, `AddAddress()`,
/// `AddCC()` and the headers, `Err` for an address `PHPMailer` rejects.
fn envelope(message: &EmailMessage) -> Result<Envelope, String> {
    let invalid = |address: &str| format!("Invalid address: {address}");

    let from = php::trim(&message.from.address()).to_owned();
    if !is_valid_address(&from) {
        return Err(invalid(&from));
    }
    let from_name = clean_name(message.from.display_name());

    let mut reply_to: Vec<(String, String)> = Vec::new();
    if let Some(address) = &message.reply_to {
        let raw = php::trim(&address.address()).to_owned();
        if !is_valid_address(&raw) {
            return Err(invalid(&raw));
        }
        reply_to.push((raw, clean_name(address.display_name())));
    }

    // `AddAnAddress()`: a recipient already added, in any case, is skipped.
    let mut seen: Vec<String> = Vec::new();
    let mut add = |list: &mut Vec<(String, String)>, address: &crate::address::EmailAddress| {
        let raw = php::trim(&address.address()).to_owned();
        if !is_valid_address(&raw) {
            return Err(invalid(&raw));
        }
        let lower = raw.to_lowercase();
        if !seen.contains(&lower) {
            seen.push(lower);
            list.push((raw, clean_name(address.display_name())));
        }
        Ok(())
    };
    let mut to = Vec::new();
    for address in &message.to {
        add(&mut to, address)?;
    }
    let mut cc = Vec::new();
    for address in &message.cc {
        add(&mut cc, address)?;
    }

    let mut message_id = String::new();
    let mut custom: Vec<(String, String)> = Vec::new();
    for (name, value) in &message.headers {
        if name.to_lowercase() == "message-id" {
            message_id.clone_from(value);
        } else {
            // `AddCustomHeader("{$name}: {$value}")` splits at the first
            // colon.
            let line = format!("{name}: {value}");
            let (name, value) = line.split_once(':').unwrap_or((&line, ""));
            custom.push((php::trim(name).to_owned(), php::trim(value).to_owned()));
        }
    }
    Ok(Envelope {
        from,
        from_name,
        reply_to,
        to,
        cc,
        message_id,
        custom,
    })
}

/// `CreateHeader()` up to `MIME-Version`.
fn create_header(envelope: &Envelope, subject: &str, date: &str) -> String {
    let mut header = String::new();
    let _ = writeln!(header, "Date: {date}");
    let _ = writeln!(header, "Return-Path: {}", envelope.from);
    if !envelope.to.is_empty() {
        header.push_str(&address_append("To", &envelope.to));
    } else if envelope.cc.is_empty() {
        header.push_str("To: undisclosed-recipients:;\n");
    }
    header.push_str(&address_append(
        "From",
        &[(envelope.from.clone(), envelope.from_name.clone())],
    ));
    if !envelope.cc.is_empty() {
        header.push_str(&address_append("Cc", &envelope.cc));
    }
    if !envelope.reply_to.is_empty() {
        header.push_str(&address_append("Reply-to", &envelope.reply_to));
    }
    let _ = writeln!(
        header,
        "Subject: {}",
        encode_header(&secure_header(subject), Position::Text)
    );
    if !envelope.message_id.is_empty() {
        let _ = writeln!(header, "Message-ID: {}", envelope.message_id);
    }
    header.push_str("X-Priority: 3\n");
    header.push_str("X-Mailer: PHPMailer 5.1 (phpmailer.sourceforge.net)\n");
    for (name, value) in &envelope.custom {
        let _ = writeln!(header, "{name}: {}", encode_header(value, Position::Text));
    }
    header.push_str("MIME-Version: 1.0\n");
    header
}

/// `GetMailMIME()` and `CreateBody()` for a plain or an alternative
/// message: the end of the header and the body.
fn create_body(body: &str, alt_body: &str, content_type: &str, boundary: &str) -> (String, String) {
    let mut header = String::new();
    let mut text = String::new();
    if alt_body.is_empty() {
        header.push_str("Content-Transfer-Encoding: base64\n");
        let _ = write!(
            header,
            "Content-Type: {content_type}; charset=\"{CHARSET}\""
        );
        text.push_str(&chunk_split(&base64(body.as_bytes()), 76));
    } else {
        header.push_str("Content-Type: multipart/alternative;\n");
        let _ = writeln!(header, "\tboundary=\"{boundary}\"");
        let part = |content_type: &str| {
            format!(
                "--{boundary}\nContent-Type: {content_type}; charset = \"{CHARSET}\"\n\
                 Content-Transfer-Encoding: base64\n\n"
            )
        };
        text.push_str(&part("text/plain"));
        text.push_str(&chunk_split(&base64(alt_body.as_bytes()), 76));
        text.push_str("\n\n");
        text.push_str(&part("text/html"));
        text.push_str(&chunk_split(&base64(body.as_bytes()), 76));
        text.push_str("\n\n");
        let _ = writeln!(text, "\n--{boundary}--");
    }
    header.push_str("\n\n");
    (header, text)
}

/// The message `PHPMailer` builds for the SMTP adapter's calls, `date` its
/// `Date` header and `unique` the `md5(uniqid())` of its boundaries. `Err`
/// is the message of the `phpmailerException` it throws.
pub fn render(message: &EmailMessage, date: &str, unique: &str) -> Result<Rendered, String> {
    let envelope = envelope(message)?;
    let (body, alt_body, content_type) = match &message.html_body {
        Some(html) => (html.as_str(), message.text_body.as_str(), "text/html"),
        None => (message.text_body.as_str(), "", "text/plain"),
    };

    // `Send()`.
    if envelope.to.len() + envelope.cc.len() < 1 {
        return Err("You must provide at least one recipient email address.".to_owned());
    }
    let mut header = create_header(&envelope, &message.subject, date);
    let (mime, text) = create_body(body, alt_body, content_type, &format!("b1_{unique}"));
    header.push_str(&mime);
    if php::is_empty_str(body) {
        return Err("Message body empty".to_owned());
    }

    let mut bytes = header.into_bytes();
    bytes.extend_from_slice(text.as_bytes());
    Ok(Rendered {
        sender: envelope.from,
        recipients: envelope
            .to
            .into_iter()
            .chain(envelope.cc)
            .map(|(address, _)| address)
            .collect(),
        data: bytes,
    })
}

/// The lines `SMTP::Data()` writes, each ending in CRLF, then the CRLF
/// before the final dot: line breaks normalized, lines longer than 998
/// bytes split (header continuations indented with a tab). Dot-stuffing is
/// left to the transport.
pub fn data_lines(data: &[u8]) -> Vec<u8> {
    let mut text = Vec::with_capacity(data.len());
    let mut index = 0;
    while index < data.len() {
        match data[index] {
            b'\r' if data.get(index + 1) == Some(&b'\n') => {
                text.push(b'\n');
                index += 2;
                continue;
            }
            b'\r' => text.push(b'\n'),
            byte => text.push(byte),
        }
        index += 1;
    }
    let lines: Vec<&[u8]> = text.split(|byte| *byte == b'\n').collect();
    // A header block starts with a field name without spaces.
    let first = lines.first().copied().unwrap_or_default();
    let mut in_headers = first
        .iter()
        .position(|byte| *byte == b':')
        .map(|colon| &first[..colon])
        .is_some_and(|field| {
            !php::is_empty_str(&String::from_utf8_lossy(field)) && !field.contains(&b' ')
        });
    let mut output = Vec::with_capacity(text.len() + lines.len() + 2);
    for line in lines {
        if line.is_empty() && in_headers {
            in_headers = false;
        }
        let mut line = line.to_vec();
        let mut out: Vec<Vec<u8>> = Vec::new();
        while line.len() > MAX_LINE_LENGTH {
            let head = &line[..MAX_LINE_LENGTH];
            let rest = match head.iter().rposition(|byte| *byte == b' ') {
                None | Some(0) => {
                    let cut = MAX_LINE_LENGTH - 1;
                    out.push(line[..cut].to_vec());
                    line[cut..].to_vec()
                }
                Some(space) => {
                    out.push(line[..space].to_vec());
                    line[space + 1..].to_vec()
                }
            };
            line = if in_headers {
                let mut indented = vec![b'\t'];
                indented.extend_from_slice(&rest);
                indented
            } else {
                rest
            };
        }
        out.push(line);
        for line in out {
            output.extend_from_slice(&line);
            output.extend_from_slice(b"\r\n");
        }
    }
    output.extend_from_slice(b"\r\n");
    output
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::address::EmailAddress;

    #[rstest]
    #[case("Plain subject", Position::Text, "Plain subject")]
    #[case("alice", Position::Phrase, "alice")]
    #[case("alice (Alice Liddell)", Position::Phrase, "\"alice (Alice Liddell)\"")]
    #[case("say \"hi\"", Position::Phrase, "\"say \\\"hi\\\"\"")]
    #[case("Caf\u{e9} au lait", Position::Text, "=?utf-8?Q?Caf=C3=A9_au_lait?=")]
    #[case("\u{65e5}\u{672c}\u{8a9e}", Position::Text, "=?utf-8?B?5pel5pys6Kqe?=")]
    fn encodes_headers_like_phpmailer(
        #[case] text: &str,
        #[case] position: Position,
        #[case] expected: &str,
    ) {
        assert_eq!(encode_header(text, position), expected);
    }

    /// Outputs of PHP's `EncodeHeader()` (`CharSet` `utf-8`).
    #[rstest]
    #[case(
        "word word word word word word word word word word word word word word word word word \
         word word word  \u{e9}",
        Position::Text,
        "=?utf-8?Q?word_word_word_word_word_word_word_word_word_word_word_word_wor?=\n \
         =?utf-8?Q?d_word_word_word_word_word_word_word__=C3=A9?="
    )]
    #[case(
        &("\u{e9}".repeat(30) + "x"),
        Position::Text,
        "=?utf-8?B?w6nDqcOpw6nDqcOpw6nDqcOpw6nDqcOpw6nDqcOpw6nDqcOpw6nDqcOpw6k=?=\n \
         =?utf-8?B?w6nDqcOpw6nDqcOpw6nDqXg=?="
    )]
    #[case(
        "D12: Bug 123 - Fix the \u{201c}thing\u{201d} in the long subject line that goes on and \
         on and on",
        Position::Text,
        "=?utf-8?Q?D12:_Bug_123_-_Fix_the_=E2=80=9Cthing=E2=80=9D_in_the_long_subj?=\n \
         =?utf-8?Q?ect_line_that_goes_on_and_on_and_on?="
    )]
    #[case(
        "Ren\u{e9}e (Ren\u{e9}e Fleming)",
        Position::Phrase,
        "=?utf-8?Q?Ren=C3=A9e_=28Ren=C3=A9e_Fleming=29?="
    )]
    #[case(
        &"\u{65e5}".repeat(40),
        Position::Text,
        "=?utf-8?B?5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel?=\n \
         =?utf-8?B?5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel?=\n \
         =?utf-8?B?5pel5pel5pel5pel5pel5pel5pel5pel5pel5pel?="
    )]
    #[case("tab\there", Position::Phrase, "\"tab\\there\"")]
    fn encodes_like_php(#[case] text: &str, #[case] position: Position, #[case] expected: &str) {
        assert_eq!(encode_header(text, position), expected);
    }

    #[test]
    fn wraps_long_q_encoded_headers_on_character_boundaries() {
        // 30 two-byte characters: 180 bytes of Q encoding, cut before each
        // encoded character that would cross 63 bytes.
        let text = "\u{e9}".repeat(30) + "x";
        let encoded = encode_header(&text, Position::Text);
        // More than a third of the bytes are 8-bit: base64.
        assert!(encoded.starts_with("=?utf-8?B?"));
        let mostly_ascii = format!("{} \u{e9}", "word ".repeat(20));
        let encoded = encode_header(&mostly_ascii, Position::Text);
        let lines: Vec<&str> = encoded.split('\n').collect();
        assert!(lines.len() > 1);
        for line in &lines {
            let line = line.trim_start();
            assert!(line.starts_with("=?utf-8?Q?") && line.ends_with("?="));
            assert!(line.len() <= 63 + "=?utf-8?Q??=".len());
        }
    }

    fn message(html: Option<&str>) -> EmailMessage {
        let mut from = EmailAddress::parse("noreply@phab.example.com");
        from.display_name = Some("alice (Alice Liddell)".to_owned());
        EmailMessage {
            from,
            reply_to: None,
            to: vec![
                EmailAddress::parse("bob@example.com"),
                EmailAddress::parse("BOB@example.com"),
            ],
            cc: vec![EmailAddress::parse("carol@example.com")],
            subject: "[Differential] D1: Fix".to_owned(),
            headers: vec![
                ("X-Phabricator-Mail-ID".to_owned(), "7".to_owned()),
                ("Message-ID".to_owned(), "<t@phab.example.com>".to_owned()),
            ],
            text_body: "Hello\n".to_owned(),
            html_body: html.map(str::to_owned),
        }
    }

    #[test]
    fn renders_plain_mail() {
        let rendered = render(&message(None), "Mon, 2 Jan 2023 03:04:05 +0000", "u").unwrap();
        assert_eq!(rendered.sender, "noreply@phab.example.com");
        assert_eq!(
            rendered.recipients,
            ["bob@example.com", "carol@example.com"]
        );
        assert_eq!(
            String::from_utf8(rendered.data).unwrap(),
            "Date: Mon, 2 Jan 2023 03:04:05 +0000\n\
             Return-Path: noreply@phab.example.com\n\
             To: bob@example.com\n\
             From: \"alice (Alice Liddell)\" <noreply@phab.example.com>\n\
             Cc: carol@example.com\n\
             Subject: [Differential] D1: Fix\n\
             Message-ID: <t@phab.example.com>\n\
             X-Priority: 3\n\
             X-Mailer: PHPMailer 5.1 (phpmailer.sourceforge.net)\n\
             X-Phabricator-Mail-ID: 7\n\
             MIME-Version: 1.0\n\
             Content-Transfer-Encoding: base64\n\
             Content-Type: text/plain; charset=\"utf-8\"\n\
             \n\
             SGVsbG8K\n"
        );
    }

    #[test]
    fn renders_alternative_mail() {
        let rendered = render(&message(Some("<b>Hi</b>")), "D", "u").unwrap();
        let text = String::from_utf8(rendered.data).unwrap();
        assert!(text.contains(
            "MIME-Version: 1.0\n\
             Content-Type: multipart/alternative;\n\
             \tboundary=\"b1_u\"\n\
             \n\
             \n\
             --b1_u\n\
             Content-Type: text/plain; charset = \"utf-8\"\n\
             Content-Transfer-Encoding: base64\n\
             \n\
             SGVsbG8K\n\
             \n\
             \n\
             --b1_u\n\
             Content-Type: text/html; charset = \"utf-8\"\n\
             Content-Transfer-Encoding: base64\n\
             \n\
             PGI+SGk8L2I+\n\
             \n\
             \n\
             \n\
             --b1_u--\n"
        ));
        assert!(text.ends_with("--b1_u--\n"));
    }

    #[test]
    fn refuses_what_phpmailer_refuses() {
        let mut empty = message(None);
        empty.text_body = "0".to_owned();
        assert_eq!(render(&empty, "D", "u").unwrap_err(), "Message body empty");
        let mut invalid = message(None);
        invalid.to = vec![EmailAddress::parse("nobody@localhost")];
        assert_eq!(
            render(&invalid, "D", "u").unwrap_err(),
            "Invalid address: nobody@localhost"
        );
    }

    #[test]
    fn writes_data_lines_like_class_smtp() {
        let long = format!("X-Long: {}\nBody: x\n\nbody\r\n", "a ".repeat(600));
        let lines = data_lines(long.as_bytes());
        let text = String::from_utf8(lines).unwrap();
        let mut split = text.split("\r\n");
        let first = split.next().unwrap();
        assert!(first.len() <= MAX_LINE_LENGTH);
        assert!(first.ends_with('a'));
        assert!(split.next().unwrap().starts_with('\t'));
        assert!(text.ends_with("\r\nbody\r\n\r\n\r\n"));
    }

    #[test]
    fn validates_addresses_conservatively() {
        assert!(is_valid_address("alice@example.com"));
        assert!(is_valid_address("phab-bot@bmo.tld"));
        assert!(!is_valid_address("alice@localhost"));
        assert!(!is_valid_address("alice@example.1com"));
        assert!(!is_valid_address("a..b@example.com"));
        assert!(!is_valid_address("\"a b\"@example.com"));
    }

    #[test]
    fn formats_dates_with_offsets() {
        assert_eq!(
            rfc_date("Mon, 2 Jan 2023 03:04:05", 0),
            "Mon, 2 Jan 2023 03:04:05 +0000"
        );
        assert_eq!(rfc_date("D", -5 * 3600 - 1800), "D -0530");
    }
}
