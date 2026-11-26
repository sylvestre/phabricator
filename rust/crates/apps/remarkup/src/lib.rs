//! `remarkup.process` (`RemarkupProcessConduitAPIMethod`): texts rendered
//! as HTML by the remarkup engine of a context, for the caller.
//!
//! The Rust engine renders the rules of libphutil like PHP (its test
//! corpus and random documents are checked against PHP), but styles
//! Phabricator's own rules (mentions, object tags, icons, embeds...) for
//! the Rust interface. So each text is rendered twice: by the pages'
//! engine, with the references the viewer can see resolved
//! (`phab_web::remarkup`), and by an engine with only the libphutil
//! rules and the Bugzilla links of moz-extensions, which render like PHP.
//! When both agree, no other rule changed the text and the HTML is
//! PHP's. Left to PHP: texts on which they differ, references to
//! applications whose rules are not ported (builds, credentials, events,
//! dashboard panels) and handles of objects the pages don't resolve, code
//! blocks PHP highlights (with Pygments, its lexers or a custom
//! highlighter engine), and values of unusual types.

use std::sync::{Arc, Mutex, PoisonError};

use phab_conduit::{ApiRequest, Method, MethodError, MethodFuture, Registry};
use phab_core::{Config, Phid, PhidType};
use phab_db::Database;
use phab_remarkup::blocks::code::Code;
use phab_remarkup::blocks::default::Paragraph;
use phab_remarkup::blocks::header::Header;
use phab_remarkup::blocks::html_table::HtmlTable;
use phab_remarkup::blocks::list::List;
use phab_remarkup::blocks::literal::Literal;
use phab_remarkup::blocks::note::Note;
use phab_remarkup::blocks::quoted::{Quotes, Reply};
use phab_remarkup::blocks::rule::HorizontalRule;
use phab_remarkup::blocks::table::SimpleTable;
use phab_remarkup::preg::{self, Pattern};
use phab_remarkup::rules::bugzilla::BugzillaLink;
use phab_remarkup::rules::document_link::DocumentLink;
use phab_remarkup::rules::escape::EscapeRemarkup;
use phab_remarkup::rules::hyperlink::Hyperlink;
use phab_remarkup::rules::monospace::Monospace;
use phab_remarkup::rules::simple::{Bold, Del, Highlight, Italic, Underline};
use phab_remarkup::{BlockRule, Engine, Highlighter, InlineRule, Mode};
use phab_web::{PageError, WebState};
use serde_json::{Value, json};

/// `getEngineContexts()`: the engine of each context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineContext {
    /// `newDifferentialMarkupEngine()`.
    Differential,
    /// `newFeedMarkupEngine()`: no embedded `YouTube` videos.
    Feed,
    /// `newDiffusionMarkupEngine()`: headers get anchors.
    Diffusion,
}

impl EngineContext {
    /// `idx($this->getEngineContexts(), $context)`.
    fn parse(value: Option<&Value>) -> Result<Self, MethodError> {
        let invalid = || MethodError::Conduit {
            // Not one of the method's error types: "Unknown Error".
            code: "ERR-INVALID_ENGINE".to_owned(),
            info: None,
        };
        match value {
            Some(Value::String(context)) => match context.as_str() {
                "differential" => Ok(Self::Differential),
                "feed" => Ok(Self::Feed),
                "diffusion" => Ok(Self::Diffusion),
                _ => Err(invalid()),
            },
            None | Some(Value::Null | Value::Bool(_)) => Err(invalid()),
            Some(Value::Number(number)) if !number.is_f64() => Err(invalid()),
            Some(_) => Err(MethodError::Unsupported("a context which is not a string")),
        }
    }

    /// The engine options of the context, from the configuration.
    fn config(self, config: &Config) -> phab_remarkup::Config {
        let mut options = phab_app_differential::markup::engine_config(config);
        match self {
            Self::Differential => {}
            Self::Feed => options.youtube = false,
            Self::Diffusion => options.generate_toc = true,
        }
        options
    }
}

/// The `contents`: `foreach` takes the values of a list or a dictionary.
fn contents(value: Option<&Value>) -> Result<Vec<String>, MethodError> {
    let values: Vec<&Value> = match value {
        Some(Value::Array(list)) => list.iter().collect(),
        Some(Value::Object(map)) => map.values().collect(),
        _ => return Err(MethodError::Unsupported("contents which are not a list")),
    };
    values
        .into_iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect::<Option<_>>()
        .ok_or(MethodError::Unsupported("contents which are not strings"))
}

