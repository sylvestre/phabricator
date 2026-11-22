//! `differential.parsecommitmessage`
//! (`DifferentialParseCommitMessageConduitAPIMethod`): the fields of a
//! commit message, the transactions which would apply them and the errors
//! (`DifferentialCommitMessageParser::parseFields()`).
//!
//! Left to PHP: messages with carriage returns, titles longer than 250
//! bytes (which `PhutilUTF8StringTruncator` shortens), lists naming
//! objects other than users and projects, the check of bug numbers with
//! Bugzilla, revision URIs other than plain `http(s)://host/D123` ones,
//! and the validation of a refused default title.

use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture};
use phab_core::{PhidType, php};
use serde_json::{Map, Value, json};

use super::fields::{self, DEFAULT_TITLE, Field, Kind};
use super::get::production_uri;
use super::objects::{self, Listed};
use crate::search;

/// `differential.parsecommitmessage`.
pub struct ParseCommitMessage;

/// PHP's `trim()` with its default characters.
fn php_trim(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
}

/// PHP's `rtrim()` with its default characters.
fn php_rtrim(text: &str) -> &str {
    text.trim_end_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
}

/// `phutil_split_lines($text, false)` of a text without carriage returns:
/// a trailing empty line is dropped, an empty text is one empty line.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return vec![""];
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// `DifferentialCommitMessageParser::parseCorpus()`: the text of each
/// field, in the order they first appear, and the errors about fields
/// given twice. The labels are the fields' names and aliases.
fn parse_corpus(corpus: &str, fields: &[Field]) -> (Vec<(Field, String)>, Vec<String>) {
    let mut labels: Vec<(String, Field)> = Vec::new();
    for &field in fields {
        for label in field.aliases().iter().chain([&field.name()]) {
            labels.push((label.to_ascii_lowercase(), field));
        }
    }
    // The label regexp, /^(?P<field>labels):(?P<text>.*)$/i: labels have
    // no colon, so a label line starts with one of them and a colon.
    let label_of = |line: &str| -> Option<(Field, usize)> {
        let colon = line.find(':')?;
        let label = line[..colon].to_ascii_lowercase();
        labels
            .iter()
            .find(|(known, _)| *known == label)
            .map(|(_, field)| (*field, colon))
    };

    let mut errors = Vec::new();
    let mut seen: Vec<Field> = Vec::new();
    let mut lines: Vec<(Field, &str)> = Vec::new();
    let mut field = Field::Title;
    for line in split_lines(php_trim(corpus)) {
        // The first line is always the title.
        if !seen.contains(&Field::Title) {
            field = Field::Title;
            seen.push(field);
            lines.push((field, php_trim(line)));
            continue;
        }
        let mut text = line;
        if let Some((labeled, colon)) = label_of(line) {
            text = php_trim(&line[colon + 1..]);
            field = labeled;
            if seen.contains(&field) {
                errors.push(format!(
                    "Field \"{}\" occurs twice in commit message!",
                    &line[..colon]
                ));
            } else {
                seen.push(field);
            }
        }
        lines.push((field, text));
    }

    let mut grouped: Vec<(Field, Vec<&str>)> = Vec::new();
    for (field, line) in lines {
        match grouped.iter_mut().find(|(known, _)| *known == field) {
            Some((_, list)) => list.push(line),
            None => grouped.push((field, vec![line])),
        }
    }

    // Without a summary, the text after the first blank line of the title
    // is the summary.
    let has_summary = grouped.iter().any(|(field, _)| *field == Field::Summary);
    if !has_summary
        && let Some(index) = grouped.iter().position(|(field, _)| *field == Field::Title)
    {
        let title = &grouped[index].1;
        if let Some(blank) = title.iter().position(|line| php_trim(line).is_empty()) {
            let summary = title[blank..].to_vec();
            grouped[index].1.truncate(blank);
            if !php_trim(&summary.join("\n")).is_empty() {
                grouped.push((Field::Summary, summary));
            }
        }
    }

    let texts = grouped
        .into_iter()
        .map(|(field, lines)| {
            let text = php_rtrim(&lines.join("\n"))
                .trim_start_matches('\n')
                .to_owned();
            (field, text)
        })
        .collect();
    (texts, errors)
}

/// A parsed field value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Parsed {
    Text(String),
    List(Vec<Listed>),
    RevisionId(Option<i64>),
}

