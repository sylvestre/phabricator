//! How `PhabricatorEmbedFileRemarkupRule` embeds files (`{F123}`): images,
//! videos and sounds in place, other files as links.

use indexmap::IndexMap;

use crate::markup::{self, Content, Markup};
use crate::objects::{FileObject, ObjectHandle};
use crate::options::{self, OptionValue};
use crate::php;
use crate::preg::{self, Pattern};

static DIMENSION: Pattern = Pattern::new(r"^(?:\d*\.)?\d+%?$");

/// `getFileOptions()`: the options as written, as strings (`true` is
/// `"1"`).
fn file_options(input: &[u8]) -> IndexMap<Vec<u8>, OptionValue> {
    let input = php::trim_chars(input, b", ");
    if input.is_empty() {
        return IndexMap::new();
    }
    options::parse(input)
}

/// `parseDimension()`.
fn dimension(value: Option<&OptionValue>) -> Option<Vec<u8>> {
    let value = value?.to_php_string();
    let value = php::trim(&value);
    preg::is_match(&DIMENSION, value).then(|| value.to_vec())
}

/// `phutil_format_bytes()`.
pub fn format_bytes(bytes: u64) -> String {
    const LABELS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    #[expect(clippy::cast_precision_loss, reason = "PHP divides floats too")]
    let mut size = bytes as f64;
    let mut label = 0;
    while size >= 1024.0 && label + 1 < LABELS.len() {
        size /= 1024.0;
        label += 1;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "floor() of a positive size"
    )]
    let whole = size.floor() as u64;
    format!("{whole} {}", LABELS[label])
}

/// `renderObjectEmbed()` of `PhabricatorEmbedFileRemarkupRule`.
pub fn render_embed(file: &FileObject, handle: &ObjectHandle, options: &[u8]) -> Markup {
    let options = file_options(options);
    let option = |key: &[u8]| options.get(key);
    let string = |key: &[u8]| option(key).map(OptionValue::to_php_string);
    let layout = string(b"layout").unwrap_or_else(|| b"left".to_vec());
    let force_link = layout == b"link";
    let (mut is_audio, mut is_video) = (file.is_audio, file.is_video);
    // "application/ogg" is both: video, unless "media=audio".
    if is_audio && is_video {
        if string(b"media").as_deref() == Some(b"audio") {
            is_video = false;
        } else {
            is_audio = false;
        }
    }
    let viewable = file.is_viewable_image || is_audio || is_video;
    if file.is_viewable_image && !force_link {
        render_image(file, &options, &layout)
    } else if (is_video || is_audio) && !force_link {
        render_media(file, &options, is_video)
    } else {
        let name = string(b"name").unwrap_or_else(|| file.name.clone());
        render_link(file, handle, &name, viewable)
    }
}

