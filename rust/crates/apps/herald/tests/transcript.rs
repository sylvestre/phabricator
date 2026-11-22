//! The transcript of Mozilla's secure revision rules on a comment, byte for
//! byte as PHP wrote it on the development stack (`fixtures/comment-*.bin`,
//! from `scripts/write-parity.py --show`: new PHIDs and durations there are
//! placeholders).

use phab_app_herald::engine::{self, ActionRecord, Applied, Condition, Rule, Subject};
use phab_db::phpser::Value as Php;

const SECURE: &str = "PHID-PROJ-ny4vv46ou3q6czrebiig";
const OWNER: &str = "PHID-USER-p2ejxxxls4jnndhjs4qb";
const REVISION: &str = "PHID-DREV-edjtusnk22iepdwjfktk";
/// A transaction PHID as long as the one PHP made.
const XACT: &str = "PHID-XACT-DREV-xxxxxxxxxxxxxxx";

fn rules() -> Vec<Rule> {
    vec![
        Rule {
            id: "1".to_owned(),
            phid: "PHID-HRUL-qfiuh4wcceqot7c3zeoq".to_owned(),
            name: "Add Secure Revision Project Tag".to_owned(),
            author_phid: OWNER.to_owned(),
            rule_type: "global".to_owned(),
            must_match_all: true,
            config_version: 38,
            author_valid: true,
            author_can_view: true,
            conditions: vec![Condition {
                id: "1".to_owned(),
                field: "new-object".to_owned(),
                condition: "true".to_owned(),
                value: Php::Null,
            }],
            actions: vec![ActionRecord {
                action: "projects.add".to_owned(),
                target: Php::list([Php::str(SECURE)]),
            }],
        },
        Rule {
            id: "2".to_owned(),
            phid: "PHID-HRUL-ra3gvxfqfrm7h4tidmci".to_owned(),
            name: "Secure Mail".to_owned(),
            author_phid: OWNER.to_owned(),
            rule_type: "global".to_owned(),
            must_match_all: false,
            config_version: 38,
            author_valid: true,
            author_can_view: true,
            conditions: vec![
                Condition {
                    id: "2".to_owned(),
                    field: "rule".to_owned(),
                    condition: "conditions".to_owned(),
                    value: Php::str("PHID-HRUL-qfiuh4wcceqot7c3zeoq"),
                },
                Condition {
                    id: "3".to_owned(),
                    field: "projects".to_owned(),
                    condition: "any".to_owned(),
                    value: Php::list([Php::str(SECURE)]),
                },
            ],
            actions: vec![ActionRecord {
                action: "email.must-encrypt".to_owned(),
                target: Php::Null,
            }],
        },
    ]
}

/// The masking of `write-parity.py`: the new PHID and the durations.
fn mask(bytes: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes).replace(XACT, "PHID-XACT-DREV-<1>");
    let mut output = String::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find("\"elapsed\";d:") {
        let (head, tail) = rest.split_at(at + "\"elapsed\";d:".len());
        output.push_str(head);
        output.push_str("<duration>");
        rest = &tail[tail.find(';').unwrap_or(0)..];
    }
    output.push_str(rest);
    output.into_bytes()
}

#[test]
fn writes_the_transcript_php_wrote() {
    let subject = Subject {
        phid: REVISION.to_owned(),
        content_type: "differential".to_owned(),
        name: "Bug 1890001 - Throw InvalidCharacterError for empty attribute names".to_owned(),
        applied_transaction_phids: Some(vec![XACT.to_owned()]),
    };
    let values = vec![
        ("new-object".to_owned(), Php::Bool(false)),
        ("rule".to_owned(), Php::Null),
        (
            "projects".to_owned(),
            Php::list([Php::str("PHID-PROJ-sv7lxylfx3dmjxbkjkt4"), Php::str(SECURE)]),
        ),
    ];
    let evaluation = engine::evaluate(&rules(), &subject, &values);
    let applied: Vec<Applied> = evaluation
        .effects
        .iter()
        .map(|effect| Applied {
            effect: effect.clone(),
            applied: true,
            reasons: vec![(
                "do.must-encrypt".to_owned(),
                Php::list([Php::str(&effect.rule_phid)]),
            )],
            standard: false,
        })
        .collect();
    let row = evaluation.transcript(&subject, &applied);
    let fixture = |name: &str| {
        std::fs::read(format!(
            "{}/tests/fixtures/comment-{name}.bin",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("fixture")
    };
    let show = |bytes: &[u8]| String::from_utf8_lossy(bytes).replace('\0', "~");
    for (name, ours) in [
        ("ruleTranscripts", &row.rule_transcripts),
        ("conditionTranscripts", &row.condition_transcripts),
        ("applyTranscripts", &row.apply_transcripts),
    ] {
        assert_eq!(show(ours), show(&fixture(name)), "{name}");
    }
    assert_eq!(
        show(&mask(&row.object_transcript)),
        show(&fixture("objectTranscript"))
    );
    assert_eq!(engine::rules_header(&applied), "<2>");
}
