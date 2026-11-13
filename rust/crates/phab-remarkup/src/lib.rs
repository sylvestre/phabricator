//! A port of the Remarkup engine (`PhutilRemarkupEngine`, its block and
//! inline rules), for the Rust interface (`RUST_PORT_PLAN.md` §9, W1).
//!
//! The engine works on bytes like PHP, with the same regular expression
//! library (PCRE2), and keeps PHP's distinction between plain strings and
//! safe HTML ([`Markup`]), on which the rules depend. The test corpus of
//! `PhutilRemarkupEngineTestCase` runs against it.

pub mod blocks;
pub mod engine;
mod highlight;
mod html;
mod lexer;
pub mod markup;
mod options;
mod php;
pub mod preg;
pub mod rule;
pub mod rules;
pub mod storage;
mod uri;

pub use engine::{Config, Context, Engine, Mode, Rendered};
pub use markup::Markup;
pub use options::OptionValue;
pub use rule::{BlockRule, InlineRule};
