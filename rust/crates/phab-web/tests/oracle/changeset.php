<?php

// Writes changeset.json: what PHP's /differential/changeset/ answers to the
// Ajax requests of its revision pages (a changeset, and its "show more"
// context), with the parse cache it stores, for tests/changeset_pages.rs.
//
// It runs the repository's DifferentialChangesetParser and renderers on
// changesets built in memory, without a database (pygments.enabled is
// turned off, which only changes the cached highlighting: since Bug 2093445
// it defaults to on, and every language is plain text without it), and
// answers as
// DifferentialChangesetViewController and AphrontAjaxResponse do. Arcanist
// must be checked out next to the repository, or in ARCANIST_ROOT:
//
//   ARCANIST_ROOT=/path/to/arcanist \
//     php rust/crates/phab-web/tests/oracle/changeset.php \
//     > rust/crates/phab-web/tests/oracle/changeset.json

$root = dirname(__DIR__, 5);
$arcanist = getenv('ARCANIST_ROOT');
if (!$arcanist) {
  $arcanist = dirname($root).'/arcanist';
}

require_once $arcanist.'/support/init/init-script.php';
phutil_load_library($arcanist.'/src');
phutil_load_library($root.'/src');
PhabricatorEnv::initializeScriptEnvironment(true);

$oracle_env = PhabricatorEnv::beginScopedEnv();
$oracle_env->overrideEnvConfig('pygments.enabled', false);

// The Ajax requests carry the page's metadata block.
$_REQUEST['__metablock__'] = '3';

/** A hunk from its lines, each with its marker. */
function oracle_hunk($old_offset, $new_offset, array $lines) {
  $old_len = 0;
  $new_len = 0;
  foreach ($lines as $line) {
    switch ($line[0]) {
      case ' ':
        $old_len++;
        $new_len++;
        break;
      case '-':
        $old_len++;
        break;
      case '+':
        $new_len++;
        break;
    }
  }
  return array(
    'oldOffset' => $old_offset,
    'oldLen' => $old_len,
    'newOffset' => $new_offset,
    'newLen' => $new_len,
    'changes' => implode('', $lines),
  );
}

/** Context lines. */
function oracle_context($from, $to, $format) {
  $lines = array();
  for ($ii = $from; $ii <= $to; $ii++) {
    $lines[] = ' '.sprintf($format, $ii)."\n";
  }
  return $lines;
}

function oracle_changeset(array $spec) {
  $changeset = id(new DifferentialChangeset())
    ->setID($spec['id'])
    ->setDiffID(7)
    ->setFilename($spec['filename'])
    ->setOldFile(idx($spec, 'oldFile'))
    ->setAwayPaths(idx($spec, 'awayPaths', array()))
    ->setChangeType(idx($spec, 'changeType', DifferentialChangeType::TYPE_CHANGE))
    ->setFileType(DifferentialChangeType::FILE_TEXT)
    ->setMetadata(idx($spec, 'metadata', array()))
    ->setOldProperties(array())
    ->setNewProperties(array());

  $hunks = array();
  $add = 0;
  $del = 0;
  foreach ($spec['hunks'] as $hunk_spec) {
    $hunk = id(new DifferentialHunk())
      ->setOldOffset($hunk_spec['oldOffset'])
      ->setOldLen($hunk_spec['oldLen'])
      ->setNewOffset($hunk_spec['newOffset'])
      ->setNewLen($hunk_spec['newLen'])
      ->setChanges($hunk_spec['changes']);
    $add += count($hunk->getAddedLines());
    $del += count($hunk->getRemovedLines());
    $hunks[] = $hunk;
  }
  $changeset->attachHunks($hunks);
  $changeset->setAddLines(idx($spec, 'addLines', $add));
  $changeset->setDelLines(idx($spec, 'delLines', $del));
  return $changeset;
}

/** A parser as the controller configures it for one changeset. */
function oracle_parser(DifferentialChangeset $changeset, $reference) {
  $viewer = new PhabricatorUser();
  // As PhabricatorChangesetViewStateEngine makes it for a viewer who
  // picked the side by side renderer, and didn't hide the changeset.
  $viewstate = id(new PhabricatorChangesetViewState())
    ->setRendererKey('2up')
    ->setHidden(false);

  $engine = id(new PhabricatorMarkupEngine())
    ->setViewer($viewer);
  $engine->process();

  return id(new DifferentialChangesetParser())
    ->setViewer($viewer)
    ->setViewState($viewstate)
    ->setCoverage(null)
    ->setChangeset($changeset)
    ->setRenderingReference($reference)
    ->setRenderCacheKey(null)
    ->setRightSideCommentMapping($changeset->getID(), true)
    ->setLeftSideCommentMapping($changeset->getID(), false)
    ->setHandles(array())
    ->setMarkupEngine($engine)
    ->setShowEditAndReplyLinks(true)
    ->setCanMarkDone(false)
    ->setObjectOwnerPHID(null);
}

