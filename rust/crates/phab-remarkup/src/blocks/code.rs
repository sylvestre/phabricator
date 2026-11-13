//! `PhutilRemarkupCodeBlockRule`: indented blocks and ```` ``` ```` fences,
//! with `lang=`, `name=`, `lines=` and `counterexample` options.

use indexmap::IndexMap;

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::options::{self, OptionValue};
use crate::preg::{self, Pattern};
use crate::rule::BlockRule;
use crate::{highlight, php};

static INDENTED: Pattern = Pattern::new(r"^(\s{2,}).+");
static FENCE_START: Pattern = Pattern::new(r"^\s*(```)");
static FENCE_ONE_LINE: Pattern = Pattern::new(r"^\s*(```)(.*)(```)\s*$");
static FENCE_END: Pattern = Pattern::new(r"```\s*$");
static INDENT: Pattern = Pattern::new(r"^\s{2,}");
static FENCED_LANGUAGE: Pattern = Pattern::new(r"^\s*```(([a-z]+)\n)?");
static OPENING_FENCE: Pattern = Pattern::new(r"^\s*```(\s*\n)?");
static LINE_START: Pattern = Pattern::new(r"(?m)^");

const OPTION_KEYS: &[&[u8]] = &[b"counterexample", b"lang", b"name", b"lines"];

/// `PhutilRemarkupCodeBlockRule`.
pub struct Code;

impl BlockRule for Code {
    fn class_name(&self) -> &'static str {
        "PhutilRemarkupCodeBlockRule"
    }

    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize {
        let fenced = if preg::is_match(&INDENTED, lines[cursor]) {
            false
        } else if preg::is_match(&FENCE_START, lines[cursor]) {
            true
        } else {
            return 0;
        };
        if fenced && preg::is_match(&FENCE_ONE_LINE, lines[cursor]) {
            return 1;
        }
        let mut num_lines = 1;
        for line in &lines[cursor + 1..] {
            if fenced {
                num_lines += 1;
                if preg::is_match(&FENCE_END, line) {
                    break;
                }
            } else {
                if !php::is_blank(line) && !preg::is_match(&INDENT, line) {
                    break;
                }
                num_lines += 1;
            }
        }
        num_lines
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        _children: Option<Markup>,
    ) -> Option<Markup> {
        let mut text = text.to_vec();
        let mut fenced_language = None;
        if let Some(groups) = preg::captures(&FENCED_LANGUAGE, &text) {
            if groups.len() == 3 && !groups.get(2).is_empty() {
                fenced_language = Some(groups.get(2).to_vec());
            }
            text = preg::replace(&OPENING_FENCE, &text, b"");
            text = preg::replace(&FENCE_END, &text, b"");
        }

        let mut lines: Vec<&[u8]> = php::explode_lines(&text);
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }

        let options = take_options(&mut lines, fenced_language);
        let option = |key: &[u8]| options.get(key);

        // Back to a 0-level indent.
        let min_indent = lines
            .iter()
            .filter_map(|line| line.iter().position(|byte| *byte != b' '))
            .fold(80, usize::min);
        let mut text = php::implode(b"\n", &lines);
        if min_indent > 0 {
            let pattern = preg::compile(&format!("(?m)^{}", " ".repeat(min_indent)));
            text = preg::replace_regex(&pattern, &text, b"");
        }

        let counterexample = option(b"counterexample").is_some_and(is_truthy);
        if cx.is_text_mode() {
            return Some(text_block(&text, counterexample, option(b"name")));
        }

        let language = match option(b"lang").filter(|value| is_truthy(value)) {
            Some(language) => as_string(language),
            None => highlight::guess_language(&text).unwrap_or_else(|| {
                cx.config()
                    .default_language
                    .clone()
                    .filter(|language| !language.is_empty() && language != "0")
                    .unwrap_or_else(|| "text".to_owned())
                    .into_bytes()
            }),
        };

        let mail = cx.is_html_mail_mode();
        let code_body = highlight_source(&text, &language, counterexample, option(b"lines"), mail);

        let header_attributes: Vec<u8> = if mail {
            let mut rules = vec![
                "padding: 6px 12px;",
                "font-size: 13px;",
                "font-weight: bold;",
                "display: inline-block;",
                "border-top-left-radius: 3px;",
                "border-top-right-radius: 3px;",
                "color: rgba(0,0,0,.75);",
            ];
            rules.push(if counterexample {
                "background: #f7e6e6"
            } else {
                "background: rgba(71, 87, 120, 0.08);"
            });
            rules.join(" ").into_bytes()
        } else {
            b"remarkup-code-header".to_vec()
        };
        let header_key = if mail { "style" } else { "class" };
        let name_header = option(b"name")
            .filter(|value| is_truthy(value))
            .map(|name| {
                markup::tag(
                    "div",
                    &[(header_key, Some(&header_attributes))],
                    Content::One(&Markup::Text(as_string(name))),
                )
            });

        let class: &[u8] = if counterexample {
            b"remarkup-code-block code-block-counterexample"
        } else {
            b"remarkup-code-block"
        };
        let block_style: Option<&[u8]> = mail.then_some(b"margin: 12px 0;");
        let content = vec![name_header, Some(code_body)];
        Some(markup::tag(
            "div",
            &[
                ("class", Some(class)),
                ("style", block_style),
                ("data-code-lang", Some(&language)),
                ("data-sigil", Some(b"remarkup-code-block")),
            ],
            Content::List(&content),
        ))
    }
}

