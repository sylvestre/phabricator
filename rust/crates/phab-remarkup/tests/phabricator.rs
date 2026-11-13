//! The Phabricator rules, with objects resolved by hand.

use phab_remarkup::{Config, Markup, MentionedUser, Mode, ObjectHandle, Resolved, phabricator};

fn config() -> Config {
    Config {
        allowed_protocols: ["http", "https"].map(str::to_owned).into(),
        base_uri: Some("https://phabricator.test/".to_owned()),
        bugzilla_url: Some("https://bugzilla.test".to_owned()),
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
        },
    );
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
