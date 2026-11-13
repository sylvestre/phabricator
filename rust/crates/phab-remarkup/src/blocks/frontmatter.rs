//! `ReviewHelperFrontmatterRemarkupRule` (moz-extensions): the review
//! helper starts its comments with a block giving their severity and
//! category:
//!
//! ```text
//! ---
//! reviewhelper:
//!   severity: major
//!   category: security
//! ---
//! ```

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::BlockRule;

/// `MAX_LINES`: the closing `---` must come within this many lines.
const MAX_LINES: usize = 12;
const SEVERITIES: &[&str] = &["critical", "major", "minor", "nit"];
const CATEGORY_ICONS: &[(&str, &str)] = &[
    ("security", "fa-lock"),
    ("correctness", "fa-bug"),
    ("performance", "fa-bolt"),
    ("maintainability", "fa-wrench"),
    ("style", "fa-paint-brush"),
    ("scope", "fa-bullseye"),
];

static LINE: Pattern = Pattern::new(r"^\s+([a-z_]+):\s*([a-z_]+)$");

/// `ReviewHelperFrontmatter`: a valid frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    /// One of [`SEVERITIES`].
    pub severity: &'static str,
    /// One of the categories, if given.
    pub category: Option<(&'static str, &'static str)>,
}

impl Frontmatter {
    /// `newFromLines()`: `None` where PHP throws
    /// `ReviewHelperFrontmatterException`.
    pub fn from_lines(lines: &[&[u8]]) -> Option<Self> {
        let mut values: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for line in lines {
            let line = php::rtrim(line, php::WHITESPACE);
            let groups = preg::captures(&LINE, line)?;
            let key = groups.get(1).to_vec();
            let value = groups.get(2).to_vec();
            match values.iter_mut().find(|(existing, _)| *existing == key) {
                Some(entry) => entry.1 = value,
                None => values.push((key, value)),
            }
        }
        let value = |key: &[u8]| {
            values
                .iter()
                .find(|(existing, _)| existing == key)
                .map(|(_, value)| value)
        };
        let severity = SEVERITIES
            .iter()
            .find(|severity| Some(severity.as_bytes()) == value(b"severity").map(Vec::as_slice))
            .copied()?;
        let category = match value(b"category") {
            None => None,
            Some(category) => Some(
                CATEGORY_ICONS
                    .iter()
                    .find(|(name, _)| name.as_bytes() == category.as_slice())
                    .copied()?,
            ),
        };
        Some(Self { severity, category })
    }

    /// `getTextLabel()`: `[Major] [Security]`.
    pub fn text_label(&self) -> String {
        match self.category {
            Some((category, _)) => format!("[{}] [{}]", ucfirst(self.severity), ucfirst(category)),
            None => format!("[{}]", ucfirst(self.severity)),
        }
    }

    /// `toDictionary()`, encoded like `phutil_json_encode()`.
    fn to_json(&self) -> String {
        match self.category {
            Some((category, _)) => format!(
                r#"{{"severity":"{}","category":"{category}"}}"#,
                self.severity
            ),
            None => format!(r#"{{"severity":"{}","category":null}}"#, self.severity),
        }
    }
}

fn ucfirst(text: &str) -> String {
    let mut characters = text.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().chain(characters).collect())
        .unwrap_or_default()
}

/// `extractFrontmatterBlock()`: the lines between the `---` markers and
/// how many lines the block takes.
fn extract_block<'a>(lines: &[&'a [u8]]) -> Option<(Vec<&'a [u8]>, usize)> {
    if lines.len() < 3
        || php::rtrim(lines[0], php::WHITESPACE) != b"---"
        || php::rtrim(lines[1], php::WHITESPACE) != b"reviewhelper:"
    {
        return None;
    }
    (2..lines.len().min(MAX_LINES))
        .find(|index| php::rtrim(lines[*index], php::WHITESPACE) == b"---")
        .map(|index| (lines[2..index].to_vec(), index + 1))
}

/// `ReviewHelperFrontmatterRemarkupRule`.
pub struct ReviewHelperFrontmatter;

impl BlockRule for ReviewHelperFrontmatter {
    fn class_name(&self) -> &'static str {
        "ReviewHelperFrontmatterRemarkupRule"
    }

    fn priority(&self) -> i32 {
        250
    }

    /// Only at the start of a comment, or of a quoted comment.
    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize {
        if cursor != 0 {
            return 0;
        }
        let Some((_, count)) = extract_block(lines) else {
            return 0;
        };
        count + crate::blocks::rule::blank_lines_after(lines, count)
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        _children: Option<Markup>,
    ) -> Option<Markup> {
        let lines = php::split_lines(text);
        let frontmatter =
            extract_block(&lines).and_then(|(lines, _)| Frontmatter::from_lines(&lines));
        let Some(frontmatter) = frontmatter else {
            return Some(Markup::default());
        };
        if cx.is_text_mode() {
            return Some(Markup::text(frontmatter.text_label()));
        }

        let mut label = vec![Some(markup::tag(
            "span",
            &[("class", Some(b"reviewhelper-severity-part"))],
            Content::One(&Markup::text(ucfirst(frontmatter.severity))),
        ))];
        if let Some((category, icon)) = frontmatter.category {
            label.push(Some(Markup::text(" ")));
            // Screen readers skip the icon and read the category name.
            let icon = markup::tag(
                "span",
                &[
                    (
                        "class",
                        Some(format!("visual-only phui-icon-view phui-font-fa {icon}").as_bytes()),
                    ),
                    ("aria-hidden", Some(b"true")),
                ],
                Content::One(&Markup::default()),
            );
            let parts = vec![
                Some(icon),
                Some(Markup::text(" ")),
                Some(Markup::text(ucfirst(category))),
            ];
            label.push(Some(markup::tag(
                "span",
                &[("class", Some(b"reviewhelper-category-part"))],
                Content::List(&parts),
            )));
        }
        Some(markup::tag(
            "p",
            &[
                ("class", Some(b"reviewhelper-severity-label")),
                ("style", Some(b"display: none;")),
                (
                    "data-reviewhelper-frontmatter",
                    Some(frontmatter.to_json().as_bytes()),
                ),
            ],
            Content::List(&label),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_is_validated() {
        let parse = |lines: &[&str]| {
            let lines: Vec<&[u8]> = lines.iter().map(|line| line.as_bytes()).collect();
            Frontmatter::from_lines(&lines)
        };
        assert_eq!(
            parse(&["  severity: major\n", "  category: security\n"]).map(|f| f.text_label()),
            Some("[Major] [Security]".to_owned())
        );
        assert_eq!(
            parse(&["  severity: nit"]).map(|f| f.text_label()),
            Some("[Nit]".to_owned())
        );
        assert_eq!(parse(&["  category: style"]), None);
        assert_eq!(parse(&["  severity: huge"]), None);
        assert_eq!(parse(&["severity: major"]), None);
    }
}
