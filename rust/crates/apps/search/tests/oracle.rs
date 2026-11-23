//! The Ferret text functions against what PHP makes of the same texts
//! (`tests/oracle/ferret.json`, written by `tests/oracle/ferret.php` from
//! the repository's PHP code): Porter stems, term and stemmed corpora,
//! sorted ngrams, and the fields of whole documents.

use phab_app_search::{ferret, porter, text};
use serde_json::Value;

fn oracle() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/ferret.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn stems_like_porter_php() {
    let oracle = oracle();
    for pair in oracle["stems"].as_array().unwrap() {
        let word = pair[0].as_str().unwrap();
        let stem = String::from_utf8(porter::stem(word.as_bytes())).unwrap();
        assert_eq!(stem, pair[1].as_str().unwrap(), "{word:?}");
    }
}

#[test]
fn builds_corpora_and_ngrams_like_php() {
    let oracle = oracle();
    for corpus in oracle["corpora"].as_array().unwrap() {
        let source = corpus["text"].as_str().unwrap();
        assert_eq!(
            text::terms_corpus(source).unwrap(),
            corpus["terms"],
            "{source:?}"
        );
        assert_eq!(
            text::stem_corpus(source).unwrap(),
            corpus["stems"],
            "{source:?}"
        );
        assert_eq!(
            text::term_ngrams(source).unwrap(),
            strings(&corpus["ngrams"]),
            "{source:?}"
        );
    }
}

#[test]
fn builds_documents_like_php() {
    let oracle = oracle();
    for document in oracle["documents"].as_array().unwrap() {
        let fields: Vec<(String, String)> = document["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| {
                (
                    pair[0].as_str().unwrap().to_owned(),
                    pair[1].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let corpus = ferret::build(&fields).unwrap();
        let expected = &document["corpus"];
        let expected_fields: Vec<ferret::Field> = expected["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| ferret::Field {
                key: field["fieldKey"].as_str().unwrap().to_owned(),
                raw: field["rawCorpus"].as_str().unwrap().to_owned(),
                term: field["termCorpus"].as_str().unwrap().to_owned(),
                normal: field["normalCorpus"].as_str().unwrap().to_owned(),
            })
            .collect();
        assert_eq!(corpus.fields, expected_fields, "{fields:?}");
        assert_eq!(corpus.ngrams, strings(&expected["ngrams"]), "{fields:?}");
    }
}
