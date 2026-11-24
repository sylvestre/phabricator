//! Unit test results (`HarbormasterUnitSummaryView` and
//! `HarbormasterUnitPropertyView`), as the buildable and revision pages
//! show them.

use phab_app_harbormaster::web::UnitMessage;
use phab_remarkup::Mode;
use serde_json::Value;

use super::status::Status;
use crate::error::PageError;
use crate::state::WebState;
use crate::{diff_html, remarkup};

/// `newUnitMessageDetailsView()` shows this many bytes of text details.
const TEXT_LIMIT: usize = 2048;
/// ...and this many bytes of Remarkup details.
const REMARKUP_LIMIT: usize = 8192;
/// ...and this many lines of text.
const TEXT_LINES: usize = 3;

/// A row of the table.
pub(crate) struct UnitRow {
    pub(crate) status: Status,
    pub(crate) duration: Option<String>,
    pub(crate) name: String,
    pub(crate) uri: String,
    pub(crate) details_html: Option<String>,
    /// Text details are monospaced; Remarkup is rendered.
    pub(crate) is_text: bool,
}

/// The "Unit Tests" box.
pub(crate) struct UnitSummary {
    /// The status of the worst result.
    pub(crate) status: Status,
    /// The page of all results (`setShowViewAll()`), linked from the
    /// header and from the counts of the hidden ones.
    pub(crate) view_all_uri: Option<String>,
    pub(crate) rows: Vec<UnitRow>,
    pub(crate) any_duration: bool,
    /// The link to all results, with their counts, when some are hidden.
    pub(crate) full_results: Option<String>,
}

/// `HarbormasterUnitStatus`: label, icon, color and sort key of a result.
fn result_spec(result: &str) -> (String, &'static str, &'static str, &'static str) {
    let (label, icon, color, sort) = match result {
        "fail" => ("Failed", "fa-times", "red", "A"),
        "broken" => ("Broken", "fa-bomb", "indigo", "B"),
        "unsound" => ("Unsound", "fa-exclamation-triangle", "yellow", "C"),
        "pass" => ("Passed", "fa-check", "green", "D"),
        "skip" => ("Skipped", "fa-fast-forward", "blue", "E"),
        other => {
            return (
                format!("Unknown Status (\"{other}\")"),
                "fa-question-circle",
                "violet",
                "N",
            );
        }
    };
    (label.to_owned(), icon, color, sort)
}

pub(crate) fn result_status(result: &str) -> Status {
    let (label, icon, color, _) = result_spec(result);
    Status {
        name: label,
        icon,
        color,
    }
}

/// `HarbormasterUnitStatus::getUnitStatusCountLabel()`, as the English
/// translation says it.
fn count_label(result: &str, count: usize) -> String {
    let count = thousands(count as u64);
    let what = match result {
        "fail" => "Failed",
        "broken" => "Broken",
        "unsound" => "Unsound",
        "pass" => "Passed",
        "skip" => "Skipped",
        _ => "Other",
    };
    format!("{count} {what}")
}