/** What saveCache() stores once the parser processed the changeset. */
function oracle_cache(DifferentialChangesetParser $parser) {
  $read = function() {
    $cache = array();
    foreach (self::getCacheableProperties() as $key) {
      switch ($key) {
        case 'cacheVersion':
          $cache[$key] = self::CACHE_VERSION;
          break;
        case 'cacheHost':
          $cache[$key] = 'oracle';
          break;
        default:
          $cache[$key] = $this->$key;
          break;
      }
    }
    return serialize($cache);
  };
  return Closure::bind($read, $parser, 'DifferentialChangesetParser')->__invoke();
}

function oracle_case(array $spec) {
  $reference = (string)$spec['id'];

  // The full parse, as a request for every row stores it.
  $changeset = oracle_changeset($spec);
  $parser = oracle_parser($changeset, $reference);
  $rows = 100000;
  $parser->setRange(0, $rows)->setMask(array());
  $parser->renderChangeset();
  $cache = oracle_cache($parser);
  // That render's metadata isn't part of the answer.
  CelerityAPI::getStaticResourceResponse()->buildAjaxResponse(null);

  // The request.
  $changeset = oracle_changeset($spec);
  $range = idx($spec, 'range');
  list($range_s, $range_e, $mask) =
    DifferentialChangesetParser::parseRangeSpecification($range);
  $parser = oracle_parser($changeset, $reference)
    ->setRange($range_s, $range_e)
    ->setMask($mask);

  $response = $parser->newChangesetResponse();
  $mcov = $parser->renderModifiedCoverage();
  $response->setCoverage(
    array(
      'differential-mcoverage-'.md5($changeset->getFilename()) => $mcov,
    ));
  $answer = $response->reduceProxyResponse()->buildResponseString();

  $hunks = array();
  foreach ($changeset->getHunks() as $hunk) {
    $hunks[] = array(
      'oldOffset' => (int)$hunk->getOldOffset(),
      'oldLen' => (int)$hunk->getOldLen(),
      'newOffset' => (int)$hunk->getNewOffset(),
      'newLen' => (int)$hunk->getNewLen(),
      'changes' => $hunk->getChanges(),
      'dataEncoding' => $hunk->getDataEncoding(),
    );
  }

  return array(
    'name' => $spec['name'],
    'changeset' => array(
      'id' => (int)$changeset->getID(),
      'filename' => $changeset->getFilename(),
      'oldFile' => $changeset->getOldFile(),
      'awayPaths' => $changeset->getAwayPaths(),
      'changeType' => (int)$changeset->getChangeType(),
      'metadata' => (object)$changeset->getMetadata(),
      'addLines' => (int)$changeset->getAddLines(),
      'delLines' => (int)$changeset->getDelLines(),
    ),
    'hunks' => $hunks,
    'range' => $range,
    'cache' => base64_encode($cache),
    'answer' => $answer,
  );
}

// A C file with a function, changed in the middle, with every line of
// context: gaps above and below, with scope lines.
$body = array();
$body[] = " #include <stdio.h>\n";
$body = array_merge($body, oracle_context(2, 30, '// header comment %d'));
$body[] = " int main(int argc, char **argv) {\n";
$body = array_merge($body, oracle_context(32, 70, '  int value_%d = 0;'));
$body[] = "-  return value_70 < 1;\n";
$body[] = "+  return value_70 <= 1 && argc > 0;\n";
$body = array_merge($body, oracle_context(72, 120, '  value_%d++;'));
$body[] = " }\n";
$full_context = oracle_hunk(1, 1, $body);

$specs = array();
$specs[] = array(
  'name' => 'a changed file',
  'id' => 101,
  'filename' => 'src/main.c',
  'hunks' => array($full_context),
);
$specs[] = array(
  'name' => 'the top of the first gap',
  'id' => 101,
  'filename' => 'src/main.c',
  'hunks' => array($full_context),
  'range' => '0-61/0-20',
);
$specs[] = array(
  'name' => 'the last lines of the first gap',
  'id' => 101,
  'filename' => 'src/main.c',
  'hunks' => array($full_context),
  'range' => '0-61/41-20',
);
$specs[] = array(
  'name' => 'all of the last gap',
  'id' => 101,
  'filename' => 'src/main.c',
  'hunks' => array($full_context),
  'range' => '79-42/79-42',
);
$specs[] = array(
  'name' => 'the whole file',
  'id' => 101,
  'filename' => 'src/main.c',
  'hunks' => array($full_context),
  'range' => '0-122',
);