/// The languages `PhutilDefaultSyntaxHighlighterEngine` highlights with
/// its own highlighters, which Rust doesn't have; every other one is
/// Pygments' or plain text (Bug 2093445 dropped the lexers of PHP,
/// Python, Java and JSON, and the console highlighter).
const LEXER_LANGUAGES: &[&str] = &["diviner", "remarkup", "rainbow"];

/// Whether PHP shows code in `language` as plain text, like the engine
/// does without a highlighter.
fn is_plain(language: &[u8], pygments: bool) -> bool {
    match std::str::from_utf8(language) {
        Ok("text" | "txt") => true,
        Ok(language) => !pygments && !LEXER_LANGUAGES.contains(&language),
        Err(_) => false,
    }
}

/// The PHID types whose `{PHID-...}` handles the pages resolve.
const HANDLE_TYPES: &[PhidType] = &[
    PhidType::REVISION,
    PhidType::PROJECT,
    PhidType::USER,
    PhidType::REPOSITORY,
    PhidType::FILE,
];

/// Something like a reference to a build (`B1`), a credential (`K1`), an
/// event (`E1`) or a dashboard panel (`W1`), whose rules are not ported:
/// the Rust engine leaves them as written, PHP may link them.
static UNPORTED_OBJECTS: Pattern = Pattern::new(r"(?<![A-Za-z0-9])[BEKW][1-9]");

/// A code block of the HTML.
static CODE_BLOCK: Pattern = Pattern::new(r#"<pre class="remarkup-code"#);

/// Whether the pages' engine resolves every reference of `text` PHP
/// would.
fn resolves_like_php(engine: &Engine, text: &str) -> bool {
    if preg::is_match(&UNPORTED_OBJECTS, text.as_bytes()) {
        return false;
    }
    let prepared = engine.prepare(text.as_bytes(), Mode::Html);
    prepared
        .handle_phids()
        .into_iter()
        .all(|phid| HANDLE_TYPES.contains(&Phid::new(phid).phid_type()))
}

/// The engine with only the rules of libphutil and the Bugzilla links,
/// in the order of `newMarkupEngine()`.
fn trusted_engine(options: phab_remarkup::Config) -> Engine {
    let rules: Vec<Box<dyn InlineRule>> = vec![
        Box::new(EscapeRemarkup),
        Box::new(Monospace),
        Box::new(DocumentLink),
        Box::new(BugzillaLink),
        Box::new(Hyperlink::default()),
        Box::new(Bold),
        Box::new(Italic),
        Box::new(Del),
        Box::new(Underline),
        Box::new(Highlight),
    ];
    let blocks: Vec<Box<dyn BlockRule>> = vec![
        Box::new(Quotes),
        Box::new(Reply),
        Box::new(Literal),
        Box::new(Header),
        Box::new(HorizontalRule),
        Box::new(List),
        Box::new(Code),
        Box::new(Note),
        Box::new(HtmlTable),
        Box::new(SimpleTable),
        Box::new(Paragraph),
    ];
    Engine::new(options, blocks, rules)
}

/// What the pages' remarkup needs, for one call.
struct State {
    database: Database,
    config: Config,
    now: i64,
    engine: Engine,
}

impl WebState for State {
    fn database(&self) -> &Database {
        &self.database
    }

    fn config(&self) -> &Config {
        &self.config
    }

    fn now(&self) -> i64 {
        self.now
    }

    fn remarkup(&self) -> &Engine {
        &self.engine
    }

    fn php(&self) -> Option<&phab_web::php::PhpConduit> {
        None
    }
}

fn page_error(error: PageError) -> MethodError {
    match error {
        PageError::Fallback(reason) => MethodError::Unsupported(reason),
        other => MethodError::internal(format!("{other:?}")),
    }
}

/// `remarkup.process`.
pub struct RemarkupProcess;

impl Method for RemarkupProcess {
    fn name(&self) -> &'static str {
        "remarkup.process"
    }

    fn params(&self) -> &'static [&'static str] {
        &["context", "contents"]
    }

    fn application(&self) -> Option<phab_conduit::Application> {
        None
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("ERR-NO-CONTENT", "Content may not be empty."),
            ("ERR-INVALID-ENGINE", "Invalid markup engine."),
        ]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let engine_context = EngineContext::parse(request.value("context"))?;
            let texts = contents(request.value("contents"))?;
            if texts.is_empty() {
                return Ok(json!([]));
            }
            let config = &request.context.config;
            let custom_highlighter = config
                .nonempty_string("syntax-highlighter.engine")
                .is_some_and(|engine| engine != "PhutilDefaultSyntaxHighlighterEngine");
            let pygments = config.truthy("pygments.enabled");

            // The languages of the code blocks the engines would highlight.
            let languages: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
            let mut options = engine_context.config(config);
            let seen = Arc::clone(&languages);
            options.highlighter = Some(Highlighter(Arc::new(move |language: &[u8], _: &[u8]| {
                seen.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(language.to_vec());
                None
            })));
            let plain = trusted_engine(options.clone());
            let engine = phab_remarkup::phabricator::engine(options);
            if !texts.iter().all(|text| resolves_like_php(&engine, text)) {
                return Err(MethodError::Unsupported(
                    "references the pages don't resolve",
                ));
            }

            let state = State {
                database: request.context.database.clone(),
                config: config.clone(),
                now: request.now,
                engine,
            };
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            let rendered =
                phab_web::remarkup::render_all(&state, &request.viewer, &refs, Mode::Html)
                    .await
                    .map_err(page_error)?;
            let mut results = Vec::with_capacity(rendered.len());
            for (text, markup) in texts.iter().zip(rendered) {
                let html = markup.into_bytes();
                if plain.markup(text.as_bytes(), Mode::Html).output.as_bytes() != html {
                    return Err(MethodError::Unsupported(
                        "remarkup Phabricator's rules render",
                    ));
                }
                if custom_highlighter && preg::is_match(&CODE_BLOCK, &html) {
                    return Err(MethodError::Unsupported("a custom highlighter engine"));
                }
                let html = String::from_utf8(html)
                    .map_err(|_| MethodError::Unsupported("HTML which is not UTF-8"))?;
                results.push(json!({"content": html}));
            }
            let highlighted = languages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .any(|language| !is_plain(language, pygments));
            if highlighted {
                return Err(MethodError::Unsupported("code blocks PHP highlights"));
            }
            Ok(Value::Array(results))
        })
    }
}