impl Parsed {
    /// The value as `fields` shows it; reviewers are flattened with their
    /// `!` (`flattenReviewers()`).
    fn to_json(&self) -> Value {
        match self {
            Self::Text(text) => json!(text),
            Self::List(list) => list
                .iter()
                .map(|item| {
                    let suffix = if item.blocking { "!" } else { "" };
                    json!(format!("{}{suffix}", item.phid))
                })
                .collect(),
            Self::RevisionId(id) => json!(id),
        }
    }
}

/// An integer as PHP's `(int)` casts a string of digits.
fn php_int(digits: &str) -> Result<i64, MethodError> {
    if digits.len() > 18 {
        return Err(MethodError::Unsupported("huge revision IDs"));
    }
    digits
        .parse()
        .map_err(|_| MethodError::Unsupported("unusual revision IDs"))
}

/// The hosts of `PhabricatorEnv::getSelfURIMap()`, lowercased.
fn self_hosts(request: &ApiRequest<'_>) -> Result<Vec<String>, MethodError> {
    let config = &request.context.config;
    let mut uris = vec![
        production_uri(request, "/")?,
        config.uri("/").map_err(MethodError::internal)?,
    ];
    match config.value("phabricator.allowed-uris") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            for uri in list {
                uris.push(
                    uri.as_str()
                        .ok_or(MethodError::Unsupported("unusual phabricator.allowed-uris"))?
                        .to_owned(),
                );
            }
        }
        Some(_) => return Err(MethodError::Unsupported("unusual phabricator.allowed-uris")),
    }
    let mut hosts = Vec::new();
    for uri in uris {
        match simple_uri(&uri) {
            Some((host, _)) => hosts.push(host.to_ascii_lowercase()),
            None if !uri.contains(':') && !uri.contains('@') => {}
            None => return Err(MethodError::Unsupported("unusual self URIs")),
        }
    }
    Ok(hosts)
}

/// The host and path of `http(s)://host[:port][/path]`, without user,
/// query or fragment.
fn simple_uri(uri: &str) -> Option<(&str, &str)> {
    let rest = uri
        .strip_prefix("http://")
        .or_else(|| uri.strip_prefix("https://"))?;
    let (authority, path) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest, ""),
    };
    let host = match authority.split_once(':') {
        Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => host,
        Some(_) => return None,
        None => authority,
    };
    let host_ok = !host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-');
    let path_ok = path
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && !b"?#%@\\".contains(&byte));
    (host_ok && path_ok).then_some((host, path))
}

/// `DifferentialRevisionIDCommitMessageField::parseFieldValue()`.
fn parse_revision_id(request: &ApiRequest<'_>, value: &str) -> Result<Option<i64>, MethodError> {
    let first = php_trim(split_lines(php_trim(value))[0]);
    if let Some(digits) = first.strip_prefix(['d', 'D'])
        && !digits.is_empty()
        && !digits.starts_with('0')
        && digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return php_int(digits).map(Some);
    }
    // parseRevisionIDFromURI(): a "/D123" URI of the install.
    match simple_uri(first) {
        Some((host, path)) => {
            if !self_hosts(request)?.contains(&host.to_ascii_lowercase()) {
                return Ok(None);
            }
            match path.strip_prefix("/D") {
                Some(digits)
                    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    php_int(digits).map(Some)
                }
                _ => Ok(None),
            }
        }
        // Without a colon, an "@" or a "//", a URI has no domain.
        None if !first.contains([':', '@']) && !first.contains("//") => Ok(None),
        None => Err(MethodError::Unsupported("unusual revision URIs")),
    }
}

/// The PHID types a list field allows, whether it allows partial results
/// and whether reviewers may be blocking.
fn list_options(field: Field) -> (&'static [PhidType], bool, bool) {
    const PEOPLE: &[PhidType] = &[PhidType::USER, PhidType::PROJECT];
    match field {
        Field::Reviewers => (PEOPLE, false, true),
        Field::ReviewedBy => (PEOPLE, true, false),
        Field::Tags => (&[PhidType::PROJECT], false, false),
        _ => (PEOPLE, false, false),
    }
}

/// `parseFieldValue()`: the value, or the message of the
/// `DifferentialFieldParseException`.
async fn parse_value(
    request: &ApiRequest<'_>,
    field: Field,
    text: &str,
) -> Result<Result<Parsed, String>, MethodError> {
    match field.kind() {
        Kind::Text if field == Field::Title && text == DEFAULT_TITLE => Ok(Err(
            "Replace the default title line with a human-readable revision title which describes \
             the changes you are making."
                .to_owned(),
        )),
        Kind::Text => Ok(Ok(Parsed::Text(text.to_owned()))),
        Kind::RevisionId => Ok(Ok(Parsed::RevisionId(parse_revision_id(request, text)?))),
        Kind::List => {
            let (allowed, partial, suffixes) = list_options(field);
            Ok(
                objects::parse_list(request, text, allowed, partial, suffixes)
                    .await?
                    .map(Parsed::List),
            )
        }
    }
}

