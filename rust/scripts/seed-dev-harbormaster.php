<?php
// Seeds Harbormaster data to try the Rust Harbormaster pages with: a build
// plan with two steps (the second depending on the first), a new revision
// whose diff has a buildable, and a build of the plan with a target per
// step, a lint message, unit results of every kind, an external link and
// a text log. Goes through PHP's editors and Conduit methods. Run inside
// the phabricator container after scripts/seed-dev-stack.sh:
//
//   docker cp rust/scripts/seed-dev-harbormaster.php \
//     phabricator-phabricator-1:/tmp/seed-dev-harbormaster.php
//   docker exec phabricator-phabricator-1 sh -c \
//     'cd /app/phabricator && php /tmp/seed-dev-harbormaster.php'
//
// Each run adds a revision and a build; the plan is reused. The revisions
// of the write-parity scenarios (D1-D7, D50, D51) get no buildable.

require_once '/app/phabricator/scripts/init/init-script.php';

$omnipotent = PhabricatorUser::getOmnipotentUser();

function load_user($username) {
  $user = id(new PhabricatorPeopleQuery())
    ->setViewer(PhabricatorUser::getOmnipotentUser())
    ->withUsernames(array($username))
    ->needUserSettings(true)
    ->executeOne();
  if (!$user) {
    throw new Exception(
      "No user \"{$username}\": run scripts/seed-dev-stack.sh first.");
  }
  return $user;
}

function call($method, array $params, PhabricatorUser $user) {
  return id(new ConduitCall($method, $params))
    ->setUser($user)
    ->execute();
}

function content_source() {
  return PhabricatorContentSource::newForSource(
    PhabricatorConsoleContentSource::SOURCECONST);
}

/**
 * The plan named $name, created with `harbormaster.buildplan.edit` if it
 * doesn't exist, with its steps.
 */
function seed_plan($name, PhabricatorUser $admin) {
  $plan = id(new HarbormasterBuildPlan())->loadOneWhere('name = %s', $name);
  if (!$plan) {
    $result = call('harbormaster.buildplan.edit', array(
      'transactions' => array(
        array('type' => 'name', 'value' => $name),
      ),
    ), $admin);
    $plan = id(new HarbormasterBuildPlanQuery())
      ->setViewer(PhabricatorUser::getOmnipotentUser())
      ->withPHIDs(array($result['object']['phid']))
      ->executeOne();
  }

  $steps = id(new HarbormasterBuildStepQuery())
    ->setViewer(PhabricatorUser::getOmnipotentUser())
    ->withBuildPlanPHIDs(array($plan->getPHID()))
    ->execute();
  if (!$steps) {
    $run = seed_step(
      $plan,
      'Run the tests',
      "Starts the **try** push.",
      array('uri' => 'https://ci.example.test/run', 'method' => 'POST'),
      array(),
      $admin);
    seed_step(
      $plan,
      'Report the results',
      '',
      array('uri' => 'https://ci.example.test/report', 'method' => 'GET'),
      array($run->getPHID()),
      $admin);
    $steps = id(new HarbormasterBuildStepQuery())
      ->setViewer(PhabricatorUser::getOmnipotentUser())
      ->withBuildPlanPHIDs(array($plan->getPHID()))
      ->execute();
  }
  // Oldest first, the order the build runs them in.
  return array($plan, array_reverse($steps));
}

/**
 * An "HTTP Request" step, through `HarbormasterBuildStepEditor` as
 * `HarbormasterStepEditController` does.
 */
function seed_step(
  HarbormasterBuildPlan $plan,
  $name,
  $description,
  array $details,
  array $depends_on,
  PhabricatorUser $admin) {

  $step = HarbormasterBuildStep::initializeNewStep($admin)
    ->setBuildPlanPHID($plan->getPHID())
    ->setClassName('HarbormasterHTTPRequestBuildStepImplementation');
  foreach ($details as $key => $value) {
    $step->setDetail($key, $value);
  }
  $types = array(
    HarbormasterBuildStepTransaction::TYPE_CREATE => null,
    HarbormasterBuildStepTransaction::TYPE_NAME => $name,
    HarbormasterBuildStepTransaction::TYPE_DESCRIPTION => $description,
    HarbormasterBuildStepTransaction::TYPE_DEPENDS_ON => $depends_on,
  );
  $xactions = array();
  foreach ($types as $type => $value) {
    $xaction = id(new HarbormasterBuildStepTransaction())
      ->setTransactionType($type);
    if ($value !== null) {
      $xaction->setNewValue($value);
    }
    $xactions[] = $xaction;
  }
  id(new HarbormasterBuildStepEditor())
    ->setActor($admin)
    ->setContentSource(content_source())
    ->setContinueOnNoEffect(true)
    ->applyTransactions($step, $xactions);
  return $step;
}

$admin = load_user('admin');
$alice = load_user('alice');

list($plan, $steps) = seed_plan('Try Build', $admin);

