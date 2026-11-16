//! The Phabricator rules, with objects resolved by hand.

use std::sync::Arc;

use phab_remarkup::objects::Preview;
use phab_remarkup::{
    Config, FileObject, Highlighter, Markup, MentionedUser, Mode, ObjectHandle, RepositoryRef,
    Resolved, phabricator,
};

fn config() -> Config {
    Config {
        allowed_protocols: ["http", "https"].map(str::to_owned).into(),
        base_uri: Some("https://phabricator.test/".to_owned()),
        bugzilla_url: Some("https://bugzilla.test".to_owned()),
        self_hosts: ["phabricator.test".to_owned()].into(),
        ..phabricator::default_config()
    }
}

fn resolved() -> Resolved {
    let mut resolved = Resolved::default();
    resolved.objects.insert(
        ("D", b"12".to_vec()),
        ObjectHandle {
            phid: "PHID-DREV-12".to_owned(),
            name: b"D12".to_vec(),
            full_name: b"D12: Fix <it>".to_vec(),
            uri: b"/D12".to_vec(),
            closed: true,
            ..ObjectHandle::default()
        },
    );
    resolved.objects.insert(
        ("#", b"Firefox-Build".to_vec()),
        ObjectHandle {
            phid: "PHID-PROJ-1".to_owned(),
            name: b"Firefox Build".to_vec(),
            full_name: b"Firefox Build".to_vec(),
            uri: b"/tag/firefox-build/".to_vec(),
            closed: false,
            ..ObjectHandle::default()
        },
    );
    add_other_objects(&mut resolved);
    resolved.users.insert(
        b"alice".to_vec(),
        MentionedUser {
            phid: "PHID-USER-alice".to_owned(),
            user_name: b"Alice".to_vec(),
            is_disabled: false,
            can_view_context: false,
        },
    );
    resolved
}

/// The objects of the rules beyond Differential, projects and people.
fn add_other_objects(resolved: &mut Resolved) {
    resolved.objects.insert(
        ("H", b"3".to_vec()),
        ObjectHandle {
            phid: "PHID-HRUL-3".to_owned(),
            name: b"H3".to_vec(),
            full_name: b"H3 Security".to_vec(),
            uri: b"/H3".to_vec(),
            ..ObjectHandle::default()
        },
    );
    resolved.objects.insert(
        ("", b"rMOZILLACENTRALabcdef1".to_vec()),
        ObjectHandle {
            phid: "PHID-CMIT-1".to_owned(),
            name: b"rMOZILLACENTRALabcdef123456".to_vec(),
            full_name: b"rMOZILLACENTRALabcdef123456: Land it".to_vec(),
            uri: b"/rMOZILLACENTRALabcdef123456".to_vec(),
            ..ObjectHandle::default()
        },
    );
    resolved.objects.insert(
        ("F", b"7".to_vec()),
        ObjectHandle {
            phid: "PHID-FILE-7".to_owned(),
            name: b"F7".to_vec(),
            full_name: b"F7: shot.png".to_vec(),
            uri: b"/F7".to_vec(),
            ..ObjectHandle::default()
        },
    );
    resolved.files.insert(
        b"7".to_vec(),
        FileObject {
            name: b"shot.png".to_vec(),
            mime_type: b"image/png".to_vec(),
            best_uri: b"/file/data/key/PHID-FILE-7/shot.png".to_vec(),
            download_uri: b"/file/download/key/PHID-FILE-7/shot.png".to_vec(),
            preview: Some(Preview {
                uri: b"/file/xform/preview/PHID-FILE-7/key/".to_vec(),
                size: Some((b"220".to_vec(), b"110".to_vec())),
            }),
            image_size: Some((800, 400)),
            is_viewable_image: true,
            byte_size: 2048,
            alt_text: b"Screenshot".to_vec(),
            ..FileObject::default()
        },
    );
    resolved.handles.insert(
        "PHID-DREV-12".to_owned(),
        resolved.objects[&("D", b"12".to_vec())].clone(),
    );
    resolved.repositories.insert(
        b"mozilla-central".to_vec(),
        RepositoryRef {
            phid: "PHID-REPO-1".to_owned(),
            monogram: b"rMOZILLACENTRAL".to_vec(),
            uri: b"/source/mozilla-central/".to_vec(),
            default_branch: Some(b"default".to_vec()),
            searchfox: Some(b"https://searchfox.org/mozilla-central/".to_vec()),
        },
    );
    resolved.repositories.insert(
        b"try".to_vec(),
        RepositoryRef {
            phid: "PHID-REPO-2".to_owned(),
            monogram: b"R2".to_vec(),
            uri: b"/source/try/".to_vec(),
            default_branch: Some(b"default".to_vec()),
            searchfox: None,
        },
    );
}

