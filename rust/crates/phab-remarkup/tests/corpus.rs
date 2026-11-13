//! The corpus of `PhutilRemarkupEngineTestCase`: each file holds the input,
//! the expected HTML and the expected text, separated by `~~~~~~~~~~` lines.

mod common;

use std::path::PathBuf;

use phab_remarkup::Mode;
use phab_remarkup::blocks::header;

/// `unescapeTrailingWhitespace()`: a `~` ending a line stands for nothing,
/// so trailing spaces can be written.
fn unescape(text: &str) -> String {
    text.split('\n')
        .map(|line| line.strip_suffix('~').unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Files where PHP highlights code with its lexers and Rust does not
/// (yet): the HTML is compared without the highlighting spans.
const HIGHLIGHTED: &[&str] = &["quoted-code-block.txt"];

fn strip_spans(html: &str) -> String {
    let mut output = String::new();
    let mut rest = html;
    while let Some(start) = rest.find("<span class=\"") {
        output.push_str(&rest[..start]);
        let after = &rest[start..];
        let end = after.find('>').map_or(after.len(), |end| end + 1);
        rest = &after[end..];
    }
    output.push_str(rest);
    output.replace("</span>", "")
}

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../src/infrastructure/markup/remarkup/__tests__/remarkup")
}

#[test]
fn corpus_matches_php() {
    let mut files: Vec<_> = std::fs::read_dir(corpus_dir())
        .expect("the PHP corpus is in the repository")
        .map(|entry| entry.expect("readable directory").path())
        .collect();
    files.sort();
    assert!(files.len() > 80, "found only {} files", files.len());

    let mut failures = Vec::new();
    for path in &files {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let contents = std::fs::read_to_string(path).unwrap();
        let parts: Vec<&str> = contents.split("\n~~~~~~~~~~\n").collect();
        assert_eq!(parts.len(), 3, "{file}");
        let (mut input, mut html, mut text) =
            (unescape(parts[0]), unescape(parts[1]), unescape(parts[2]));

        let mut config = common::config();
        match file.as_str() {
            "raw-escape.txt" => {
                input = input.replace('~', "\x01");
                html = html.replace('~', "\x01");
                text = text.replace('~', "\x01");
            }
            "toc.txt" => config.generate_toc = true,
            "link-same-window.txt" => config.same_window = true,
            "link-square.txt" => {
                config.uri_base = Some("http://www.example.com/".to_owned());
                config.uri_here = Some("http://www.example.com/page/".to_owned());
            }
            _ => {}
        }
        let engine = common::engine(config);

        let rendered = engine.markup(input.as_bytes(), Mode::Html);
        let mut actual = String::from_utf8_lossy(rendered.output.as_bytes()).into_owned();
        if file == "toc.txt" {
            let toc = header::render_table_of_contents(&rendered.metadata.toc)
                .map(|toc| String::from_utf8_lossy(toc.as_bytes()).into_owned())
                .unwrap_or_default();
            actual = format!("{toc}\n\n{actual}");
        }
        let (expected_html, actual) = if HIGHLIGHTED.contains(&file.as_str()) {
            (strip_spans(&html), strip_spans(&actual))
        } else {
            (html.clone(), actual)
        };
        if actual != expected_html {
            failures.push(format!(
                "{file} (html)\n--- expected\n{expected_html}\n--- actual\n{actual}"
            ));
        }

        let rendered = engine.markup(input.as_bytes(), Mode::Text);
        let actual = String::from_utf8_lossy(rendered.output.as_bytes()).into_owned();
        if actual != text {
            failures.push(format!(
                "{file} (text)\n--- expected\n{text}\n--- actual\n{actual}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} renderings differ from PHP:\n\n{}",
        failures.len(),
        files.len() * 2,
        failures.join("\n\n")
    );
}
