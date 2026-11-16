//! `PhutilRemarkupEngine`: splits text into blocks, marks each up with its
//! block rule (which applies the inline rules), then lets rules finish what
//! they deferred and expands the stored tokens.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;
use pcre2::bytes::Regex;

use crate::markup::{self, Markup};
use crate::objects::{ObjectMention, Resolved};
use crate::rule::{BlockRule, InlineRule};
use crate::storage::BlockStorage;
use crate::{php, preg};

/// `PhutilRemarkupEngine::MAX_CHILD_DEPTH`.
const MAX_CHILD_DEPTH: usize = 32;

/// How the text is rendered (`PhutilRemarkupEngine::MODE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// HTML for the web.
    Html,
    /// Plain text, for mail and the API.
    Text,
    /// HTML for mail, with inline styles instead of classes.
    HtmlMail,
}

/// Highlights the code of code blocks in a language: the HTML, or `None`
/// to leave it to the built-in highlighters (plain text for most
/// languages). The pages use syntect, where PHP uses Pygments.
#[derive(Clone)]
pub struct Highlighter(pub Arc<HighlightFn>);

/// What a [`Highlighter`] calls: the language and the code, to the HTML.
pub type HighlightFn = dyn Fn(&[u8], &[u8]) -> Option<Markup> + Send + Sync;

impl fmt::Debug for Highlighter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Highlighter")
    }
}

/// The engine settings (`setConfig()` keys).
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// `uri.allowed-protocols`: protocols which are linked.
    pub allowed_protocols: BTreeSet<String>,
    /// `uri.base`: what `[[ /path ]]` links are relative to.
    pub uri_base: Option<String>,
    /// `uri.here`: what `[[ #anchor ]]` links are relative to.
    pub uri_here: Option<String>,
    /// `uri.same-window`: links open in the same window.
    pub same_window: bool,
    /// `preserve-linebreaks`: single newlines become `<br />`.
    pub preserve_linebreaks: bool,
    /// `header.generate-toc`: headers get anchors for a table of contents.
    pub generate_toc: bool,
    /// `phutil.codeblock.language-default`.
    pub default_language: Option<String>,
    /// `phabricator.base-uri`, for `uri.full` links.
    pub base_uri: Option<String>,
    /// `phabricator.production-uri`, for links in text and mail; the base
    /// URI if unset.
    pub production_uri: Option<String>,
    /// `uri.full`: object links are absolute.
    pub uri_full: bool,
    /// `remarkup.ignored-object-names`: a PHP regular expression of names
    /// like "V1" which are not object references.
    pub ignored_object_names: Option<String>,
    /// `bugzilla.url`.
    pub bugzilla_url: Option<String>,
    /// The hosts of the install (`PhabricatorEnv::getSelfURIMap()`:
    /// the base and production URIs and `phabricator.allowed-uris`),
    /// lowercased. Links to them show the objects they point to.
    pub self_hosts: BTreeSet<String>,
    /// `remarkup.enable-embedded-youtube`.
    pub youtube: bool,
    /// The highlighter of code blocks.
    pub highlighter: Option<Highlighter>,
}

impl Config {
    /// `PhabricatorEnv::getProductionURI()`: absolute URIs stay as they
    /// are.
    pub fn production_uri(&self, path: &[u8]) -> Vec<u8> {
        if crate::uri::has_domain(path) {
            return path.to_vec();
        }
        let base = self
            .production_uri
            .as_deref()
            .filter(|uri| !uri.is_empty())
            .or(self.base_uri.as_deref())
            .unwrap_or("");
        [php::rtrim(base.as_bytes(), b"/"), path].concat()
    }

    /// `PhabricatorEnv::isSelfURI()`.
    pub fn is_self_uri(&self, uri: &[u8]) -> bool {
        crate::uri::domain(uri).is_some_and(|host| {
            self.self_hosts
                .contains(String::from_utf8_lossy(&crate::php::utf8_strtolower(&host)).as_ref())
        })
    }

