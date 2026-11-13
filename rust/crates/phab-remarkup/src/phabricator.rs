//! The engine Phabricator renders comments and summaries with
//! (`PhabricatorMarkupEngine::newMarkupEngine()`), with the rules ported
//! so far.
//!
//! Not ported yet, so their syntax renders as text: `{{{eval}}}`,
//! `{nav}`, `{key}`, `{config}`, `{icon}`, emoji, `{PHID-...}` handles,
//! `YouTube` embeds, file embeds (`{F123}`), Diffusion (`rREPO`, commits, source
//! links), and the other applications' monograms. Links to the install
//! itself are rendered as plain links.

use crate::blocks::code::Code;
use crate::blocks::default::Paragraph;
use crate::blocks::frontmatter::ReviewHelperFrontmatter;
use crate::blocks::header::Header;
use crate::blocks::html_table::HtmlTable;
use crate::blocks::interpreter::InterpreterBlock;
use crate::blocks::list::List;
use crate::blocks::literal::Literal;
use crate::blocks::note::Note;
use crate::blocks::quoted::{Quotes, Reply};
use crate::blocks::rule::HorizontalRule;
use crate::blocks::table::SimpleTable;
use crate::engine::{Config, Engine};
use crate::rule::{BlockRule, InlineRule};
use crate::rules::anchor::Anchor;
use crate::rules::bugzilla::BugzillaLink;
use crate::rules::document_link::DocumentLink;
use crate::rules::escape::EscapeRemarkup;
use crate::rules::hyperlink::Hyperlink;
use crate::rules::mention::Mention;
use crate::rules::monospace::Monospace;
use crate::rules::object::ObjectRule;
use crate::rules::simple::{Bold, Del, Highlight, Italic, Underline};

/// `getMarkupEngineDefaultConfiguration()`: line breaks are kept.
pub fn default_config() -> Config {
    Config {
        preserve_linebreaks: true,
        ignored_object_names: Some(r"/^(Q|V|M|P)\d$/".to_owned()),
        ..Config::default()
    }
}

/// `newMarkupEngine()`.
pub fn engine(config: Config) -> Engine {
    let rules: Vec<Box<dyn InlineRule>> = vec![
        Box::new(EscapeRemarkup),
        Box::new(Monospace),
        Box::new(DocumentLink),
        Box::new(BugzillaLink),
        Box::new(ObjectRule::differential()),
        Box::new(ObjectRule::project()),
        Box::new(Mention),
        Box::new(Hyperlink::default()),
        Box::new(Bold),
        Box::new(Italic),
        Box::new(Del),
        Box::new(Underline),
        Box::new(Highlight),
        Box::new(Anchor),
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
        Box::new(InterpreterBlock::default()),
        Box::new(Paragraph),
        Box::new(ReviewHelperFrontmatter),
    ];
    Engine::new(config, blocks, rules)
}

/// The scheme and host of `bugzilla.url`, which links to bugs replace the
/// path of (`PhutilURI::setPath()`).
pub fn bugzilla_origin(url: &str) -> String {
    String::from_utf8_lossy(&crate::uri::origin(url.as_bytes())).into_owned()
}