/// `getFieldTransactions()`.
fn transactions(field: Field, value: &Parsed) -> Vec<Value> {
    let kind = match field {
        Field::Title => "title",
        Field::Summary => "summary",
        Field::TestPlan => "testPlan",
        Field::Reviewers => "reviewers.set",
        Field::Subscribers => "subscribers.set",
        Field::Tags => "projects.set",
        Field::Auditors | Field::BugId | Field::BlameRevision | Field::RevertPlan => field.key(),
        Field::ReviewedBy | Field::RevisionId | Field::Conflicts | Field::GitSvnId => {
            return Vec::new();
        }
    };
    let value = match (field, value) {
        (Field::Reviewers, Parsed::List(list)) => list
            .iter()
            .map(|item| {
                if item.blocking {
                    json!(format!("blocking({})", item.phid))
                } else {
                    json!(item.phid)
                }
            })
            .collect(),
        (_, Parsed::List(list)) => list.iter().map(|item| json!(item.phid)).collect(),
        (_, value) => value.to_json(),
    };
    vec![json!({"type": kind, "value": value})]
}

/// `validateFieldValue()`: the message of the
/// `DifferentialFieldValidationException`, if any.
fn validate(
    request: &ApiRequest<'_>,
    field: Field,
    value: Option<&Parsed>,
) -> Result<Option<&'static str>, MethodError> {
    let text = match value {
        Some(Parsed::Text(text)) => Some(text.as_str()),
        _ => None,
    };
    let config = &request.context.config;
    match field {
        Field::Title => match value {
            Some(_) if text.is_some_and(str::is_empty) => Ok(Some(
                "You must provide a revision title in the first line of your commit message.",
            )),
            Some(_) => Ok(None),
            // strlen(null) after the default title was refused.
            None => Err(MethodError::Unsupported(
                "the validation of a refused title",
            )),
        },
        Field::TestPlan => {
            let required = config
                .value("differential.require-test-plan-field")
                .is_none_or(php::is_truthy);
            Ok((required && text.is_none_or(str::is_empty)).then_some(
                "You must provide a test plan. Describe the actions you performed to verify the \
                 behavior of this change.",
            ))
        }
        // DifferentialBugzillaBugIDValidator::validate().
        Field::BugId => {
            let bug = php_trim(&text.unwrap_or_default().replace('#', "")).to_owned();
            if bug.is_empty() {
                let required = config.value("bugzilla.require_bugs") == Some(&Value::Bool(true));
                return Ok(required.then_some("Bugzilla Bug ID is required"));
            }
            if !bug.bytes().all(|byte| byte.is_ascii_digit()) || bug == "0" {
                return Ok(Some("Bugzilla Bug ID must be a valid bug number"));
            }
            // Administrators skip the check with Bugzilla.
            if request.viewer.is_admin {
                return Ok(None);
            }
            Err(MethodError::Unsupported("bug numbers Bugzilla checks"))
        }
        _ => Ok(None),
    }
}