    /// `PhabricatorEnv::isValidRemoteURIForLink()`: an allowed protocol and
    /// a domain.
    pub fn is_valid_remote_uri_for_link(&self, uri: &[u8]) -> bool {
        let Ok(protocol) = crate::uri::protocol(uri) else {
            return false;
        };
        !protocol.is_empty()
            && self
                .allowed_protocols
                .contains(String::from_utf8_lossy(&protocol).as_ref())
            && crate::uri::domain(uri).is_some()
    }

    /// `PhabricatorEnv::getURI()`.
    pub fn full_uri(&self, path: &[u8]) -> Vec<u8> {
        let base = self.base_uri.as_deref().unwrap_or("");
        [php::rtrim(base.as_bytes(), b"/"), path].concat()
    }
}

/// A link found by `PhutilRemarkupHyperlinkRule`, rendered once all text is
/// marked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hyperlink {
    /// Its token in the storage.
    pub token: Vec<u8>,
    /// The URI as written.
    pub uri: Vec<u8>,
    /// How it was written: `<uri>`, `{uri}` or bare.
    pub mode: LinkMode,
}

/// How a hyperlink was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    /// `<http://...>`: linked exactly as written.
    Literal,
    /// `{http://...}`: an embed, not linked unless a handler takes it.
    Embed,
    /// `http://...` in text.
    Bare,
}

/// A `{src}` waiting for its repository
/// (`DiffusionSourceLinkRemarkupRule::KEY_SOURCELINKS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLink {
    /// Its token in the storage.
    pub token: Vec<u8>,
    /// The repository and location, or what is wrong with it.
    pub parsed: crate::rules::source_link::ParsedSource,
}

/// What rules record while marking up (`setTextMetadata()`).
#[derive(Debug, Clone, Default)]
pub struct Metadata {
    /// `PhutilRemarkupHyperlinkRule::KEY_HYPERLINKS`.
    pub hyperlinks: Vec<Hyperlink>,
    /// `PhutilRemarkupHeaderBlockRule::KEY_HEADER_TOC`: anchor, level and
    /// rendered HTML of each header, in order.
    pub toc: Vec<(Vec<u8>, usize, Vec<u8>)>,
    /// `PhabricatorObjectRemarkupRule::KEY_RULE_OBJECT`: object references
    /// waiting to be resolved.
    pub objects: Vec<ObjectMention>,
    /// `PhabricatorMentionRemarkupRule::KEY_RULE_MENTION`: the tokens of
    /// each mentioned username, lowercased.
    pub mentions: IndexMap<Vec<u8>, Vec<Vec<u8>>>,
    /// `KEY_RULE_MENTION_ORIGINAL`: each mention token's username as
    /// written.
    pub mention_originals: HashMap<Vec<u8>, Vec<u8>>,
    /// `KEY_MENTIONED` and `KEY_MENTIONED_OBJECTS`: the PHIDs of the users
    /// and objects the text mentions, in order.
    pub mentioned_phids: Vec<String>,
    /// `PhabricatorHandleRemarkupRule::KEY_RULE_HANDLE`: the tokens of each
    /// `{PHID-...}`.
    pub handles: IndexMap<String, Vec<Vec<u8>>>,
    /// The `{src}` links.
    pub source_links: Vec<SourceLink>,
}

/// What the rules see of the engine while marking up.
pub struct Context<'r> {
    config: &'r Config,
    mode: Mode,
    rules: &'r [Box<dyn InlineRule>],
    storage: BlockStorage,
    states: HashMap<&'static str, usize>,
    /// What rules record.
    pub metadata: Metadata,
    quote_depth: usize,
    /// The objects references point to, once resolved.
    pub resolved: Resolved,
    ignored_object_names: Option<Regex>,
}