/// `PhutilNumber`: digits grouped by thousands.
fn thousands(number: u64) -> String {
    let digits = number.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// The name of a message, as the string table gives it
/// (`HarbormasterBuildUnitMessageQuery::didFilterPage()`).
fn raw_name(message: &UnitMessage) -> Result<String, PageError> {
    if let Some(name) = &message.name {
        return Ok(name.clone());
    }
    let index = std::str::from_utf8(&message.name_index)
        .map_err(|_| PageError::Fallback("a unit test without name"))?;
    Ok(format!("Unknown Unit Message (\"{index}\")"))
}

/// `getUnitMessageDisplayName()`.
pub(crate) fn display_name(message: &UnitMessage) -> Result<String, PageError> {
    let mut name = raw_name(message)?;
    if !message.namespace.is_empty() {
        name = format!("{}::{name}", message.namespace);
    }
    if !message.engine.is_empty() {
        name = format!("{} > {name}", message.engine);
    }
    if name.is_empty() {
        return Ok(format!("Nameless Test ({})", message.id));
    }
    Ok(name)
}

/// `HarbormasterBuildUnitMessage::getSortKey()`.
fn sort_key(message: &UnitMessage) -> Result<String, PageError> {
    Ok(format!(
        "{}\0{}\0{}\0{}\0{}",
        result_spec(&message.result).3,
        message.engine,
        message.namespace,
        raw_name(message)?,
        message.id
    ))
}

/// Sorts messages like `msort($messages, 'getSortKey')`.
pub(crate) fn sort(messages: &mut Vec<UnitMessage>) -> Result<(), PageError> {
    let mut keyed: Vec<(String, UnitMessage)> = messages
        .drain(..)
        .map(|message| Ok((sort_key(&message)?, message)))
        .collect::<Result<_, PageError>>()?;
    keyed.sort_by(|left, right| left.0.cmp(&right.0));
    messages.extend(keyed.into_iter().map(|(_, message)| message));
    Ok(())
}

/// A string property; anything else is not something PHP shows.
fn string_property<'a>(message: &'a UnitMessage, key: &str) -> Result<Option<&'a str>, PageError> {
    match message.properties.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(PageError::Fallback("unit test details which are not text")),
    }
}

/// `newUnitMessageDetailsView($viewer)`: all the details, and whether they
/// are text (monospaced) rather than Remarkup or a message.
pub(crate) async fn full_details<S: WebState>(
    state: &S,
    viewer: &phab_policy::Viewer,
    message: &UnitMessage,
) -> Result<(String, bool), PageError> {
    let details = string_property(message, "details")?.unwrap_or("");
    let is_remarkup = string_property(message, "format")? == Some("remarkup");
    if details.is_empty() {
        return Ok(("<em>No details provided.</em>".to_owned(), false));
    }
    if !is_remarkup {
        return Ok((diff_html::escape(details), true));
    }
    let markup = remarkup::render(state, viewer, details, Mode::Html).await?;
    Ok((
        String::from_utf8_lossy(markup.as_bytes()).into_owned(),
        false,
    ))
}

/// `newUnitMessageDetailsView($viewer, true)`: the details, shortened.
async fn details<S: WebState>(
    state: &S,
    viewer: &phab_policy::Viewer,
    message: &UnitMessage,
) -> Result<(Option<String>, bool), PageError> {
    let details = string_property(message, "details")?.unwrap_or("");
    let is_remarkup = string_property(message, "format")? == Some("remarkup");
    if details.is_empty() {
        return Ok((None, !is_remarkup));
    }
    if !is_remarkup {
        // `PhutilUTF8StringTruncator` cuts at word boundaries.
        if details.len() > TEXT_LIMIT {
            return Err(PageError::Fallback("long unit test details"));
        }
        let lines: String = details.split_inclusive('\n').take(TEXT_LINES).collect();
        return Ok((Some(diff_html::escape(&lines)), true));
    }
    if details.len() > REMARKUP_LIMIT {
        return Ok((
            Some(format!(
                "<em>This test has too much data to display inline.</em> \u{b7} <a \
                 href=\"/harbormaster/unit/view/{}/\" target=\"_blank\">View Details</a>",
                message.id
            )),
            false,
        ));
    }
    let markup = remarkup::render(state, viewer, details, Mode::Html).await?;
    Ok((
        Some(String::from_utf8_lossy(markup.as_bytes()).into_owned()),
        false,
    ))
}

