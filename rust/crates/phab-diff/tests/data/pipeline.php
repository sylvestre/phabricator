<?php

// Runs the hunks of pipeline_input.json (stdin) through PHP's
// DifferentialHunkParser, the way DifferentialChangesetParser::process()
// does without realignment, and prints what the Rust port must reproduce.
// It needs the Phabricator libraries, so run it in the PHP container:
//
//   python3 pipeline_input.py
//   docker cp pipeline.php phabricator-phabricator-1:/tmp/pipeline.php
//   docker exec -i phabricator-phabricator-1 php /tmp/pipeline.php \
//     < pipeline_input.json > pipeline_expected.json

require_once '/app/phabricator/scripts/init/init-script.php';

function line_json($line) {
  if (!$line) {
    return null;
  }
  return array(
    'type' => $line['type'],
    'text' => $line['text'],
    'line' => (int)$line['line'],
  );
}

function pairs(array $map) {
  $result = array();
  foreach ($map as $key => $value) {
    $result[] = array($key, $value);
  }
  return $result;
}

$cases = phutil_json_decode(file_get_contents('php://stdin'));
$output = array();
foreach ($cases as $case) {
  $hunks = array();
  foreach ($case['hunks'] as $spec) {
    $hunks[] = id(new DifferentialHunk())
      ->setOldOffset($spec['old_offset'])
      ->setOldLen($spec['old_len'])
      ->setNewOffset($spec['new_offset'])
      ->setNewLen($spec['new_len'])
      ->setChanges($spec['changes']);
  }

  $parser = new DifferentialHunkParser();
  $parser->parseHunksForLineData($hunks);
  $parser->reparseHunksForSpecialAttributes();
  $parser->generateIntraLineDiffs();

  $blocks = $parser->generateVisibleBlocksMask($case['context']);
  $parser->generateVisibleLinesMask($case['context']);

  $intra = array();
  foreach ($parser->getIntraLineDiffs() as $key => $sides) {
    $intra[] = array($key, $sides[0], $sides[1]);
  }

  $output[] = array(
    'name' => $case['name'],
    'old' => array_map('line_json', $parser->getOldLines()),
    'new' => array_map('line_json', $parser->getNewLines()),
    'has_any_changes' => $parser->getHasAnyChanges(),
    'is_deleted' => $parser->getIsDeleted(),
    'intraline' => $intra,
    'depth_only' => pairs($parser->getDepthOnlyLines()),
    'lines_mask' => array_keys($parser->getVisibleLinesMask()),
    'blocks_mask' => array_values($blocks),
    'old_corpus' => $parser->getOldCorpus(),
    'new_corpus' => $parser->getNewCorpus(),
    'hunk_starts' => array_keys($parser->getHunkStartLines($hunks)),
  );
}

echo json_encode($output, JSON_PRETTY_PRINT | JSON_UNESCAPED_UNICODE), "\n";