impl<'r> Context<'r> {
    /// The engine settings.
    pub fn config(&self) -> &'r Config {
        self.config
    }

    /// `isTextMode()`.
    pub fn is_text_mode(&self) -> bool {
        self.mode == Mode::Text
    }

    /// `isHTMLMailMode()`.
    pub fn is_html_mail_mode(&self) -> bool {
        self.mode == Mode::HtmlMail
    }

    /// `isAnchorMode()`: rendering the text of a table of contents entry.
    pub fn is_anchor_mode(&self) -> bool {
        self.state("toc")
    }

    /// `pushState()`.
    pub fn push_state(&mut self, state: &'static str) {
        *self.states.entry(state).or_default() += 1;
    }

    /// `popState()`.
    ///
    /// # Panics
    ///
    /// If the state was not pushed.
    pub fn pop_state(&mut self, state: &'static str) {
        let count = self
            .states
            .get_mut(state)
            .unwrap_or_else(|| panic!("state {state} popped more than pushed"));
        *count -= 1;
        if *count == 0 {
            self.states.remove(state);
        }
    }

    /// `getState()`.
    pub fn state(&self, state: &str) -> bool {
        self.states.contains_key(state)
    }

    /// Whether `remarkup.ignored-object-names` matches this text.
    pub fn is_ignored_object_name(&self, text: &[u8]) -> bool {
        self.ignored_object_names
            .as_ref()
            .is_some_and(|regex| regex.is_match(text).unwrap_or(false))
    }

    /// `getQuoteDepth()`.
    pub fn quote_depth(&self) -> usize {
        self.quote_depth
    }

    /// `setQuoteDepth()`.
    pub fn set_quote_depth(&mut self, depth: usize) {
        self.quote_depth = depth;
    }

    /// `storeText()`: in text mode, the value is kept as is.
    pub fn store_text(&mut self, value: Markup) -> Vec<u8> {
        let value = if self.is_text_mode() {
            value.into_safe()
        } else {
            value
        };
        self.storage.store(value)
    }

    /// `overwriteStoredText()`.
    pub fn overwrite_stored_text(&mut self, token: &[u8], value: Markup) {
        let value = if self.is_text_mode() {
            value.into_safe()
        } else {
            value
        };
        self.storage.overwrite(token, value);
    }

    /// `restoreText()`.
    pub fn restore_text(&self, value: &Markup) -> Markup {
        self.storage.restore(value, self.is_text_mode())
    }

    /// `PhutilRemarkupBlockRule::applyRules()`.
    pub fn apply_rules(&mut self, text: Markup) -> Markup {
        let rules = self.rules;
        rules.iter().fold(text, |text, rule| rule.apply(self, text))
    }

    fn flatten(&self, outputs: &[Option<Markup>]) -> Markup {
        if self.is_text_mode() {
            let pieces: Vec<&[u8]> = outputs
                .iter()
                .map(|output| output.as_ref().map_or(&b""[..], Markup::as_bytes))
                .collect();
            let mut text = php::implode(b"\n\n", &pieces);
            text.push(b'\n');
            Markup::Text(text)
        } else {
            markup::implode_html(&Markup::text("\n\n"), outputs.iter().map(Option::as_ref))
        }
    }
}

/// A Remarkup engine: rules and settings.
pub struct Engine {
    config: Config,
    blocks: Vec<Box<dyn BlockRule>>,
    rules: Vec<Box<dyn InlineRule>>,
}

/// Marked-up text whose object references are not resolved yet.
pub struct Prepared<'e> {
    context: Context<'e>,
    output: Markup,
}

