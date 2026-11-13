//! `PhabricatorObjectRemarkupRule`: references to objects by monogram
//! (`D123`, `D123#anchor`) and embeds (`{D123}`). `ProjectRemarkupRule`
//! (`#project`) is one with a different name pattern and rendering.

use pcre2::bytes::Regex;

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::objects::{ObjectHandle, ObjectMention, ReferenceKind};
use crate::preg;
use crate::rule::{self, InlineRule};

/// `getObjectIDPattern()` of most rules.
pub const NUMERIC_ID: &str = r"[1-9]\d*";

/// `ProjectRemarkupRule::getObjectIDPattern()`: project slugs, which may
/// hold dots but not end with one.
pub const PROJECT_ID: &str =
    r#"[^.\s?!,:;{}#\(\)"'\*/~]+(?:[^\s?!,:;{}#\(\)"'\*/~]*[^.\s?!,:;{}#\(\)"'\*/~]+)*"#;

/// How a rule renders its references in HTML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// An object tag with the monogram (`PHUITagView::TYPE_OBJECT`).
    Object,
    /// The project's tag (`$handle->renderTag()`).
    Project,
}

/// A `PhabricatorObjectRemarkupRule`.
pub struct ObjectRule {
    prefix: &'static str,
    style: Style,
    embed: Regex,
    reference: Regex,
}

impl ObjectRule {
    /// A rule for objects named `prefix` followed by `id_pattern`.
    pub fn new(prefix: &'static str, id_pattern: &str, style: Style) -> Self {
        let quoted = quote(prefix);
        // A prefix starting with a word character must start a word; "#"
        // must not follow one.
        let boundary = if prefix.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
            r"\b"
        } else {
            r"\B"
        };
        Self {
            prefix,
            style,
            embed: preg::compile_unicode(&format!(
                r"\B\{{{quoted}({id_pattern})([,\s](?:[^}}\\]|\\.)*)?\}}\B"
            )),
            reference: preg::compile_unicode(&format!(
                r"(?<![#@-]){boundary}{quoted}({id_pattern})(?:#([-\w\d]+))?(?!\w)"
            )),
        }
    }

    /// `DifferentialRemarkupRule`: `D123`.
    pub fn differential() -> Self {
        Self::new("D", NUMERIC_ID, Style::Object)
    }

    /// `ProjectRemarkupRule`: `#project`.
    pub fn project() -> Self {
        Self::new("#", PROJECT_ID, Style::Project)
    }

    fn mark_up(cx: &mut Context<'_>, mut mention: ObjectMention) -> Vec<u8> {
        if !rule::is_flat_text(&mention.original) || cx.state("toc") {
            return mention.original;
        }
        if cx.is_ignored_object_name(&mention.original) {
            return mention.original;
        }
        mention.token = cx.store_text(Markup::text("x"));
        mention.quote_depth = cx.quote_depth();
        let token = mention.token.clone();
        cx.metadata.objects.push(mention);
        token
    }

    /// `renderObjectRefForAnyMedia()`.
    fn render_reference(
        &self,
        cx: &Context<'_>,
        handle: &ObjectHandle,
        mention: &ObjectMention,
    ) -> Markup {
        let config = cx.config();
        let mut href = if config.uri_full {
            config.full_uri(&handle.uri)
        } else {
            handle.uri.clone()
        };
        let mut text = [self.prefix.as_bytes(), &mention.id].concat();
        if let Some(anchor) = mention
            .anchor
            .as_deref()
            .filter(|anchor| !anchor.is_empty())
        {
            href = [&href[..], b"#", anchor].concat();
            text = [&text[..], b"#", anchor].concat();
        }
        if cx.is_text_mode() {
            return Markup::Text([&text[..], b" <", &config.production_uri(&href), b">"].concat());
        }
        if cx.is_html_mail_mode() {
            return mail_tag(&text, &config.production_uri(&href), handle);
        }
        match self.style {
            Style::Object => object_tag(&text, &href, handle),
            Style::Project => project_tag(handle),
        }
    }

    /// `renderObjectEmbedForAnyMedia()`: the default embed is a tag with
    /// the object's full name.
    fn render_embed(cx: &Context<'_>, handle: &ObjectHandle) -> Markup {
        if cx.is_text_mode() {
            return Markup::Text(
                [
                    &handle.full_name[..],
                    b" <",
                    &cx.config().production_uri(&handle.uri),
                    b">",
                ]
                .concat(),
            );
        }
        if cx.is_html_mail_mode() {
            return mail_tag(
                &handle.full_name,
                &cx.config().production_uri(&handle.uri),
                handle,
            );
        }
        object_tag(&handle.full_name, &handle.uri, handle)
    }
}

