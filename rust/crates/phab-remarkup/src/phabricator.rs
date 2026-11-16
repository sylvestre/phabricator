//! The engine Phabricator renders comments and summaries with
//! (`PhabricatorMarkupEngine::newMarkupEngine()`).
//!
//! The applications Mozilla doesn't use are not ported (`RUST_PORT_PLAN.md`
//! §9.3), so their monograms stay text: calendar events (`E1`),
//! credentials (`K1`), builds (`B1`) and dashboard panels (`W1`).

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
use crate::rules::config::ConfigLink;
use crate::rules::document_link::DocumentLink;
use crate::rules::emoji::Emoji;
use crate::rules::escape::EscapeRemarkup;
use crate::rules::eval::Eval;
use crate::rules::handle::Handle;
use crate::rules::hyperlink::Hyperlink;
use crate::rules::icon::Icon;
use crate::rules::image::Image;
use crate::rules::keyboard::Keyboard;
use crate::rules::mention::Mention;
use crate::rules::monospace::Monospace;
use crate::rules::navigation::Navigation;
use crate::rules::object::ObjectRule;
use crate::rules::self_link::{SelfObjects, SelfSources};
use crate::rules::simple::{Bold, Del, Highlight, Italic, Underline};
use crate::rules::source_link::SourceLinkRule;
use crate::rules::youtube::Youtube;

/// `getMarkupEngineDefaultConfiguration()`: line breaks are kept.
pub fn default_config() -> Config {
    Config {
        preserve_linebreaks: true,
        ignored_object_names: Some(r"/^(Q|V|M|P)\d$/".to_owned()),
        ..Config::default()
    }
}

/// `newMarkupEngine()`, with the rules in PHP's order (the order of the
/// installed applications for theirs), which breaks ties of priority.
pub fn engine(config: Config) -> Engine {
    let mut rules: Vec<Box<dyn InlineRule>> = vec![
        Box::new(EscapeRemarkup),
        Box::new(Eval),
        Box::new(Monospace),
        Box::new(DocumentLink),
        Box::new(Navigation),
        Box::new(Keyboard),
        Box::new(ConfigLink),
    ];
    if config.youtube {
        rules.push(Box::new(Youtube));
    }
    rules.extend::<[Box<dyn InlineRule>; _]>([
        Box::new(BugzillaLink),
        Box::new(Icon),
        Box::new(Emoji),
        Box::new(Handle),
        // The applications' rules; their names never overlap, so their
        // order (getApplicationOrder()) does not matter.
        Box::new(ObjectRule::differential()),
        Box::new(ObjectRule::commit()),
        Box::new(ObjectRule::repository()),
        Box::new(ObjectRule::repository_by_id()),
        Box::new(SourceLinkRule),
        Box::new(ObjectRule::file()),
        Box::new(Image),
        Box::new(ObjectRule::herald()),
        Box::new(Mention),
        Box::new(ObjectRule::project()),
        Box::new(ObjectRule::spaces()),
        Box::new(Hyperlink::with_handlers(vec![
            Box::new(SelfSources),
            Box::new(SelfObjects),
        ])),
        Box::new(Bold),
        Box::new(Italic),
        Box::new(Del),
        Box::new(Underline),
        Box::new(Highlight),
        Box::new(Anchor),
    ]);
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
