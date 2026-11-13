//! `PhutilHTMLParser` and `PhutilDOMNode`: a lenient HTML parser which keeps
//! the raw text of every node, so a table that isn't one can be shown as
//! written.

use crate::php;

/// A node: text, or a tag with children.
#[derive(Debug, Clone, Default)]
struct Node {
    content: Option<Vec<u8>>,
    tag: Option<Vec<u8>>,
    children: Vec<usize>,
    parent: Option<usize>,
    raw_head: Vec<u8>,
    raw_tail: Vec<u8>,
}

/// A parsed document: nodes in an arena, the root first.
#[derive(Debug, Clone)]
pub struct Document {
    nodes: Vec<Node>,
}

/// A child picked by [`Document::select_children_with_tags()`].
#[derive(Debug, Clone)]
pub enum Selected {
    /// Text, including the raw text of tags which were not selected.
    Content(Vec<u8>),
    /// A selected tag.
    Element(usize),
}

/// The root node.
pub const ROOT: usize = 0;

impl Document {
    /// `PhutilHTMLParser::parseDocument()`.
    pub fn parse(corpus: &[u8]) -> Self {
        // Cut the corpus into text and "<...>" segments.
        let mut segments: Vec<(bool, usize, usize)> = Vec::new();
        let mut segment_pos = 0;
        let mut in_tag = false;
        for (index, &byte) in corpus.iter().enumerate() {
            if in_tag && byte == b'>' {
                segments.push((true, segment_pos, index + 1));
                segment_pos = index + 1;
                in_tag = false;
                continue;
            }
            if byte == b'<' {
                segments.push((false, segment_pos, index));
                segment_pos = index;
                in_tag = true;
            }
        }
        segments.push((false, segment_pos, corpus.len()));

        let mut document = Self {
            nodes: vec![Node::default()],
        };
        let mut cursor = ROOT;
        for (is_tag, pos, end) in segments {
            let len = end - pos;
            let (slice_pos, slice_len) = if is_tag {
                (pos + 1, len.saturating_sub(2))
            } else {
                (pos, len)
            };
            let content = if slice_pos < corpus.len() && slice_len > 0 {
                &corpus[slice_pos..slice_pos + slice_len]
            } else {
                &[][..]
            };
            if is_tag && document.tag_node(&mut cursor, content) {
                continue;
            }
            let content = if is_tag {
                [b"<", content, b">"].concat()
            } else {
                content.to_vec()
            };
            document.append(
                cursor,
                Node {
                    content: Some(content.clone()),
                    raw_head: content,
                    ..Node::default()
                },
            );
        }
        document.merge_content(ROOT);
        document
    }

    fn append(&mut self, parent: usize, mut node: Node) -> usize {
        node.parent = Some(parent);
        self.nodes.push(node);
        let index = self.nodes.len() - 1;
        self.nodes[parent].children.push(index);
        index
    }

    /// `newTagDOMNode()`: handles a tag, or returns `false` if it is to be
    /// kept as text.
    fn tag_node(&mut self, cursor: &mut usize, raw: &[u8]) -> bool {
        let mut content = php::trim(raw);
        let is_close = content.first() == Some(&b'/');
        if is_close {
            content = php::trim(&content[1..]);
        }
        let self_close = content.last() == Some(&b'/');
        if self_close {
            content = php::trim(&content[..content.len() - 1]);
        }
        if self_close && is_close {
            return false;
        }
        let split = content.iter().position(|byte| {
            php::WHITESPACE[..4].contains(byte) || *byte == b'\x0C' || *byte == b'\x0B'
        });
        let (name, attributes) = match split {
            Some(at) => {
                let rest = &content[at..];
                let start = rest
                    .iter()
                    .position(|byte| !(byte.is_ascii_whitespace() || *byte == b'\x0B'))
                    .unwrap_or(rest.len());
                (&content[..at], &rest[start..])
            }
            None => (content, &[][..]),
        };
        if name.is_empty() || (is_close && !attributes.is_empty()) {
            return false;
        }
        let name = php::utf8_strtolower(name);
        let raw_tag = [b"<", raw, b">"].concat();

        if is_close {
            let mut node = Some(*cursor);
            while let Some(index) = node {
                if self.nodes[index].tag.as_deref() == Some(name.as_slice()) {
                    self.nodes[index].raw_tail = raw_tag;
                    *cursor = self.nodes[index].parent.unwrap_or(ROOT);
                    return true;
                }
                node = self.nodes[index].parent;
            }
            return false;
        }

        if !attributes.is_empty() && !attributes_parse(attributes) {
            return false;
        }
        let index = self.append(
            *cursor,
            Node {
                tag: Some(name),
                raw_head: raw_tag,
                ..Node::default()
            },
        );
        if !self_close {
            *cursor = index;
        }
        true
    }

    /// `newRawString()`.
    pub fn raw_string(&self, index: usize) -> Vec<u8> {
        let node = &self.nodes[index];
        let mut raw = node.raw_head.clone();
        for &child in &node.children {
            raw.extend_from_slice(&self.raw_string(child));
        }
        raw.extend_from_slice(&node.raw_tail);
        raw
    }

    /// The tag name of an element.
    pub fn tag_name(&self, index: usize) -> &[u8] {
        self.nodes[index].tag.as_deref().unwrap_or_default()
    }

    /// `selectChildrenWithTags()`: the children, with tags not listed
    /// turned into text, and consecutive text merged.
    pub fn select_children_with_tags(&self, index: usize, tags: &[&[u8]]) -> Vec<Selected> {
        let picked = self.nodes[index].children.iter().map(|&child| {
            let node = &self.nodes[child];
            match (&node.content, &node.tag) {
                (Some(_), _) => Selected::Content(node.raw_head.clone()),
                (None, Some(tag)) if tags.contains(&tag.as_slice()) => Selected::Element(child),
                _ => Selected::Content(self.raw_string(child)),
            }
        });
        merge_selected(picked)
    }