impl Prepared<'_> {
    /// The object references to resolve: rule prefix and ID as written,
    /// from references and from links to objects (`https://phab/D123`).
    pub fn object_references(&self) -> Vec<(&'static str, Vec<u8>)> {
        let mut references: Vec<(&'static str, Vec<u8>)> = Vec::new();
        let linked = self
            .self_links()
            .filter_map(crate::rules::self_link::object_name)
            .filter_map(|name| crate::objects::parse_object_name(&name))
            .filter(|(prefix, _)| *prefix != "@");
        let written = self
            .context
            .metadata
            .objects
            .iter()
            .map(|mention| (mention.prefix, mention.id.clone()));
        for reference in written.chain(linked) {
            if !references.contains(&reference) {
                references.push(reference);
            }
        }
        references
    }

    /// The links to the install (literal `<uri>` links are rendered as
    /// they are).
    fn self_links(&self) -> impl Iterator<Item = &[u8]> {
        let config = self.context.config;
        self.context
            .metadata
            .hyperlinks
            .iter()
            .filter(|link| link.mode != LinkMode::Literal)
            .map(|link| link.uri.as_slice())
            .filter(move |link| {
                config.is_self_uri(link)
                    && matches!(
                        crate::uri::protocol(link).as_deref(),
                        Ok(b"http" | b"https")
                    )
            })
    }

    /// The mentioned usernames, lowercased, from mentions and from links
    /// to profiles (`https://phab/@alice`).
    pub fn mentioned_usernames(&self) -> Vec<Vec<u8>> {
        let mut usernames: Vec<Vec<u8>> = self.context.metadata.mentions.keys().cloned().collect();
        for link in self.self_links() {
            if let Some(("@", username)) = crate::rules::self_link::object_name(link)
                .and_then(|name| crate::objects::parse_object_name(&name))
            {
                let username = username.to_ascii_lowercase();
                if !usernames.contains(&username) {
                    usernames.push(username);
                }
            }
        }
        usernames
    }

    /// The PHIDs of `{PHID-...}` handles.
    pub fn handle_phids(&self) -> Vec<String> {
        self.context.metadata.handles.keys().cloned().collect()
    }

    /// The repositories of source links, by identifier as written: a
    /// callsign, an ID or a short name.
    pub fn repository_identifiers(&self) -> Vec<Vec<u8>> {
        let mut identifiers: Vec<Vec<u8>> = Vec::new();
        let written = self
            .context
            .metadata
            .source_links
            .iter()
            .filter_map(|link| link.parsed.as_ref().ok())
            .map(|(identifier, _)| identifier.clone());
        let linked = self
            .self_links()
            .filter_map(crate::rules::self_link::browse_target)
            .map(|(identifier, _)| identifier);
        for identifier in written.chain(linked) {
            if !identifiers.contains(&identifier) {
                identifiers.push(identifier);
            }
        }
        identifiers
    }
}

/// What [`Engine::markup()`] produces.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// The HTML (safe) or text.
    pub output: Markup,
    /// What rules recorded.
    pub metadata: Metadata,
}

/// A block of lines and the rule which matched it.
struct Block {
    start: usize,
    num_lines: usize,
    rule: usize,
    empty: bool,
    merge: bool,
    text: Vec<u8>,
    children: Vec<Self>,
}

impl Engine {
    /// An engine with these rules, sorted like `setBlockRules()` (by
    /// priority, then class name) and `setMarkupRules()` (by priority,
    /// keeping the given order for ties).
    pub fn new(
        config: Config,
        mut blocks: Vec<Box<dyn BlockRule>>,
        mut rules: Vec<Box<dyn InlineRule>>,
    ) -> Self {
        blocks.sort_by(|a, b| {
            a.priority()
                .cmp(&b.priority())
                .then_with(|| a.class_name().cmp(b.class_name()))
        });
        rules.sort_by(|a, b| a.priority().total_cmp(&b.priority()));
        Self {
            config,
            blocks,
            rules,
        }
    }

    /// The engine settings.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// `markupText()` for text without object references to resolve:
    /// [`Engine::prepare()`], then [`Engine::finish()`] with nothing
    /// resolved (references render as written).
    pub fn markup(&self, text: &[u8], mode: Mode) -> Rendered {
        self.finish(self.prepare(text, mode), Resolved::default())
    }

