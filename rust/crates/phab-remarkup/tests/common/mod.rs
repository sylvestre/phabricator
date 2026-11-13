//! The engine of `PhutilRemarkupEngineTestCase`, shared by the tests.

use indexmap::IndexMap;
use phab_remarkup::blocks::code::Code;
use phab_remarkup::blocks::default::Paragraph;
use phab_remarkup::blocks::header::Header;
use phab_remarkup::blocks::html_table::HtmlTable;
use phab_remarkup::blocks::interpreter::{Interpreter, InterpreterBlock};
use phab_remarkup::blocks::list::List;
use phab_remarkup::blocks::literal::Literal;
use phab_remarkup::blocks::note::Note;
use phab_remarkup::blocks::quoted::{Quotes, Reply};
use phab_remarkup::blocks::rule::HorizontalRule;
use phab_remarkup::blocks::table::SimpleTable;
use phab_remarkup::rules::document_link::DocumentLink;
use phab_remarkup::rules::escape::EscapeRemarkup;
use phab_remarkup::rules::hyperlink::Hyperlink;
use phab_remarkup::rules::monospace::Monospace;
use phab_remarkup::rules::simple::{Bold, Del, Highlight, Italic, Underline};
use phab_remarkup::{BlockRule, Config, Context, Engine, InlineRule, Markup, OptionValue};

/// `PhutilRemarkupTestInterpreterRule`.
struct TestInterpreter;

impl Interpreter for TestInterpreter {
    fn name(&self) -> &'static str {
        "phutil_test_block_interpreter"
    }

    fn markup_content(
        &self,
        _context: &mut Context<'_>,
        content: &[u8],
        options: &IndexMap<Vec<u8>, OptionValue>,
    ) -> Markup {
        let argv: Vec<Vec<u8>> = options
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    OptionValue::Flag => b"1".to_vec(),
                    OptionValue::Value(value) => value.clone(),
                };
                [key.as_slice(), b"=", &value].concat()
            })
            .collect();
        let mut output = b"Content: (".to_vec();
        output.extend_from_slice(content);
        output.extend_from_slice(b")\nArgv: (");
        output.extend_from_slice(&argv.join(&b'&'));
        output.push(b')');
        Markup::Text(output)
    }
}

/// `buildNewTestEngine()`.
pub fn engine(config: Config) -> Engine {
    let rules: Vec<Box<dyn InlineRule>> = vec![
        Box::new(EscapeRemarkup),
        Box::new(Monospace),
        Box::new(DocumentLink),
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
        Box::new(Header),
        Box::new(HorizontalRule),
        Box::new(Code),
        Box::new(Literal),
        Box::new(Note),
        Box::new(HtmlTable),
        Box::new(SimpleTable),
        Box::new(Paragraph),
        Box::new(List),
        Box::new(InterpreterBlock::new(vec![Box::new(TestInterpreter)])),
    ];
    Engine::new(config, blocks, rules)
}

/// The configuration of the test engine.
pub fn config() -> Config {
    Config {
        allowed_protocols: ["http", "mailto", "tel"].map(str::to_owned).into(),
        ..Config::default()
    }
}
