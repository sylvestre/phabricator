<?php

// Writes ferret.json: what PHP's Ferret code makes of generated texts, for
// tests/oracle.rs. It runs the repository's PhutilSearchStemmer, Porter
// stemmer, PhabricatorSearchNgramEngine and
// PhabricatorFerretEngine::newTermsCorpus() with stubs for the rest of
// Phabricator, and the corpus loop of
// PhabricatorFerretFulltextEngineExtension::indexFulltextObject(), copied.
//
//   php rust/crates/apps/search/tests/oracle/ferret.php \
//     > rust/crates/apps/search/tests/oracle/ferret.json
//
// Texts beyond ASCII have no uppercase letters there: without mbstring,
// phutil_utf8_strtolower() is strtolower() here, which is the same for them.

$root = dirname(__DIR__, 6);

class Phobject {}

function phutil_get_library_root($library) {
  global $root;
  return $root.'/src';
}

function phutil_utf8_strtolower($string) {
  return strtolower($string);
}

function phutil_utf8v($string) {
  $res = array();
  $len = strlen($string);
  $ii = 0;
  while ($ii < $len) {
    $byte = $string[$ii];
    if ($byte <= "\x7F") {
      $res[] = $byte;
      $ii += 1;
      continue;
    } else if ($byte < "\xC0") {
      throw new Exception('Invalid UTF-8.');
    } else if ($byte <= "\xDF") {
      $seq_len = 2;
    } else if ($byte <= "\xEF") {
      $seq_len = 3;
    } else if ($byte <= "\xF7") {
      $seq_len = 4;
    } else {
      throw new Exception('Invalid UTF-8.');
    }
    $res[] = substr($string, $ii, $seq_len);
    $ii += $seq_len;
  }
  return $res;
}

require_once $root.'/src/applications/search/compiler/PhutilSearchStemmer.php';
require_once $root.'/src/applications/search/engine/PhabricatorSearchNgramEngine.php';
require_once $root.'/src/applications/search/ferret/PhabricatorFerretEngine.php';
require_once $root.'/externals/porter-stemmer/src/Porter.php';

final class OracleFerretEngine extends PhabricatorFerretEngine {
  public function getApplicationName() {
    return 'oracle';
  }
  public function getScopeName() {
    return 'oracle';
  }
  public function newSearchEngine() {
    return null;
  }
}

// The corpus loop of indexFulltextObject(), up to the common ngrams.
function build_corpus(array $document_fields) {
  $engine = new OracleFerretEngine();
  $stemmer = $engine->newStemmer();

  $virtual_fields = array();
  foreach ($document_fields as $field) {
    $virtual_fields[] = $field;

    list($key, $raw_corpus) = $field;
    switch ($key) {
      case 'titl':
      case 'body':
        $virtual_fields[] = array('core', $raw_corpus);
        break;
    }

    $virtual_fields[] = array('full', $raw_corpus);
  }

  $empty_template = array(
    'raw' => array(),
    'term' => array(),
    'normal' => array(),
  );

  $ferret_corpus_map = array();

  foreach ($virtual_fields as $field) {
    list($key, $raw_corpus) = $field;
    if (!strlen($raw_corpus)) {
      continue;
    }

    $term_corpus = $engine->newTermsCorpus($raw_corpus);

    $normal_corpus = $stemmer->stemCorpus($raw_corpus);
    $normal_corpus = $engine->newTermsCorpus($normal_corpus);

    if (!isset($ferret_corpus_map[$key])) {
      $ferret_corpus_map[$key] = $empty_template;
    }

    $ferret_corpus_map[$key]['raw'][] = $raw_corpus;
    $ferret_corpus_map[$key]['term'][] = $term_corpus;
    $ferret_corpus_map[$key]['normal'][] = $normal_corpus;
  }

  $ferret_fields = array();
  $ngrams_source = array();
  foreach ($ferret_corpus_map as $key => $fields) {
    $raw_corpus = $fields['raw'];
    $raw_corpus = implode("\n", $raw_corpus);
    if (strlen($raw_corpus)) {
      $ngrams_source[] = $raw_corpus;
    }

    $normal_corpus = $fields['normal'];
    $normal_corpus = implode("\n", $normal_corpus);
    if (strlen($normal_corpus)) {
      $ngrams_source[] = $normal_corpus;
    }

    $term_corpus = $fields['term'];
    $term_corpus = implode("\n", $term_corpus);
    if (strlen($term_corpus)) {
      $ngrams_source[] = $term_corpus;
    }

    $ferret_fields[] = array(
      'fieldKey' => (string)$key,
      'rawCorpus' => $raw_corpus,
      'termCorpus' => $term_corpus,
      'normalCorpus' => $normal_corpus,
    );
  }
  $ngrams_source = implode("\n", $ngrams_source);

  $ngram_engine = new PhabricatorSearchNgramEngine();
  $ngrams = $ngram_engine->getTermNgramsFromString($ngrams_source);

  return array(
    'fields' => $ferret_fields,
    'ngrams' => array_map('strval', $ngrams),
  );
}