impl Method for ParseCommitMessage {
    fn name(&self) -> &'static str {
        "differential.parsecommitmessage"
    }

    fn params(&self) -> &'static [&'static str] {
        &["corpus", "partial"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        Some(search::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let Some(Value::String(corpus)) = request.value("corpus") else {
                return Err(MethodError::Unsupported("a corpus which is not a string"));
            };
            if corpus.contains('\r') {
                return Err(MethodError::Unsupported("messages with carriage returns"));
            }
            let partial = request.value("partial").is_some_and(php::is_truthy);
            let fields = fields::enabled_fields(&request.context.config)?;

            let (texts, mut errors) = parse_corpus(corpus, &fields);
            if texts
                .iter()
                .any(|(field, text)| *field == Field::Title && text.len() > 250)
            {
                return Err(MethodError::Unsupported("titles PHP truncates"));
            }
            let mut parsed: Vec<(Field, Parsed)> = Vec::new();
            let mut xactions: Vec<Value> = Vec::new();
            for (field, text) in &texts {
                match parse_value(request, *field, text).await? {
                    Ok(value) => {
                        xactions.extend(transactions(*field, &value));
                        parsed.push((*field, value));
                    }
                    Err(message) => errors.push(format!(
                        "Error parsing field \"{}\": {message}",
                        field.name()
                    )),
                }
            }
            if !partial {
                for &field in &fields {
                    let value = parsed
                        .iter()
                        .find(|(known, _)| *known == field)
                        .map(|(_, value)| value);
                    if let Some(message) = validate(request, field, value)? {
                        errors.push(format!(
                            "Invalid or missing field \"{}\": {message}",
                            field.name()
                        ));
                    }
                }
            }

            let revision_id = parsed
                .iter()
                .find(|(field, _)| *field == Field::RevisionId)
                .map_or(Value::Null, |(_, value)| value.to_json());
            // An empty PHP array is a JSON list.
            let field_map = if parsed.is_empty() {
                json!([])
            } else {
                Value::Object(
                    parsed
                        .iter()
                        .map(|(field, value)| (field.key().to_owned(), value.to_json()))
                        .collect::<Map<_, _>>(),
                )
            };
            Ok(json!({
                "errors": errors,
                "fields": field_map,
                "revisionIDFieldInfo": {
                    "value": revision_id,
                    "validDomain": production_uri(request, "")?,
                },
                "transactions": xactions,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIELDS: &[Field] = &[
        Field::Title,
        Field::Summary,
        Field::TestPlan,
        Field::Reviewers,
        Field::Subscribers,
    ];

    fn parse(corpus: &str) -> (Vec<(Field, String)>, Vec<String>) {
        parse_corpus(corpus, FIELDS)
    }

    fn texts(pairs: &[(Field, &str)]) -> Vec<(Field, String)> {
        pairs
            .iter()
            .map(|(field, text)| (*field, (*text).to_owned()))
            .collect()
    }

    // The messages of DifferentialCommitMessageParserTestCase.
    #[test]
    fn parses_like_the_php_test_messages() {
        assert_eq!(
            parse("fix bug\n"),
            (texts(&[(Field::Title, "fix bug")]), vec![])
        );
        assert_eq!(
            parse("fix bug\n\nit had a bug but I fixed it\n"),
            (
                texts(&[
                    (Field::Title, "fix bug"),
                    (Field::Summary, "it had a bug but I fixed it"),
                ]),
                vec![]
            )
        );
        assert_eq!(
            parse(
                "Title\n\nSummary: This is the summary.\n\nTest Plan: Tested things.\n\nReviewers: alincoln\n\n"
            ),
            (
                texts(&[
                    (Field::Title, "Title"),
                    (Field::Summary, "This is the summary."),
                    (Field::TestPlan, "Tested things."),
                    (Field::Reviewers, "alincoln"),
                ]),
                vec![]
            )
        );
        assert_eq!(
            parse("summary: orange\n"),
            (texts(&[(Field::Title, "summary: orange")]), vec![])
        );
        assert_eq!(
            parse("fix bug\n\ncc: red\nCCs: blue\n"),
            (
                texts(&[(Field::Title, "fix bug"), (Field::Subscribers, "red\nblue")]),
                vec!["Field \"CCs\" occurs twice in commit message!".to_owned()]
            )
        );
    }

    #[test]
    fn keeps_unlabeled_lines_as_they_are() {
        assert_eq!(
            parse("  T  \nmore\n\n  indented\nTested:\n  x\n\nReviewers :y"),
            (
                texts(&[
                    (Field::Title, "T\nmore"),
                    (Field::TestPlan, "  x\n\nReviewers :y"),
                    (Field::Summary, "  indented"),
                ]),
                vec![]
            )
        );
        assert_eq!(parse(""), (texts(&[(Field::Title, "")]), vec![]));
        assert_eq!(
            parse("a\ntitle: b"),
            (
                texts(&[(Field::Title, "a\nb")]),
                vec!["Field \"title\" occurs twice in commit message!".to_owned()]
            )
        );
    }

    #[test]
    fn reads_simple_uris() {
        assert_eq!(
            simple_uri("https://phab.test:8080/D12"),
            Some(("phab.test", "/D12"))
        );
        assert_eq!(simple_uri("http://phab.test"), Some(("phab.test", "")));
        assert_eq!(simple_uri("http://u@phab.test/D1"), None);
        assert_eq!(simple_uri("http://phab.test/D1?x"), None);
        assert_eq!(simple_uri("ftp://phab.test/D1"), None);
    }
}