/// Takes the options off the first line, if it is a valid option list.
/// With a fenced language, invalid options mean the line is the language
/// (` ```lang `).
fn take_options(
    lines: &mut Vec<&[u8]>,
    fenced_language: Option<Vec<u8>>,
) -> IndexMap<Vec<u8>, OptionValue> {
    let mut custom = options::parse(lines.first().copied().unwrap_or_default());
    if custom.is_empty() {
        return IndexMap::new();
    }
    let mut valid = custom
        .keys()
        .all(|key| OPTION_KEYS.contains(&key.as_slice()));
    if !valid && let Some(language) = fenced_language {
        valid = true;
        custom.insert(b"lang".to_vec(), OptionValue::Value(language));
    }
    if !valid {
        return IndexMap::new();
    }
    lines.remove(0);
    custom
}

/// The text mode rendering: the options worth showing, then the code
/// indented by two spaces.
fn text_block(text: &[u8], counterexample: bool, name: Option<&OptionValue>) -> Markup {
    let mut header = Vec::new();
    if counterexample {
        header.push(b"counterexample".to_vec());
    }
    if let Some(name) = name.map(as_string).filter(|name| !name.is_empty()) {
        header.push([b"name=".as_slice(), &name].concat());
    }
    let mut out = Vec::new();
    if !header.is_empty() {
        out.push(php::implode(b", ", &header));
    }
    out.push(preg::replace(&LINE_START, text, b"  "));
    Markup::Text(php::implode(b"\n", &out))
}

/// `highlightSource()`: the `<pre>` with the highlighted code.
fn highlight_source(
    text: &[u8],
    language: &[u8],
    counterexample: bool,
    lines: Option<&OptionValue>,
    mail: bool,
) -> Markup {
    let aux_class: &[u8] = if counterexample {
        b" remarkup-counterexample"
    } else {
        b""
    };
    let mut aux_style: Option<Vec<u8>> = mail.then(|| {
        let background = if counterexample {
            "background: #f7e6e6;"
        } else {
            "background: rgba(71, 87, 120, 0.08);"
        };
        format!(
            "font: 11px/15px \"Menlo\", \"Consolas\", \"Monaco\", monospace; padding: 12px; margin: 0; {background}"
        )
        .into_bytes()
    });
    if let Some(lines) = lines.filter(|value| is_truthy(value)) {
        // A minimum size, or the scrollbar is unusable.
        let height = php_int(&as_string(lines)).max(6);
        let mut style = aux_style.unwrap_or_default();
        style.extend_from_slice(
            format!(" max-height: {}em; overflow: auto;", 2 * height).as_bytes(),
        );
        aux_style = Some(style);
    }

    let highlighted = highlight::highlight(language, text);
    let body = Markup::html(php::rtrim(highlighted.as_bytes(), php::WHITESPACE));
    let mut class = b"remarkup-code".to_vec();
    class.extend_from_slice(aux_class);
    markup::tag(
        "pre",
        &[("class", Some(&class)), ("style", aux_style.as_deref())],
        Content::One(&body),
    )
}

/// PHP truthiness of an option: `true`, or a string other than `""` and
/// `"0"`.
fn is_truthy(value: &OptionValue) -> bool {
    match value {
        OptionValue::Flag => true,
        OptionValue::Value(bytes) => !bytes.is_empty() && bytes != b"0",
    }
}

/// `(string)` of an option: `true` is `"1"`.
fn as_string(value: &OptionValue) -> Vec<u8> {
    match value {
        OptionValue::Flag => b"1".to_vec(),
        OptionValue::Value(bytes) => bytes.clone(),
    }
}

/// `(int)$text`: the leading digits, with an optional sign.
fn php_int(text: &[u8]) -> i64 {
    let text = php::ltrim(text, b" \t\n\r\x0B\x0C");
    let (negative, digits) = match text.first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let value =
        digits
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .fold(0i64, |value, digit| {
                value
                    .saturating_mul(10)
                    .saturating_add(i64::from(digit - b'0'))
            });
    if negative { -value } else { value }
}