mt_srand(20261123);

$pieces = array(
  'a', 'e', 'i', 'o', 'u', 'y', 'b', 'c', 'd', 'g', 'l', 'm', 'n', 'r',
  's', 't', 'z', 'A', 'Y', 'Q',
  '0', '1', '2', '5', '9', '10', '12', '100', '1e3', '.5', '-1', '0x1',
  ' ', ' ', ' ', ' ', '  ', "\n", "\t", "\r\n", "'", "''", '.', '_', '-',
  ',', ':', '"', '(', ')', '[', '/', '@', '#',
  "\u{e9}", "\u{fc}", "\u{6771}", "\u{1f389}", "\u{a0}", "\u{2028}",
  "\u{2014}", "\u{2019}",
  'ing', 'ed', 'ation', 'ness', 'ies', 'sses', 'eed', 'ful', 'ize',
  'tion', 'ement', 'ble', 'ally', 'ousli',
);

function random_text($length) {
  global $pieces;
  $text = '';
  for ($ii = 0; $ii < $length; $ii++) {
    $text .= $pieces[mt_rand(0, count($pieces) - 1)];
  }
  return $text;
}

$words = array();
foreach (file($root.'/externals/wordlist/password.lst') as $line) {
  $line = trim($line);
  if (!strlen($line) || $line[0] == '#') {
    continue;
  }
  $words[] = strtolower($line);
}
for ($ii = 0; $ii < 500; $ii++) {
  $words[] = strtolower(preg_replace('/[^a-z0-9]/i', '', random_text(6)));
}
$words = array_values(array_unique($words));

$stems = array();
foreach ($words as $word) {
  $stems[] = array($word, Porter::Stem($word));
}

$engine = new OracleFerretEngine();
$stemmer = new PhutilSearchStemmer();
$ngram_engine = new PhabricatorSearchNgramEngine();

$texts = array(
  '',
  'Fix the crash when loading the revision',
  "Bug 1234567 - Part 2: don't crash in nsFoo::Bar() r=alice",
  "whom'st've 'quoted' ''double'' trailing'",
  'version 9 10 5a 1e3 0x1 -12 .5 100 12 12 1.',
  ' 9  10  5a ',
);
for ($ii = 0; $ii < 200; $ii++) {
  $texts[] = random_text(mt_rand(1, 60));
}
for ($ii = 0; $ii < 4; $ii++) {
  $texts[] = random_text(mt_rand(1000, 1500));
}

$corpora = array();
foreach ($texts as $text) {
  $corpora[] = array(
    'text' => $text,
    'terms' => $engine->newTermsCorpus($text),
    'stems' => $stemmer->stemCorpus($text),
    'ngrams' => array_map(
      'strval',
      $ngram_engine->getTermNgramsFromString($text)),
  );
}

$keys = array('titl', 'body', 'plan', 'cmnt', 'cmnt', 'cmnt', '!abc');
$documents = array();
for ($ii = 0; $ii < 40; $ii++) {
  $fields = array();
  $count = mt_rand(1, 6);
  for ($jj = 0; $jj < $count; $jj++) {
    $text = (mt_rand(0, 5) == 0) ? '' : random_text(mt_rand(1, 80));
    $fields[] = array($keys[mt_rand(0, count($keys) - 1)], $text);
  }
  $documents[] = array(
    'fields' => $fields,
    'corpus' => build_corpus($fields),
  );
}

echo json_encode(
  array(
    'stems' => $stems,
    'corpora' => $corpora,
    'documents' => $documents,
  ),
  JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE |
  JSON_INVALID_UTF8_SUBSTITUTE)."\n";