fn render(text: &str, mode: Mode) -> (String, Vec<String>) {
    let engine = phabricator::engine(config());
    let prepared = engine.prepare(text.as_bytes(), mode);
    let rendered = engine.finish(prepared, resolved());
    (
        String::from_utf8(rendered.output.into_bytes()).unwrap(),
        rendered.metadata.mentioned_phids,
    )
}

fn html(text: &str) -> String {
    render(text, Mode::Html).0
}

#[test]
fn references_to_visible_objects_link() {
    assert_eq!(
        html("See D12."),
        "<p>See <a href=\"/D12\" class=\"remarkup-object remarkup-object-closed\" \
         title=\"D12: Fix &lt;it&gt;\" data-phid=\"PHID-DREV-12\">D12</a>.</p>"
    );
    assert_eq!(
        html("D12#inline-3"),
        "<p><a href=\"/D12#inline-3\" class=\"remarkup-object remarkup-object-closed\" \
         title=\"D12: Fix &lt;it&gt;\" data-phid=\"PHID-DREV-12\">D12#inline-3</a></p>"
    );
    assert_eq!(
        html("{D12}"),
        "<p><a href=\"/D12\" class=\"remarkup-object remarkup-object-closed\" \
         title=\"D12: Fix &lt;it&gt;\" data-phid=\"PHID-DREV-12\">D12: Fix &lt;it&gt;</a></p>"
    );
}

#[test]
fn hidden_or_missing_objects_stay_text() {
    assert_eq!(html("D13 and {D14}"), "<p>D13 and {D14}</p>");
    // Not references: inside words, code and links.
    assert_eq!(html("xD12 D12x `D12`").matches("data-phid").count(), 0);
}

#[test]
fn text_mode_gives_production_uris() {
    let (text, phids) = render("D12 and @alice", Mode::Text);
    assert_eq!(text, "D12 <https://phabricator.test/D12> and @alice\n");
    assert_eq!(phids, ["PHID-DREV-12"]);
}

#[test]
fn mentions_link_to_profiles() {
    let (output, phids) = render("@ALICE, @bob and x@alice", Mode::Html);
    assert_eq!(
        output,
        "<p><a href=\"/p/Alice/\" class=\"remarkup-mention remarkup-mention-exiled\" \
         data-phid=\"PHID-USER-alice\">@Alice</a>, \
         <span class=\"phabricator-remarkup-mention-unknown\">@bob</span> and x@alice</p>"
    );
    assert_eq!(phids, ["PHID-USER-alice"]);
}

#[test]
fn projects_show_their_name() {
    assert_eq!(
        html("#Firefox-Build, #unknown"),
        "<p><a href=\"/tag/firefox-build/\" class=\"remarkup-project\" \
         data-phid=\"PHID-PROJ-1\">Firefox Build</a>, #unknown</p>"
    );
}

