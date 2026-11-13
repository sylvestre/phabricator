<?php
// Renders random Remarkup with PHP's engine, for tests/oracle.rs:
//
//   php tests/oracle/generate.php /path/to/libphutil COUNT SEED > cases.json
//   PHAB_REMARKUP_ORACLE=cases.json cargo test -p phab-remarkup --test oracle
//
// The engine is the one of PhutilRemarkupEngineTestCase. libphutil is the
// arcanist checkout the Dockerfile clones.

list(, $libphutil, $count, $seed) = $argv;
require_once $libphutil.'/support/init/init-script.php';
phutil_load_library(dirname(__FILE__).'/../../../../../src');

function new_engine() {
  $engine = new PhutilRemarkupEngine();
  $engine->setConfig(
    'uri.allowed-protocols',
    array('http' => true, 'mailto' => true, 'tel' => true));
  $rules = array(
    new PhutilRemarkupEscapeRemarkupRule(),
    new PhutilRemarkupMonospaceRule(),
    new PhutilRemarkupDocumentLinkRule(),
    new PhutilRemarkupHyperlinkRule(),
    new PhutilRemarkupBoldRule(),
    new PhutilRemarkupItalicRule(),
    new PhutilRemarkupDelRule(),
    new PhutilRemarkupUnderlineRule(),
    new PhutilRemarkupHighlightRule(),
  );
  $blocks = array(
    new PhutilRemarkupQuotesBlockRule(),
    new PhutilRemarkupReplyBlockRule(),
    new PhutilRemarkupHeaderBlockRule(),
    new PhutilRemarkupHorizontalRuleBlockRule(),
    new PhutilRemarkupCodeBlockRule(),
    new PhutilRemarkupLiteralBlockRule(),
    new PhutilRemarkupNoteBlockRule(),
    new PhutilRemarkupTableBlockRule(),
    new PhutilRemarkupSimpleTableBlockRule(),
    new PhutilRemarkupDefaultBlockRule(),
    new PhutilRemarkupListBlockRule(),
  );
  foreach ($blocks as $block) {
    if (!($block instanceof PhutilRemarkupCodeBlockRule)) {
      $block->setMarkupRules($rules);
    }
  }
  $engine->setBlockRules($blocks);
  return $engine;
}

$fragments = array(
  '**', '//', '`', '##', '~~', '__', '!!', '- ', '-- ', '* ', '# ', '## ',
  '1. ', '3) ', '[ ] ', '[x] ', '  ', '    ', "\n", "\n", "\n\n", '> ',
  '>>! In D1, a wrote:', '| a | b |', "|---|---|", '<table><tr><td>c</td></tr></table>',
  '<td>', 'http://example.com/a(b)', 'https://x.org/.', '<http://e.com/>',
  '{http://e.com/}', 'javascript://x', 'mailto:a@b.c', '[[ /x | y ]]',
  '[[ http://a.com/ ]]', '[n](http://b.com/c)', '[x](y)', 'NOTE: ',
  '(IMPORTANT) ', 'WARNING:', '%%%', '```', 'lang=php', 'name=f.c', '=', '==',
  '---', '***', '___', '{anchor #x}', '<', '>', '&', '"', "'", 'é', '(', ')',
  'word', 'two words', 'x.y', ':', '|', '[', ']', '#', '0', 'counterexample',
  "\t", '$ ls', 'D123', '@alice',
);

mt_srand((int)$seed);
$cases = array();
for ($ii = 0; $ii < (int)$count; $ii++) {
  $text = '';
  $length = mt_rand(1, 25);
  for ($jj = 0; $jj < $length; $jj++) {
    $text .= $fragments[mt_rand(0, count($fragments) - 1)];
  }
  $case = array('input' => $text);
  foreach (array('html' => PhutilRemarkupEngine::MODE_DEFAULT,
                 'text' => PhutilRemarkupEngine::MODE_TEXT) as $key => $mode) {
    try {
      $engine = new_engine();
      $engine->setMode($mode);
      $case[$key] = (string)$engine->markupText($text);
    } catch (Throwable $ex) {
      $case[$key] = null;
    }
  }
  $cases[] = $case;
}
echo json_encode($cases, JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES), "\n";
