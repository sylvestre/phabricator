//! `PhabricatorNavigationRemarkupRule`: `{nav Home > Settings}` shows a
//! sequence of steps in the interface.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::options::{self, OptionValue};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::{self, InlineRule};
use crate::rules::icon;

static NAVIGATION: Pattern = Pattern::new(r"(?m)\{nav\b((?:[^}\\]+|\\.)*)\}");

/// `PhabricatorNavigationRemarkupRule`.
pub struct Navigation;

/// An attribute of [`markup::tag()`].
type Attribute<'a> = (&'a str, Option<&'a [u8]>);

/// One step of the sequence.
struct Item {
    name: Vec<u8>,
    instructions: bool,
    href: Option<Vec<u8>>,
    icon: Option<Vec<u8>>,
}

fn parse_item(element: &[u8]) -> Item {
    if !element.contains(&b'=') {
        return Item {
            name: php::trim(element).to_vec(),
            instructions: false,
            href: None,
            icon: None,
        };
    }
    let parsed = options::parse(element);
    let value = |key: &[u8]| parsed.get(key).map(OptionValue::to_php_string);
    Item {
        name: value(b"name").unwrap_or_default(),
        instructions: value(b"type").as_deref() == Some(b"instructions"),
        href: value(b"href"),
        icon: value(b"icon").filter(|icon| !icon.is_empty() && icon != b"0"),
    }
}

fn render_item(cx: &Context<'_>, item: &Item) -> Markup {
    let name = Markup::text(item.name.clone());
    let name = if item.instructions {
        markup::tag("em", &[], Content::One(&name))
    } else {
        name
    };
    let icon = item
        .icon
        .as_deref()
        .and_then(|icon| icon::is_icon(icon).then(|| icon::render(icon, None, false)));
    let content = [icon, Some(name)];
    let class: &[u8] = if item.instructions {
        b"remarkup-nav-item remarkup-nav-instructions"
    } else {
        b"remarkup-nav-item"
    };
    match item
        .href
        .as_deref()
        .filter(|href| cx.config().is_valid_remote_uri_for_link(href))
    {
        Some(href) => markup::tag(
            "a",
            &[
                ("href", Some(href)),
                ("class", Some(class)),
                ("target", Some(b"_blank")),
                ("rel", Some(b"noreferrer")),
            ],
            Content::List(&content),
        ),
        None => markup::tag("span", &[("class", Some(class))], Content::List(&content)),
    }
}

impl InlineRule for Navigation {
    fn priority(&self) -> f64 {
        200.0
    }

    fn apply(&self, cx: &mut Context<'_>, text: Markup) -> Markup {
        Markup::Text(preg::replace_callback(
            &NAVIGATION,
            text.as_bytes(),
            |groups| {
                if !rule::is_flat_text(groups.get(0)) {
                    return groups.get(0).to_vec();
                }
                let elements = php::ltrim(groups.get(1), b", \n");
                let sequence: Vec<Item> = elements
                    .split(|byte| *byte == b'>')
                    .map(parse_item)
                    .collect();
                if cx.is_text_mode() {
                    let names: Vec<&[u8]> = sequence.iter().map(|item| &item.name[..]).collect();
                    return php::implode(b" > ", &names);
                }
                let (arrow, nav): (&[Attribute<'_>], &[Attribute<'_>]) = if cx.is_html_mail_mode() {
                    (&[("style", Some(b"color: #92969D;"))], &[])
                } else {
                    (
                        &[("class", Some(b"remarkup-nav-sequence-arrow"))],
                        &[("class", Some(b"remarkup-nav-sequence"))],
                    )
                };
                let joiner = markup::tag("span", arrow, Content::One(&Markup::text(" \u{2192} ")));
                let items: Vec<Option<Markup>> = sequence
                    .iter()
                    .map(|item| Some(render_item(cx, item)))
                    .collect();
                let items = markup::implode_html(&joiner, items.iter().map(Option::as_ref));
                cx.store_text(markup::tag("span", nav, Content::One(&items)))
            },
        ))
    }
}