    /// `preprocessText()`: marks the blocks up, leaving tokens for what
    /// needs the database.
    pub fn prepare(&self, text: &[u8], mode: Mode) -> Prepared<'_> {
        let mut cx = Context {
            config: &self.config,
            mode,
            rules: &self.rules,
            storage: BlockStorage::default(),
            states: HashMap::new(),
            metadata: Metadata::default(),
            quote_depth: 0,
            resolved: Resolved::default(),
            ignored_object_names: self
                .config
                .ignored_object_names
                .as_deref()
                .and_then(|regex| preg::compile_php(php::trim(regex.as_bytes()))),
        };

        let blocks = self.split_into_blocks(text, 0);
        let outputs: Vec<Option<Markup>> = blocks
            .iter()
            .map(|block| self.markup_block(&mut cx, block))
            .collect();
        let output = cx.flatten(&outputs);
        Prepared {
            context: cx,
            output,
        }
    }

    /// `postprocessText()`: renders the deferred tokens, objects with what
    /// `resolved` says the viewer may see, and expands the storage.
    pub fn finish(&self, prepared: Prepared<'_>, resolved: Resolved) -> Rendered {
        let Prepared {
            context: mut cx,
            output,
        } = prepared;
        cx.resolved = resolved;
        for block in &self.blocks {
            block.postprocess(&mut cx);
        }
        for rule in &self.rules {
            rule.did_markup_text(&mut cx);
        }

        Rendered {
            output: cx.restore_text(&output),
            metadata: cx.metadata,
        }
    }

    fn markup_block(&self, cx: &mut Context<'_>, block: &Block) -> Option<Markup> {
        let rule = &self.blocks[block.rule];
        rule.will_markup_child_blocks(cx);
        let children: Vec<Option<Markup>> = block
            .children
            .iter()
            .map(|child| self.markup_block(cx, child))
            .collect();
        rule.did_markup_child_blocks(cx);
        let children = if children.is_empty() {
            None
        } else {
            Some(cx.flatten(&children))
        };
        rule.markup_text(cx, &block.text, children)
    }

    /// `splitTextIntoBlocks()`.
    fn split_into_blocks(&self, text: &[u8], depth: usize) -> Vec<Block> {
        let lines = php::split_lines(text);
        let mut blocks: Vec<Block> = Vec::new();
        let mut cursor = 0;
        while cursor < lines.len() {
            let (rule, num_lines) = self
                .blocks
                .iter()
                .enumerate()
                .find_map(|(index, block)| {
                    let count = block.matching_line_count(&lines, cursor);
                    (count > 0).then_some((index, count))
                })
                .expect("the default block rule matches every line");
            let current = Block {
                start: cursor,
                num_lines,
                rule,
                empty: is_empty_block(&lines, cursor, num_lines),
                merge: self.blocks[rule].is_default(),
                text: Vec::new(),
                children: Vec::new(),
            };
            match blocks.last_mut() {
                Some(last) if should_merge(&lines, last, &current) => {
                    last.num_lines += current.num_lines;
                    last.empty = last.empty && current.empty;
                }
                _ => blocks.push(current),
            }
            cursor += num_lines;
        }

        for block in &mut blocks {
            block.text = lines[block.start..block.start + block.num_lines].concat();
        }

        if depth < MAX_CHILD_DEPTH {
            for block in &mut blocks {
                let rule = &self.blocks[block.rule];
                if !rule.supports_child_blocks() {
                    continue;
                }
                let (parent, child) = rule.extract_child_text(&block.text);
                block.text = parent;
                block.children = self.split_into_blocks(&child, depth + 1);
            }
        }

        blocks
    }
}

/// `shouldMergeParagraphBlocks()`.
fn should_merge(lines: &[&[u8]], last: &Block, current: &Block) -> bool {
    if !last.merge || !current.merge {
        return false;
    }
    if last.empty || current.empty {
        return true;
    }
    let tail = lines[current.start - 1];
    let head = lines[current.start];
    !php::is_blank(tail) && !php::is_blank(head)
}

/// `isEmptyBlock()`.
fn is_empty_block(lines: &[&[u8]], start: usize, num_lines: usize) -> bool {
    lines[start..start + num_lines]
        .iter()
        .all(|line| php::is_blank(line))
}