    /// `newRawContentString()`: the raw text of the children.
    pub fn raw_content_string(&self, index: usize) -> Vec<u8> {
        match self
            .select_children_with_tags(index, &[])
            .into_iter()
            .next()
        {
            Some(Selected::Content(text)) => text,
            Some(Selected::Element(child)) => self.raw_string(child),
            None => Vec::new(),
        }
    }

    /// `mergeContent()`: joins consecutive text children, recursively.
    fn merge_content(&mut self, index: usize) {
        let children = std::mem::take(&mut self.nodes[index].children);
        let mut merged = Vec::new();
        let mut text: Option<Vec<u8>> = None;
        for child in children {
            if self.nodes[child].content.is_some() {
                text.get_or_insert_with(Vec::new)
                    .extend_from_slice(&self.raw_string(child));
                continue;
            }
            if let Some(text) = text.take().filter(|text| !text.is_empty()) {
                merged.push(self.new_content(index, text));
            }
            merged.push(child);
        }
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            merged.push(self.new_content(index, text));
        }
        let count = merged.len();
        self.nodes[index].children = merged;
        for position in 0..count {
            let child = self.nodes[index].children[position];
            self.nodes[child].parent = Some(index);
            self.merge_content(child);
        }
    }

    fn new_content(&mut self, parent: usize, text: Vec<u8>) -> usize {
        self.nodes.push(Node {
            content: Some(text.clone()),
            raw_head: text,
            parent: Some(parent),
            ..Node::default()
        });
        self.nodes.len() - 1
    }
}

fn merge_selected(nodes: impl Iterator<Item = Selected>) -> Vec<Selected> {
    let mut results = Vec::new();
    let mut text: Option<Vec<u8>> = None;
    for node in nodes {
        match node {
            Selected::Content(part) => text.get_or_insert_with(Vec::new).extend_from_slice(&part),
            element @ Selected::Element(_) => {
                if let Some(text) = text.take().filter(|text| !text.is_empty()) {
                    results.push(Selected::Content(text));
                }
                results.push(element);
            }
        }
    }
    if let Some(text) = text.filter(|text| !text.is_empty()) {
        results.push(Selected::Content(text));
    }
    results
}

/// Whether `parseAttributes()` accepts the attributes (it returns `null`
/// for malformed ones, which keeps the tag as text).
fn attributes_parse(attributes: &[u8]) -> bool {
    #[derive(PartialEq)]
    enum State {
        Key,
        Name,
        Equals,
        Value,
        Quoted,
        Unquoted,
    }
    let is_space = |byte: u8| matches!(byte, b' ' | b'\n' | b'\t' | b'\r');
    let mut state = State::Key;
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut name_pos = 0;
    let mut name: Vec<u8> = Vec::new();
    for (index, &byte) in attributes.iter().enumerate() {
        match state {
            State::Key => {
                if is_space(byte) {
                    continue;
                }
                if byte == b'=' || byte == b'"' {
                    return false;
                }
                name_pos = index;
                state = State::Name;
            }
            State::Name => {
                if byte == b'=' || is_space(byte) {
                    name = php::utf8_strtolower(&attributes[name_pos..index]);
                    if seen.contains(&name) {
                        return false;
                    }
                }
                if byte == b'=' {
                    state = State::Value;
                } else if is_space(byte) {
                    state = State::Equals;
                }
            }
            State::Equals => {
                if is_space(byte) {
                    continue;
                }
                if byte == b'=' {
                    state = State::Value;
                    continue;
                }
                seen.push(name.clone());
                name_pos = index;
                state = State::Name;
            }
            State::Value => {
                if is_space(byte) {
                    continue;
                }
                if byte == b'=' {
                    return false;
                }
                state = if byte == b'"' {
                    State::Quoted
                } else {
                    State::Unquoted
                };
            }
            State::Quoted => {
                if byte == b'"' {
                    seen.push(name.clone());
                    state = State::Key;
                }
            }
            State::Unquoted => {
                if is_space(byte) {
                    seen.push(name.clone());
                    state = State::Key;
                }
            }
        }
    }
    match state {
        State::Name => !seen.contains(&attributes[name_pos..].to_vec()),
        State::Equals | State::Value => !seen.contains(&name),
        State::Key | State::Quoted | State::Unquoted => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_text_is_kept() {
        let text = b"<table><tr><td>a <b>b</b></td></tr></table> x";
        let document = Document::parse(text);
        let top = document.select_children_with_tags(ROOT, &[b"table"]);
        let Selected::Element(table) = top[0] else {
            panic!("no table");
        };
        assert_eq!(
            document.raw_string(table),
            b"<table><tr><td>a <b>b</b></td></tr></table>"
        );
        assert!(matches!(&top[1], Selected::Content(text) if text == b" x"));
        let rows = document.select_children_with_tags(table, &[b"tr"]);
        let Selected::Element(row) = rows[0] else {
            panic!("no row");
        };
        let cells = document.select_children_with_tags(row, &[b"td"]);
        let Selected::Element(cell) = cells[0] else {
            panic!("no cell");
        };
        assert_eq!(document.raw_content_string(cell), b"a <b>b</b>");
    }

    #[test]
    fn malformed_attributes_keep_the_tag_as_text() {
        assert!(attributes_parse(b"class=\"x\" id=y"));
        assert!(!attributes_parse(b"=x"));
        assert!(!attributes_parse(b"a=1 a=2"));
    }
}