// Partial context: two hunks, the first not at the top.
$specs[] = array(
  'name' => 'hunks without all the context',
  'id' => 102,
  'filename' => 'lib/util.py',
  'hunks' => array(
    oracle_hunk(
      10,
      10,
      array_merge(
        oracle_context(10, 12, 'x%d = 1'),
        array("-y = 2\n", "+y = 3\n", "+z = 4\n"),
        oracle_context(14, 16, 'x%d = 1'))),
    oracle_hunk(
      40,
      41,
      array_merge(
        oracle_context(40, 42, 'w%d = 5'),
        array("-\tq = 'a & b'\n", "+\tq = \"<a> & b\"\n"),
        oracle_context(44, 46, 'w%d = 5'))),
  ),
);

// Added, with tabs and without a final newline.
$specs[] = array(
  'name' => 'an added file',
  'id' => 103,
  'filename' => 'docs/new.txt',
  'changeType' => DifferentialChangeType::TYPE_ADD,
  'hunks' => array(
    oracle_hunk(
      0,
      1,
      array(
        "+first line\n",
        "+\tindented\tline\n",
        "+\xC3\xA9t\xC3\xA9 \xE2\x80\x94 unicode\n",
        "+last line\n",
        "\\ No newline at end of file\n",
      )),
  ),
);

// Shields.
$specs[] = array(
  'name' => 'a deleted file',
  'id' => 104,
  'filename' => 'old/gone.js',
  'changeType' => DifferentialChangeType::TYPE_DELETE,
  'hunks' => array(
    oracle_hunk(1, 0, array("-var a = 1;\n", "-var b = 2;\n")),
  ),
);
$specs[] = array(
  'name' => 'the contents of a deleted file',
  'id' => 104,
  'filename' => 'old/gone.js',
  'changeType' => DifferentialChangeType::TYPE_DELETE,
  'hunks' => array(
    oracle_hunk(1, 0, array("-var a = 1;\n", "-var b = 2;\n")),
  ),
  'range' => '0-2',
);
$specs[] = array(
  'name' => 'a generated file',
  'id' => 105,
  'filename' => 'gen/parser.rs',
  'hunks' => array(
    oracle_hunk(
      1,
      1,
      array(" // @"."generated by a tool\n", "-fn a() {}\n", "+fn b() {}\n")),
  ),
);
$specs[] = array(
  'name' => 'a very large change',
  'id' => 106,
  'filename' => 'big/data.csv',
  'addLines' => 2000,
  'delLines' => 1000,
  'hunks' => array(oracle_hunk(1, 1, array("-a,b\n", "+a,c\n"))),
);
$specs[] = array(
  'name' => 'a file moved away',
  'id' => 107,
  'filename' => 'from/here.c',
  'changeType' => DifferentialChangeType::TYPE_MOVE_AWAY,
  'awayPaths' => array('to/there.c'),
  'hunks' => array(),
);
$specs[] = array(
  'name' => 'a file moved here, with copied lines',
  'id' => 108,
  'filename' => 'to/there.c',
  'oldFile' => 'from/here.c',
  'changeType' => DifferentialChangeType::TYPE_MOVE_HERE,
  'metadata' => array(
    'copy:lines' => array(
      2 => array('from/here.c', 7, '-'),
      3 => array('', 1, '+'),
    ),
  ),
  'hunks' => array(
    oracle_hunk(
      1,
      1,
      array(
        " int a;\n",
        "-int b;\n",
        "+int moved;\n",
        "+int copied;\n",
        " int c;\n",
      )),
  ),
);
$specs[] = array(
  'name' => 'an indentation change',
  'id' => 109,
  'filename' => 'src/indent.py',
  'hunks' => array(
    oracle_hunk(
      1,
      1,
      array(
        " def f():\n",
        "-    if x:\n",
        "-        return 1\n",
        "+    if x:\n",
        "+            return 1\n",
        " \n",
        "-  return 2\n",
        "+    return 2\n",
      )),
  ),
);

$cases = array();
foreach ($specs as $spec) {
  $cases[] = oracle_case($spec);
}

echo id(new PhutilJSON())->encodeAsList($cases);