/// `preg_quote()`.
fn quote(text: &str) -> String {
    let mut quoted = String::new();
    for character in text.chars() {
        if ".\\+*?[^]$(){}=!<>|:-#/".contains(character) {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted
}

/// A link to an object. The Rust interface styles it; PHP renders a
/// `PHUITagView` with a hovercard.
fn object_tag(text: &[u8], href: &[u8], handle: &ObjectHandle) -> Markup {
    let class: &[u8] = if handle.closed {
        b"remarkup-object remarkup-object-closed"
    } else {
        b"remarkup-object"
    };
    markup::tag(
        "a",
        &[
            ("href", Some(href)),
            ("class", Some(class)),
            ("title", Some(&handle.full_name)),
            ("data-phid", Some(handle.phid.as_bytes())),
        ],
        Content::One(&Markup::text(text)),
    )
}

/// A project tag: its name, linking to its page.
fn project_tag(handle: &ObjectHandle) -> Markup {
    markup::tag(
        "a",
        &[
            ("href", Some(&handle.uri)),
            ("class", Some(b"remarkup-project")),
            ("data-phid", Some(handle.phid.as_bytes())),
        ],
        Content::One(&Markup::text(handle.name.clone())),
    )
}

/// `renderObjectTagForMail()`.
fn mail_tag(text: &[u8], href: &[u8], handle: &ObjectHandle) -> Markup {
    let decoration = if handle.closed {
        "text-decoration: line-through;"
    } else {
        "text-decoration: none;"
    };
    let style = format!(
        "background-color: #e7e7e7;\n          border-color: #e7e7e7;\n          border-radius: 3px;\n          \
         padding: 0 4px;\n          font-weight: bold;\n          color: black;{decoration}"
    );
    markup::tag(
        "a",
        &[("href", Some(href)), ("style", Some(style.as_bytes()))],
        Content::One(&Markup::text(text)),
    )
}

impl InlineRule for ObjectRule {
    fn priority(&self) -> f64 {
        450.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        let text = preg::replace_callback_regex(&self.embed, text.as_bytes(), |groups| {
            Self::mark_up(
                cx,
                ObjectMention {
                    prefix: self.prefix,
                    id: groups.get(1).to_vec(),
                    kind: ReferenceKind::Embed,
                    anchor: None,
                    options: groups.is_set(2).then(|| groups.get(2).to_vec()),
                    original: groups.get(0).to_vec(),
                    token: Vec::new(),
                    quote_depth: 0,
                },
            )
        });
        let text = preg::replace_callback_regex(&self.reference, &text, |groups| {
            Self::mark_up(
                cx,
                ObjectMention {
                    prefix: self.prefix,
                    id: groups.get(1).to_vec(),
                    kind: ReferenceKind::Reference,
                    anchor: groups.is_set(2).then(|| groups.get(2).to_vec()),
                    options: None,
                    original: groups.get(0).to_vec(),
                    token: Vec::new(),
                    quote_depth: 0,
                },
            )
        });
        Markup::Text(text)
    }

    /// `didMarkupText()`: visible objects render, the others stay as
    /// written.
    fn did_markup_text(&self, cx: &mut Context<'_>) {
        let (mine, others): (Vec<_>, Vec<_>) = std::mem::take(&mut cx.metadata.objects)
            .into_iter()
            .partition(|mention| mention.prefix == self.prefix);
        cx.metadata.objects = others;
        for mention in &mine {
            let key = (self.prefix, mention.id.clone());
            let Some(handle) = cx.resolved.objects.get(&key).cloned() else {
                cx.overwrite_stored_text(&mention.token, Markup::text(mention.original.clone()));
                continue;
            };
            if !cx.metadata.mentioned_phids.contains(&handle.phid) {
                cx.metadata.mentioned_phids.push(handle.phid.clone());
            }
            let view = match mention.kind {
                ReferenceKind::Reference => self.render_reference(cx, &handle, mention),
                ReferenceKind::Embed => Self::render_embed(cx, &handle),
            };
            cx.overwrite_stored_text(&mention.token, view);
        }
    }
}