#[test]
fn bugs_link_to_bugzilla() {
    assert_eq!(
        html("Bug 1234 comment 5, bug #99"),
        "<p><a href=\"https://bugzilla.test/show_bug.cgi?id=1234#c5\">Bug 1234 comment 5</a>, \
         <a href=\"https://bugzilla.test/show_bug.cgi?id=99\">bug #99</a></p>"
    );
}

#[test]
fn line_breaks_are_kept() {
    assert_eq!(html("a\nb"), "<p>a<br />\nb</p>");
}

#[test]
fn review_helper_frontmatter() {
    let text = "---\nreviewhelper:\n  severity: major\n  category: security\n---\n\nThe **text**.";
    // PHP's rendering, without the Javelin metadata of the icon.
    assert_eq!(
        html(text),
        "<p class=\"reviewhelper-severity-label\" style=\"display: none;\" \
         data-reviewhelper-frontmatter=\"{&quot;severity&quot;:&quot;major&quot;,&quot;category&quot;:&quot;security&quot;}\">\
         <span class=\"reviewhelper-severity-part\">Major</span> <span class=\"reviewhelper-category-part\">\
         <span class=\"visual-only phui-icon-view phui-font-fa fa-lock\" aria-hidden=\"true\"></span> Security</span></p>\n\n\
         <p>The <strong>text</strong>.</p>"
    );
    assert_eq!(
        render(text, Mode::Text).0,
        "[Major] [Security]\n\nThe **text**.\n"
    );
    // A broken frontmatter disappears, like in PHP.
    assert_eq!(
        html("---\nreviewhelper:\n  severity: huge\n---\nx"),
        "\n\n<p>x</p>"
    );
}

#[test]
fn markup_is_safe_html() {
    let engine = phabricator::engine(config());
    let output = engine.markup(b"<script>", Mode::Html).output;
    assert_eq!(output, Markup::html("<p>&lt;script&gt;</p>"));
}

fn text(text: &str) -> String {
    render(text, Mode::Text).0
}

#[test]
fn eval_names_the_platform() {
    assert_eq!(
        html("${{{strings.platform.server.name}}} ${{{x.y}}}"),
        "<p>Phabricator ${{{x.y}}}</p>"
    );
}

#[test]
fn keys_show_their_symbols() {
    assert_eq!(
        html("{key cmd shift P}"),
        "<p><kbd title=\"Command\">\u{2318}</kbd><span class=\"kbd-join\">+</span>\
         <kbd title=\"Shift\">\u{21e7}</kbd><span class=\"kbd-join\">+</span><kbd>P</kbd></p>"
    );
    assert_eq!(text("{key ctrl \\}}"), "[\u{2303}] + [}]\n");
}

#[test]
fn navigation_sequences() {
    assert_eq!(
        html("{nav icon=home, name=Home > Settings > type=instructions, name=Pick one}"),
        "<p><span class=\"remarkup-nav-sequence\"><span class=\"remarkup-nav-item\">\
         <span class=\"fa fa-home remarkup-icon\" aria-hidden=\"true\"></span>Home</span>\
         <span class=\"remarkup-nav-sequence-arrow\"> \u{2192} </span>\
         <span class=\"remarkup-nav-item\">Settings</span>\
         <span class=\"remarkup-nav-sequence-arrow\"> \u{2192} </span>\
         <span class=\"remarkup-nav-item remarkup-nav-instructions\"><em>Pick one</em></span></span></p>"
    );
    assert_eq!(text("{nav A > B}"), "A > B\n");
    // Only remote links are linked.
    assert!(html("{nav name=X, href=https://x.test/}").contains("href=\"https://x.test/\""));
    assert!(!html("{nav name=X, href=javascript:alert(1)}").contains("href"));
}

#[test]
fn icons_are_checked() {
    assert_eq!(
        html("{icon camera, color=red, spin} {icon nope color=nocolor} {icon bug color=red spin}"),
        "<p><span class=\"fa fa-camera remarkup-icon remarkup-icon-red fa-spin\" aria-hidden=\"true\"></span> \
         <span class=\"fa fa-paw remarkup-icon\" aria-hidden=\"true\"></span> \
         <span class=\"fa fa-bug remarkup-icon\" aria-hidden=\"true\"></span></p>"
    );
    assert_eq!(text("{icon camera}"), "{icon camera}\n");
}