// A new revision by Alice, which nothing else uses.
$diff = <<<'DIFF'
diff --git a/dom/base/Harbor.cpp b/dom/base/Harbor.cpp
new file mode 100644
--- /dev/null
+++ b/dom/base/Harbor.cpp
@@ -0,0 +1,5 @@
+#include "mozilla/dom/Harbor.h"
+
+int Harbor::Ships() {
+  return 3;
+}
DIFF;
$raw = call('differential.createrawdiff', array('diff' => $diff), $alice);
$created = call('differential.revision.edit', array(
  'transactions' => array(
    array('type' => 'update', 'value' => $raw['phid']),
    array('type' => 'title', 'value' => 'Bug 1890100 - Count the ships in the harbor'),
    array('type' => 'summary', 'value' => 'A revision with builds, for the Harbormaster pages.'),
    array('type' => 'testPlan', 'value' => 'See the build.'),
  ),
), $alice);
call('differential.revision.edit', array(
  'objectIdentifier' => $created['object']['phid'],
  'transactions' => array(array('type' => 'request-review', 'value' => true)),
), $alice);
$revision_id = $created['object']['id'];

// The automatic buildable of the diff (Herald may have made it already),
// and a build of the plan. The build is made like applyPlan() makes one,
// but already building, so no daemon runs its HTTP steps: its targets wait
// for the messages sent below.
$buildable = HarbormasterBuildable::createOrLoadExisting(
  $omnipotent,
  $raw['phid'],
  $created['object']['phid']);
$buildable
  ->setBuildableStatus(HarbormasterBuildableStatus::STATUS_BUILDING)
  ->save();

$build = HarbormasterBuild::initializeNewBuild($omnipotent)
  ->setBuildablePHID($buildable->getPHID())
  ->setBuildPlanPHID($plan->getPHID())
  ->setBuildParameters(array())
  ->setInitiatorPHID($admin->getPHID())
  ->setBuildStatus(HarbormasterBuildStatus::STATUS_BUILDING)
  ->save();
// Loaded back with its buildable and the diff, which the variables need.
$build = id(new HarbormasterBuildQuery())
  ->setViewer($omnipotent)
  ->withPHIDs(array($build->getPHID()))
  ->executeOne();
$variables = $build->retrieveVariablesFromBuild();

$now = PhabricatorTime::getNow();
$targets = array();
foreach ($steps as $step) {
  $targets[] = HarbormasterBuildTarget::initializeNewBuildTarget(
    $build,
    $step,
    $variables)
    ->setTargetStatus(HarbormasterBuildTarget::STATUS_WAITING)
    ->setDateStarted($now - 300)
    ->save();
}
list($run_target, $report_target) = $targets;

call('harbormaster.createartifact', array(
  'buildTargetPHID' => $run_target->getPHID(),
  'artifactKey' => 'treeherder',
  'artifactType' => 'uri',
  'artifactData' => array(
    'uri' => 'https://treeherder.mozilla.org/jobs?repo=try&revision=0123456789ab',
    'name' => 'Treeherder Jobs',
    'ui.external' => true,
  ),
), $admin);

$log = HarbormasterBuildLog::initializeNewBuildLog($run_target)
  ->setLogSource('try')
  ->setLogType('stdout')
  ->openBuildLog();
for ($line = 1; $line <= 40; $line++) {
  $log->append("[task {$line}] mach test dom/base: ok\n");
}
$log->append("[task 41] TEST-UNEXPECTED-FAIL | test_harbor.html | ships\n");
$log->closeBuildLog();

// The results, as code-review bots send them.
call('harbormaster.sendmessage', array(
  'receiver' => $run_target->getPHID(),
  'type' => 'fail',
  'lint' => array(
    array(
      'name' => 'Missing semicolon',
      'code' => 'clang-format',
      'severity' => 'error',
      'path' => 'dom/base/Harbor.cpp',
      'line' => 4,
      'char' => 11,
    ),
    array(
      'name' => 'Function could be const',
      'code' => 'readability-make-member-function-const',
      'severity' => 'warning',
      'path' => 'dom/base/Harbor.cpp',
      'line' => 3,
    ),
  ),
  'unit' => array(
    array(
      'name' => 'test_ships',
      'namespace' => 'dom/base/test/test_harbor.html',
      'engine' => 'mochitest',
      'result' => 'fail',
      'duration' => 1.25,
      'details' => "Expected 3 ships, got 2.\nStack:\n  test_harbor.html:12\n  harness.js:40",
    ),
    array(
      'name' => 'test_docks',
      'engine' => 'mochitest',
      'result' => 'broken',
      'details' => "The harness **crashed**: see the `try` log.",
      'format' => 'remarkup',
    ),
    array(
      'name' => 'test_tides',
      'engine' => 'gtest',
      'result' => 'unsound',
      'duration' => 0.004,
    ),
    array(
      'name' => 'test_anchors',
      'engine' => 'gtest',
      'result' => 'pass',
      'duration' => 12.5,
    ),
    array(
      'name' => 'test_lighthouse',
      'engine' => 'gtest',
      'result' => 'skip',
      'details' => 'Skipped on this platform.',
    ),
    array(
      'name' => 'test_buoys',
      'engine' => 'gtest',
      'result' => 'pass',
    ),
  ),
), $admin);
call('harbormaster.sendmessage', array(
  'receiver' => $report_target->getPHID(),
  'type' => 'pass',
), $admin);

$base = rtrim(PhabricatorEnv::getProductionURI('/'), '/');
echo "Plan:      {$base}/harbormaster/plan/{$plan->getID()}/\n";
echo "Revision:  {$base}/D{$revision_id}\n";
echo "Buildable: {$base}/B{$buildable->getID()}\n";
echo "Build:     {$base}/harbormaster/build/{$build->getID()}/\n";