/// The "Unit Tests" box of the buildable `buildable_id`: the results,
/// worst first. With a `limit`, the first ones and a link to all of them;
/// without, all of them.
pub(crate) async fn summary<S: WebState>(
    state: &S,
    viewer: &phab_policy::Viewer,
    buildable_id: i64,
    mut messages: Vec<UnitMessage>,
    limit: Option<usize>,
) -> Result<UnitSummary, PageError> {
    sort(&mut messages)?;
    let status = messages.first().map_or_else(
        || Status {
            name: "No Unit Tests".to_owned(),
            icon: "fa-ban",
            color: "grey",
        },
        |head| result_status(&head.result),
    );
    let mut rows = Vec::new();
    let mut any_duration = false;
    for message in messages.iter().take(limit.unwrap_or(usize::MAX)) {
        let duration = message.duration.map(|duration| {
            // `(int)(1000 * $duration)` truncates.
            #[allow(clippy::cast_possible_truncation)]
            let millis = (1000.0 * duration) as i64;
            let text = thousands(millis.unsigned_abs());
            if millis < 0 {
                format!("-{text} ms")
            } else {
                format!("{text} ms")
            }
        });
        any_duration |= duration.is_some();
        let (details_html, is_text) = details(state, viewer, message).await?;
        rows.push(UnitRow {
            status: result_status(&message.result),
            duration,
            name: display_name(message)?,
            uri: format!("/harbormaster/unit/view/{}/", message.id),
            details_html,
            is_text,
        });
    }
    let view_all_uri = limit.map(|_| format!("/harbormaster/unit/{buildable_id}/"));
    let full_results = limit.filter(|limit| messages.len() > *limit).map(|_| {
        // `mgroup()`: the results in their first sorted position.
        let mut groups: Vec<(&str, usize)> = Vec::new();
        for message in &messages {
            match groups
                .iter_mut()
                .find(|(result, _)| *result == message.result)
            {
                Some((_, count)) => *count += 1,
                None => groups.push((&message.result, 1)),
            }
        }
        let counts: Vec<String> = groups
            .iter()
            .map(|(result, count)| count_label(result, *count))
            .collect();
        format!("View Full Test Results ({})", counts.join(" \u{b7} "))
    });
    Ok(UnitSummary {
        status,
        view_all_uri,
        rows,
        any_duration,
        full_results,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;

    fn message(id: i64, result: &str, name: &str) -> UnitMessage {
        UnitMessage {
            id,
            target_phid: phab_core::Phid::new("PHID-HMBT-1"),
            engine: String::new(),
            namespace: String::new(),
            name: Some(name.to_owned()),
            name_index: b"aaaaaaaaaaaa".to_vec(),
            result: result.to_owned(),
            duration: None,
            properties: Map::new(),
            date_created: 1,
        }
    }

    #[test]
    fn worst_results_first() {
        let mut messages = vec![
            message(1, "pass", "a"),
            message(2, "fail", "z"),
            message(3, "odd", "b"),
            message(4, "broken", "c"),
        ];
        sort(&mut messages).unwrap();
        let ids: Vec<i64> = messages.iter().map(|message| message.id).collect();
        assert_eq!(ids, [2, 4, 1, 3]);
    }

    #[test]
    fn names_like_php() {
        let mut named = message(7, "pass", "test_x");
        named.namespace = "Suite".to_owned();
        named.engine = "py".to_owned();
        assert_eq!(display_name(&named).unwrap(), "py > Suite::test_x");
        assert_eq!(
            display_name(&message(8, "pass", "")).unwrap(),
            "Nameless Test (8)"
        );
        let mut unknown = message(9, "pass", "");
        unknown.name = None;
        assert_eq!(
            display_name(&unknown).unwrap(),
            "Unknown Unit Message (\"aaaaaaaaaaaa\")"
        );
        let mut odd = message(10, "pass", "");
        odd.properties = match json!({"details": ["x"]}) {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        assert!(string_property(&odd, "details").is_err());
    }

    #[test]
    fn counts_like_the_english_translation() {
        assert_eq!(count_label("fail", 1234), "1,234 Failed");
        assert_eq!(count_label("weird", 2), "2 Other");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }
}