#[test]
fn emoji() {
    assert_eq!(
        html("ok :+1: :thumbsup: :nope:"),
        "<p>ok \u{1f44d} \u{1f44d} :nope:</p>"
    );
}

#[test]
fn config_options_link_to_their_page() {
    assert_eq!(
        html("@{config:bugzilla.url} @{config:no.such.key}"),
        "<p><a href=\"/config/edit/bugzilla.url/\" target=\"_blank\">bugzilla.url</a> \
         @{config:no.such.key}</p>"
    );
    assert_eq!(text("@{config:bugzilla.url}"), "\"bugzilla.url\"\n");
}

#[test]
fn youtube_only_when_enabled() {
    let link = "https://www.youtube.com/watch?v=abc";
    assert!(!html(link).contains("iframe"));
    let engine = phabricator::engine(Config {
        youtube: true,
        ..config()
    });
    let output = engine
        .markup(link.as_bytes(), Mode::Html)
        .output
        .into_bytes();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "<p><div class=\"embedded-youtube-video\"><iframe width=\"650\" height=\"400\" \
         style=\"margin: 1em auto; border: 0px;\" src=\"https://www.youtube.com/embed/abc\" \
         frameborder=\"0\"></iframe></div></p>"
    );
}

#[test]
fn external_images_go_through_the_proxy() {
    assert_eq!(
        html("{img https://x.test/a b.png} {img src=https://x.test/c.png, width=20} {img nope}"),
        "<p><img src=\"/file/imageproxy/?uri=https%3A%2F%2Fx.test%2Fa+b.png\" \
         class=\"remarkup-external-image\" /> \
         <img src=\"/file/imageproxy/?uri=https%3A%2F%2Fx.test%2Fc.png\" width=\"20\" \
         class=\"remarkup-external-image\" /> {img nope}</p>"
    );
}

#[test]
fn handles_show_visible_objects() {
    assert_eq!(
        html("{PHID-DREV-12} {PHID-DREV-99} {PHID-ZZZZ-1}"),
        "<p><a href=\"/D12\" class=\"remarkup-handle remarkup-handle-closed\" \
         data-phid=\"PHID-DREV-12\">D12: Fix &lt;it&gt;</a> {PHID-DREV-99} {PHID-ZZZZ-1}</p>"
    );
    assert_eq!(text("{PHID-DREV-12}"), "{PHID-DREV-12}\n");
}

#[test]
fn other_monograms() {
    let output = html("H3, rMOZILLACENTRALabcdef1 and H4");
    assert!(output.contains(">H3</a>"), "{output}");
    assert!(
        output.contains("href=\"/rMOZILLACENTRALabcdef123456\""),
        "{output}"
    );
    assert!(output.ends_with(" and H4</p>"), "{output}");
}

#[test]
fn files_embed() {
    assert_eq!(
        html("{F7}"),
        "<p><div class=\"phabricator-remarkup-embed-layout-left\">\
         <a href=\"/file/data/key/PHID-FILE-7/shot.png\" class=\"phabricator-remarkup-embed-image\" \
         target=\"_blank\"><img src=\"/file/xform/preview/PHID-FILE-7/key/\" width=\"220\" \
         height=\"110\" alt=\"Screenshot\" /></a></div></p>"
    );
    let full = html("{F7, size=full, alt=\"A shot\", layout=right, float}");
    assert!(
        full.contains("width=\"800\" height=\"400\" alt=\"A shot\""),
        "{full}"
    );
    assert!(
        full.contains(
            "phabricator-remarkup-embed-layout-right phabricator-remarkup-embed-float-right"
        ),
        "{full}"
    );
    let link = html("{F7, layout=link}");
    assert!(link.contains("class=\"remarkup-file-link\""), "{link}");
    assert!(link.contains("F7 \u{b7} 2 KB"), "{link}");
    assert_eq!(text("{F7}"), "F7: shot.png <https://phabricator.test/F7>\n");
}