fn render_image(
    file: &FileObject,
    options: &IndexMap<Vec<u8>, OptionValue>,
    layout: &[u8],
) -> Markup {
    let size = options
        .get(&b"size"[..])
        .filter(|size| size.is_truthy())
        .map(OptionValue::to_php_string);
    let mut src = file.best_uri.clone();
    let mut width = None;
    let mut height = None;
    let mut class: &[u8] = b"phabricator-remarkup-embed-image";
    let explicit = size.is_none().then(|| {
        (
            dimension(options.get(&b"width"[..])),
            dimension(options.get(&b"height"[..])),
        )
    });
    match explicit {
        // PHP tests the strings' truthiness: "0" is no size.
        Some((w, h))
            if [&w, &h]
                .iter()
                .any(|value| value.as_deref().is_some_and(|v| v != b"0")) =>
        {
            width = w;
            height = h;
        }
        _ => match size.as_deref() {
            Some(b"full") => {
                width = file.image_size.map(|(w, _)| w.to_string().into_bytes());
                height = file.image_size.map(|(_, h)| h.to_string().into_bytes());
                class = b"phabricator-remarkup-embed-image-full";
            }
            Some(b"wide") => {
                width = file.image_size.map(|(w, _)| w.to_string().into_bytes());
                class = b"phabricator-remarkup-embed-image-wide";
            }
            _ => {
                if let Some(preview) = &file.preview {
                    src.clone_from(&preview.uri);
                    if let Some((w, h)) = &preview.size {
                        width = Some(w.clone());
                        height = Some(h.clone());
                    }
                }
            }
        },
    }
    // A bare "alt" or an empty one falls back to the file's.
    let alt = match options.get(&b"alt"[..]) {
        Some(OptionValue::Value(alt)) if !alt.is_empty() => alt.clone(),
        _ => file.alt_text.clone(),
    };
    let image = markup::tag(
        "img",
        &[
            ("src", Some(&src)),
            ("width", width.as_deref()),
            ("height", height.as_deref()),
            ("alt", Some(&alt)),
        ],
        Content::None,
    );
    let link = markup::tag(
        "a",
        &[
            ("href", Some(&file.best_uri)),
            ("class", Some(class)),
            ("target", Some(b"_blank")),
        ],
        Content::One(&image),
    );
    let layout_name: &[u8] = match layout {
        b"right" | b"center" | b"inline" | b"left" => layout,
        _ => b"left",
    };
    let mut layout_class = [b"phabricator-remarkup-embed-layout-", layout_name].concat();
    if options
        .get(&b"float"[..])
        .is_some_and(OptionValue::is_truthy)
    {
        match layout {
            b"center" | b"inline" => {}
            b"right" => layout_class.extend_from_slice(b" phabricator-remarkup-embed-float-right"),
            _ => layout_class.extend_from_slice(b" phabricator-remarkup-embed-float-left"),
        }
    }
    let element = if layout == b"inline" { "span" } else { "div" };
    markup::tag(
        element,
        &[("class", Some(&layout_class))],
        Content::One(&link),
    )
}

fn render_media(
    file: &FileObject,
    options: &IndexMap<Vec<u8>, OptionValue>,
    is_video: bool,
) -> Markup {
    let truthy = |key: &[u8]| options.get(key).is_some_and(OptionValue::is_truthy);
    let autoplay = truthy(b"autoplay");
    // Without preloading, a video shows no first frame.
    let preload: &[u8] = if autoplay || is_video {
        b"auto"
    } else {
        b"none"
    };
    // See T13135: browsers refuse some video types they can play.
    let mime: Option<&[u8]> = (!is_video).then_some(&file.mime_type);
    let alt = options.get(&b"alt"[..]).map(OptionValue::to_php_string);
    let source = markup::tag(
        "source",
        &[("src", Some(&file.best_uri)), ("type", mime)],
        Content::None,
    );
    markup::tag(
        if is_video { "video" } else { "audio" },
        &[
            ("controls", Some(b"controls")),
            ("preload", Some(preload)),
            ("autoplay", autoplay.then_some(b"autoplay")),
            ("loop", truthy(b"loop").then_some(b"loop")),
            ("alt", alt.as_deref()),
            ("class", Some(b"phabricator-media")),
        ],
        Content::One(&source),
    )
}

/// `PhabricatorFileLinkView`: the file's monogram and name, its size, and
/// a download link.
fn render_link(file: &FileObject, handle: &ObjectHandle, name: &[u8], viewable: bool) -> Markup {
    let href: &[u8] = if viewable {
        &file.best_uri
    } else {
        &file.download_uri
    };
    let icon = crate::rules::icon::render(b"file-text-o", None, false);
    let title = Markup::text(name);
    let size = Markup::text(format_bytes(file.byte_size));
    let monogram = Markup::text(handle.name.clone());
    let parts = [
        Some(icon),
        Some(markup::tag(
            "span",
            &[("class", Some(b"remarkup-file-name"))],
            Content::One(&title),
        )),
        Some(markup::tag(
            "span",
            &[("class", Some(b"remarkup-file-details"))],
            Content::List(&[Some(monogram), Some(Markup::text(" \u{b7} ")), Some(size)]),
        )),
    ];
    markup::tag(
        "a",
        &[
            ("href", Some(href)),
            ("class", Some(b"remarkup-file-link")),
            ("data-phid", Some(handle.phid.as_bytes())),
        ],
        Content::List(&parts),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_like_php() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5 MB");
    }
}
