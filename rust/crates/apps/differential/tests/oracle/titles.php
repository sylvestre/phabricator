<?php
// Renders feed titles with PHP, for the expected values of tests/titles.rs:
//
//   python3 tests/oracle/cases.py
//   php tests/oracle/titles.php /path/to/libphutil \
//     < tests/oracle/titles.json > tests/oracle/titles.expected
//
// Each case is {"type", "old", "new", "metadata", "author"}. Handles are
// alice, bob, D1, #build, #two and a hidden user (see below); the output is
// the HTML and text title of each case, one JSON object per line.

list(, $libphutil) = $argv;
require_once $libphutil.'/support/init/init-script.php';
$root = dirname(__FILE__).'/../../../../../..';
phutil_load_library($root.'/src');
phutil_load_library($root.'/moz-extensions/src');

PhabricatorEnv::initializeScriptEnvironment(true);
$translator = PhutilTranslator::getInstance();
$translator->setLocale(PhutilLocale::loadLocale('en_US'));
$translator->setTranslations(
  PhutilTranslation::getTranslationMapForLocale('en_US'));

function new_handle($phid, $name, $uri) {
  $handle = new PhabricatorObjectHandle();
  $handle->setPHID($phid);
  $handle->setType(phid_get_type($phid));
  $handle->setName($name);
  $handle->setURI($uri);
  $handle->setPolicyFiltered(false);
  return $handle;
}

$handles = array(
  'PHID-USER-alice' => new_handle('PHID-USER-alice', 'alice', '/p/alice/'),
  'PHID-USER-bob' => new_handle('PHID-USER-bob', 'bob', '/p/bob/'),
  'PHID-DREV-1' => new_handle('PHID-DREV-1', 'D1: Fix it', '/D1'),
  'PHID-PROJ-build' => new_handle('PHID-PROJ-build', 'Build', '/tag/build/'),
  'PHID-PROJ-two' => new_handle('PHID-PROJ-two', 'Two', '/tag/two/'),
);
$hidden = new PhabricatorObjectHandle();
$hidden->setPHID('PHID-USER-hidden');
$hidden->setType('USER');
$hidden->setPolicyFiltered(true);
$handles['PHID-USER-hidden'] = $hidden;

// Handle lists load through the viewer's pool, which would query the
// database: fill it with the handles above.
$viewer = new PhabricatorUser();
$viewer->loadHandles(array());
$pool_property = new ReflectionProperty('PhabricatorUser', 'handlePool');
$pool_property->setAccessible(true);
$pool = $pool_property->getValue($viewer);
$handles_property = new ReflectionProperty('PhabricatorHandlePool', 'handles');
$handles_property->setAccessible(true);
$handles_property->setValue($pool, $handles);

$cases = json_decode(file_get_contents('php://stdin'), true);
foreach ($cases as $case) {
  $result = array();
  foreach (array('html', 'text') as $target) {
    $xaction = new DifferentialTransaction();
    $xaction->setAuthorPHID($case['author']);
    $xaction->setObjectPHID('PHID-DREV-1');
    $revision = DifferentialRevision::initializeNewRevision(
      PhabricatorUser::getOmnipotentUser());
    $revision->setPHID('PHID-DREV-1');
    $xaction->attachObject($revision);
    $xaction->setTransactionType($case['type']);
    $xaction->setOldValue($case['old']);
    $xaction->setNewValue($case['new']);
    foreach ($case['metadata'] as $key => $value) {
      $xaction->setMetadataValue($key, $value);
    }
    $xaction->setHandles($handles);
    $xaction->attachViewer($viewer);
    $xaction->setRenderingTarget($target);
    try {
      $title = $xaction->getTitleForFeed();
      $result[$target] = ($title === null) ? null : (string)$title;
    } catch (Throwable $ex) {
      $result[$target] = 'EXCEPTION: '.$ex->getMessage().(getenv('TRACE') ? $ex->getTraceAsString() : '');
    }
  }
  echo json_encode($result, JSON_UNESCAPED_SLASHES), "\n";
}