#[test]
fn source_links() {
    assert_eq!(
        html("{src mozilla-central:dom/a.cpp, line=12}"),
        "<p><a href=\"https://searchfox.org/mozilla-central/source/dom/a.cpp#12\" \
         class=\"remarkup-source-link\" data-phid=\"PHID-REPO-1\">\
         <span class=\"fa fa-file-text-o remarkup-icon\" aria-hidden=\"true\"></span>\
         rMOZILLACENTRAL dom/a.cpp:12</a></p>"
    );
    let output = html("{src b.py repository=try, commit=abc, ref=beta}");
    assert!(
        output.contains("href=\"/source/try/browse/beta/b.py;abc\""),
        "{output}"
    );
    assert!(output.contains("R2 b.py (on beta at abc)"), "{output}");
    assert!(html("{src a.cpp}").contains("remarkup-syntax-hint"));
    assert_eq!(html("{src secret:a.cpp}"), "<p>{src secret:a.cpp}</p>");
}

#[test]
fn links_to_the_install() {
    assert_eq!(
        html("https://phabricator.test/D12 and {https://phabricator.test/D12}"),
        "<p><a href=\"https://phabricator.test/D12\" class=\"remarkup-handle remarkup-handle-closed\" \
         data-phid=\"PHID-DREV-12\">https://phabricator.test/D12</a> and \
         <a href=\"https://phabricator.test/D12\" class=\"remarkup-handle remarkup-handle-closed\" \
         data-phid=\"PHID-DREV-12\">D12: Fix &lt;it&gt;</a></p>"
    );
    let output = html("https://phabricator.test/source/mozilla-central/browse/default/a.cpp$3");
    assert!(
        output.contains("class=\"remarkup-source-link\""),
        "{output}"
    );
    // Elsewhere, or hidden: plain links.
    assert!(html("https://other.test/D12").contains("class=\"remarkup-link\""));
    assert!(html("https://phabricator.test/D99").contains("class=\"remarkup-link\""));
}

#[test]
fn references_to_resolve() {
    let engine = phabricator::engine(config());
    let prepared = engine.prepare(
        b"D1 {F2} https://phabricator.test/H5 https://phabricator.test/@Bob {PHID-USER-x} \
          {src mozilla-central:a} https://phabricator.test/diffusion/R2/browse/x",
        Mode::Html,
    );
    assert_eq!(
        prepared.object_references(),
        [
            ("D", b"1".to_vec()),
            ("F", b"2".to_vec()),
            ("H", b"5".to_vec())
        ]
    );
    assert_eq!(prepared.mentioned_usernames(), [b"bob".to_vec()]);
    assert_eq!(prepared.handle_phids(), ["PHID-USER-x"]);
    assert_eq!(
        prepared.repository_identifiers(),
        [b"mozilla-central".to_vec(), b"R2".to_vec()]
    );
}

#[test]
fn code_blocks_use_the_highlighter() {
    let engine = phabricator::engine(Config {
        highlighter: Some(Highlighter(Arc::new(|language: &[u8], code: &[u8]| {
            Some(Markup::html([language, b":", code].concat()))
        }))),
        ..config()
    });
    let output = engine.markup(b"```lang=rust\nfn x\n```", Mode::Html).output;
    assert!(
        String::from_utf8_lossy(output.as_bytes())
            .contains("<pre class=\"remarkup-code\">rust:fn x</pre>"),
        "{output:?}"
    );
    let console = engine.markup(b"```\n$ ls\n```", Mode::Html).output;
    assert!(String::from_utf8_lossy(console.as_bytes()).contains("class=\"gp\""));
}