/// Registers the Remarkup methods.
pub fn register(registry: &mut Registry) {
    registry.register(RemarkupProcess);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts() {
        assert_eq!(
            EngineContext::parse(Some(&json!("feed"))),
            Ok(EngineContext::Feed)
        );
        for context in [None, Some(json!("nope")), Some(json!(1)), Some(json!(true))] {
            assert_eq!(
                EngineContext::parse(context.as_ref()),
                Err(MethodError::Conduit {
                    code: "ERR-INVALID_ENGINE".to_owned(),
                    info: None
                })
            );
        }
        assert!(matches!(
            EngineContext::parse(Some(&json!(["feed"]))),
            Err(MethodError::Unsupported(_))
        ));
    }

    #[test]
    fn plain_languages() {
        assert!(is_plain(b"text", true));
        assert!(is_plain(b"rust", false));
        assert!(!is_plain(b"rust", true));
        // PHP's JSON, PHP, Python and Java lexers are gone (Bug 2093445).
        for language in ["json", "php", "py", "java", "console"] {
            assert!(is_plain(language.as_bytes(), false), "{language}");
            assert!(!is_plain(language.as_bytes(), true), "{language}");
        }
        assert!(!is_plain(b"rainbow", false));
    }

    #[test]
    fn reads_contents() {
        assert_eq!(
            contents(Some(&json!({"a": "x", "b": "y"}))),
            Ok(vec!["x".to_owned(), "y".to_owned()])
        );
        assert!(contents(Some(&json!(["x", 1]))).is_err());
        assert!(contents(None).is_err());
    }

    #[test]
    fn unported_objects() {
        for text in ["see B12", "{W3}", "https://phab.test/K1"] {
            assert!(preg::is_match(&UNPORTED_OBJECTS, text.as_bytes()), "{text}");
        }
        for text in ["AB12", "B0", "web"] {
            assert!(
                !preg::is_match(&UNPORTED_OBJECTS, text.as_bytes()),
                "{text}"
            );
        }
    }
}
