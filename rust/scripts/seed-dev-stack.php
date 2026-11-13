<?php
// Seeds the development database with users, projects and revisions in
// various states, going through PHP's own editors and Conduit methods so
// Herald, feed and policies behave as in production. Run inside the
// phabricator container; scripts/seed-dev-stack.sh does it.

require_once '/app/phabricator/scripts/init/init-script.php';

$omnipotent = PhabricatorUser::getOmnipotentUser();

function seed_user($username, $real_name) {
  $user = id(new PhabricatorUser())->loadOneWhere('userName = %s', $username);
  if ($user) {
    return $user;
  }
  $user = id(new PhabricatorUser())
    ->setUsername($username)
    ->setRealName($real_name)
    ->setIsApproved(1);
  $email = id(new PhabricatorUserEmail())
    ->setAddress($username.'@example.com')
    ->setIsVerified(1);
  id(new PhabricatorUserEditor())
    ->setActor(PhabricatorUser::getOmnipotentUser())
    ->createNewUser($user, $email);
  return $user;
}

function reload_user(PhabricatorUser $user) {
  return id(new PhabricatorPeopleQuery())
    ->setViewer(PhabricatorUser::getOmnipotentUser())
    ->withPHIDs(array($user->getPHID()))
    ->needUserSettings(true)
    ->executeOne();
}

function call($method, array $params, PhabricatorUser $user) {
  return id(new ConduitCall($method, $params))
    ->setUser($user)
    ->execute();
}

function seed_project($name, array $members, PhabricatorUser $actor) {
  $existing = id(new PhabricatorProjectQuery())
    ->setViewer(PhabricatorUser::getOmnipotentUser())
    ->withNames(array($name))
    ->executeOne();
  if ($existing) {
    return $existing->getPHID();
  }
  $result = call('project.edit', array(
    'transactions' => array(
      array('type' => 'name', 'value' => $name),
      array('type' => 'members.add', 'value' => mpull($members, 'getPHID')),
    ),
  ), $actor);
  return $result['object']['phid'];
}

function seed_revision(
  PhabricatorUser $author,
  $title,
  $summary,
  $test_plan,
  $diff,
  array $extra) {

  $raw = call('differential.createrawdiff', array('diff' => $diff), $author);
  $transactions = array(
    array('type' => 'update', 'value' => $raw['phid']),
    array('type' => 'title', 'value' => $title),
    array('type' => 'summary', 'value' => $summary),
    array('type' => 'testPlan', 'value' => $test_plan),
  );
  foreach ($extra as $transaction) {
    $transactions[] = $transaction;
  }
  $result = call('differential.revision.edit', array(
    'transactions' => $transactions,
  ), $author);
  // New revisions start as drafts; ask for review like `moz-phab` does.
  call('differential.revision.edit', array(
    'objectIdentifier' => $result['object']['phid'],
    'transactions' => array(array('type' => 'request-review', 'value' => true)),
  ), $author);
  return array($result['object']['id'], $result['object']['phid'], $raw['id']);
}

function act($id, PhabricatorUser $user, array $transactions) {
  call('differential.revision.edit', array(
    'objectIdentifier' => 'D'.$id,
    'transactions' => $transactions,
  ), $user);
}

function inline($revision_id, $diff_id, $path, $line, $content, PhabricatorUser $user, $is_new = true) {
  call('differential.createinline', array(
    'revisionID' => $revision_id,
    'diffID' => $diff_id,
    'filePath' => $path,
    'isNewFile' => $is_new,
    'lineNumber' => $line,
    'lineLength' => 0,
    'content' => $content,
  ), $user);
}

function publish_inlines($revision_id, $message, PhabricatorUser $user) {
  call('differential.createcomment', array(
    'revision_id' => $revision_id,
    'message' => $message,
    'attach_inlines' => true,
  ), $user);
}

$admin = reload_user(id(new PhabricatorUser())->loadOneWhere('userName = %s', 'admin'));
$alice = reload_user(seed_user('alice', 'Alice Liddell'));
$bob = reload_user(seed_user('bob', 'Bob Builder'));
$carol = reload_user(seed_user('carol', 'Carol Danvers'));
$dave = reload_user(seed_user('dave', 'Dave Lister'));

$build = seed_project('Build Reviewers', array($bob, $carol), $admin);
$frontend = seed_project('Frontend Reviewers', array($alice, $dave), $admin);
$secure = id(new PhabricatorProjectQuery())
  ->setViewer($omnipotent)
  ->withNames(array('secure-revision'))
  ->executeOne()
  ->getPHID();

/**
 * A git-style diff of files, from their old and new contents (`null` for
 * an added file), made with `diff -u` so the hunks are right.
 */
function make_diff(array $files) {
  $out = '';
  foreach ($files as $path => $contents) {
    list($old, $new) = $contents;
    $dir = sys_get_temp_dir().'/seed-'.mt_rand();
    mkdir($dir);
    file_put_contents($dir.'/old', (string)$old);
    file_put_contents($dir.'/new', $new);
    list(, $stdout) = exec_manual('diff -U 3 %s %s', $dir.'/old', $dir.'/new');
    Filesystem::remove($dir);
    $lines = phutil_split_lines($stdout);
    array_shift($lines);
    array_shift($lines);
    $out .= "diff --git a/{$path} b/{$path}\n";
    if ($old === null) {
      $out .= "new file mode 100644\n--- /dev/null\n";
    } else {
      $out .= "--- a/{$path}\n";
    }
    $out .= "+++ b/{$path}\n".implode('', $lines);
  }
  return $out;
}

$element_old = <<<'CPP'
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. */

#include "mozilla/dom/Element.h"
#include "mozilla/dom/Document.h"

namespace mozilla::dom {

nsresult Element::SetAttribute(const nsAString& aName,
                               const nsAString& aValue) {
  if (aName.IsEmpty()) {
    return NS_ERROR_INVALID_ARG;
  }
  return SetAttr(kNameSpaceID_None, aName, aValue, true);
}

nsresult Element::RemoveAttribute(const nsAString& aName) {
  return UnsetAttr(kNameSpaceID_None, aName, true);
}

}  // namespace mozilla::dom
CPP;
$element_new = str_replace(
  "  if (aName.IsEmpty()) {\n    return NS_ERROR_INVALID_ARG;\n  }\n",
  "  if (MOZ_UNLIKELY(aName.IsEmpty())) {\n    return NS_ERROR_DOM_INVALID_CHARACTER_ERR;\n  }\n  MOZ_ASSERT(IsInComposedDoc() || !GetParent());\n",
  $element_old);
$test_html = <<<'HTML'
<!DOCTYPE html>
<script>
  const div = document.createElement("div");
  SimpleTest.doesThrow(
    () => div.setAttribute("", "x"),
    "An empty attribute name throws"
  );
</script>
HTML;
$element_diff = make_diff(array(
  'dom/base/Element.cpp' => array($element_old, $element_new),
  'dom/base/test/test_setAttribute.html' => array(null, $test_html),
));

$moz_build_old = "# -*- Mode: python; indent-tabs-mode: nil; tab-width: 40 -*-\n\nMOCHITEST_MANIFESTS += [\n    \"test/mochitest.toml\",\n]\n\nFINAL_LIBRARY = \"xul\"\n";
$moz_build_diff = make_diff(array(
  'dom/base/moz.build' => array(
    $moz_build_old,
    str_replace("\"test/mochitest.toml\",\n", "\"test/mochitest.toml\",\n    \"test/attributes/mochitest.toml\",\n", $moz_build_old),
  ),
));

$python_old = <<<'PY'
class BuildBackend:
    """Base class for build backends."""

    def consume(self, objs):
        """Consume a stream of TreeMetadata instances."""
        for obj in objs:
            self.consume_object(obj)
        self.consume_finished()

    def consume_object(self, obj):
        raise NotImplementedError

    def consume_finished(self):
        pass
PY;
$python_diff = make_diff(array(
  'python/mozbuild/mozbuild/backend/base.py' => array(
    $python_old,
    str_replace(
      "            self.consume_object(obj)\n        self.consume_finished()\n",
      "            if not self.consume_object(obj):\n                raise Exception(\"Unhandled object: %r\" % obj)\n        return self.consume_finished()\n",
      $python_old),
  ),
));

$http_old = <<<'CPP'
nsresult nsHttpChannel::ProcessResponse() {
  uint32_t httpStatus = mResponseHead->Status();
  if (httpStatus == 304) {
    return ProcessNotModified();
  }
  return ContinueProcessResponse1();
}
CPP;
$security_diff = make_diff(array(
  'netwerk/protocol/http/nsHttpChannel.cpp' => array(
    $http_old,
    str_replace("httpStatus == 304)", "httpStatus == 304 && mCachedResponseHead)", $http_old),
  ),
));
$security_child_diff = make_diff(array(
  'netwerk/protocol/http/nsHttpChannel.cpp' => array(
    $http_old,
    str_replace(
      "  return ContinueProcessResponse1();\n",
      "  if (!mResponseHead->HasHeader(nsHttp::Content_Type)) {\n    mResponseHead->SetContentType(\"application/octet-stream\"_ns);\n  }\n  return ContinueProcessResponse1();\n",
      $http_old),
  ),
));

$css_old = ".tabbrowser-tab {\n  min-height: var(--tab-min-height);\n  padding-inline: 2px;\n  border-radius: var(--tab-border-radius);\n}\n";
$css_diff = make_diff(array(
  'browser/themes/shared/tabbrowser/tabs.css' => array(
    $css_old,
    str_replace("padding-inline: 2px;", "padding-inline: var(--tab-padding-inline, 2px);", $css_old),
  ),
));

// D1: Alice's change, waiting on Bob and on the Build Reviewers group
// (blocking); Bob and Carol review it with inline comments.
list($d1, $d1_phid, $d1_diff) = seed_revision(
  $alice,
  'Bug 1890001 - Throw InvalidCharacterError for empty attribute names',
  "The spec says `setAttribute(\"\", x)` throws an `InvalidCharacterError`.\n\n".
  "  - Matches the WHATWG DOM spec.\n  - See bug 1890001 comment 3.\n\nDepends on nothing; @bob knows this code.",
  "Added `test_setAttribute.html`; `./mach test dom/base/test` passes.",
  $element_diff,
  array(
    array('type' => 'reviewers.set', 'value' => array($bob->getPHID(), 'blocking('.$build.')')),
    array('type' => 'projects.add', 'value' => array($frontend)),
  ));
inline($d1, $d1_diff, 'dom/base/Element.cpp', 12, "Is this **really** the right error? `NS_ERROR_DOM_INVALID_CHARACTER_ERR` is for names with invalid characters.", $bob);
inline($d1, $d1_diff, 'dom/base/Element.cpp', 14, "This assertion fires for detached elements in shadow trees, see bug 1700000.", $bob);
publish_inlines($d1, "A couple of questions inline. The test is nice.", $bob);
inline($d1, $d1_diff, 'dom/base/test/test_setAttribute.html', 5, "nit: also check `setAttributeNS`.", $carol);
publish_inlines($d1, "Looks reasonable from the build side.", $carol);

// D2: depends on D1, accepted by the admin: ready to land.
list($d2, $d2_phid) = seed_revision(
  $alice,
  'Bug 1890002 - Add the attribute tests to the mochitest manifests',
  'Registers the new directory so the tests run in CI.',
  'Try push: green.',
  $moz_build_diff,
  array(
    array('type' => 'reviewers.set', 'value' => array($admin->getPHID())),
    array('type' => 'parents.set', 'value' => array($d1_phid)),
  ));
act($d2, $admin, array(
  array('type' => 'comment', 'value' => 'LGTM, thanks!'),
  array('type' => 'accept', 'value' => true),
));

// D3: Bob's change; the admin requests changes, Alice still has to review.
list($d3) = seed_revision(
  $bob,
  'Bug 1890003 - Fail the build backend on unhandled objects',
  "Silently ignoring objects hid real bugs.\n\n```lang=python\nif not self.consume_object(obj):\n    raise Exception(obj)\n```",
  'Ran `./mach build` with the artifact backend.',
  $python_diff,
  array(
    array('type' => 'reviewers.set', 'value' => array($admin->getPHID(), $alice->getPHID())),
    array('type' => 'projects.add', 'value' => array($build)),
  ));
act($d3, $admin, array(
  array('type' => 'comment', 'value' => "This breaks the `FasterMake` backend, which returns `None`.\n\n> Silently ignoring objects hid real bugs.\n\nAgreed, but we need a migration first."),
  array('type' => 'reject', 'value' => true),
));

// D4: a secure revision by Carol, with a public child depending on it.
list($d4, $d4_phid) = seed_revision(
  $carol,
  'Bug 1890004 - Check the cached response head before processing a 304',
  'Avoids using a null response head.',
  'Covered by the crashtest in the security bug.',
  $security_diff,
  array(
    array('type' => 'reviewers.set', 'value' => array($admin->getPHID(), $dave->getPHID())),
    array('type' => 'projects.add', 'value' => array($secure)),
  ));
seed_revision(
  $carol,
  'Bug 1890005 - Default the content type of responses without one',
  'Follow-up of D'.$d4.'.',
  'Manual testing.',
  $security_child_diff,
  array(
    array('type' => 'reviewers.set', 'value' => array($dave->getPHID())),
    array('type' => 'parents.set', 'value' => array($d4_phid)),
  ));

// D6: Dave's change, abandoned.
list($d6) = seed_revision(
  $dave,
  'Bug 1890006 - Make the tab padding configurable',
  'Superseded by the new tab strip.',
  'None.',
  $css_diff,
  array(
    array('type' => 'reviewers.set', 'value' => array($alice->getPHID(), $frontend)),
  ));
act($d6, $dave, array(
  array('type' => 'comment', 'value' => 'Abandoning, the tab strip rewrite covers this.'),
  array('type' => 'abandon', 'value' => true),
));

// D7: Dave's change waiting on the Frontend Reviewers group (Alice).
seed_revision(
  $dave,
  'Bug 1890007 - Use the tab padding variable in the compact density',
  "Small follow-up.\n\nNOTE: Needs the theme change of D".$d6." to be relanded.",
  'Checked compact and normal density.',
  $css_diff,
  array(
    array('type' => 'reviewers.set', 'value' => array($frontend)),
  ));

// Bugzilla bug IDs, set by the admin, whom the BMO check doesn't apply to.
$bugs = array($d1 => 1890001, $d2 => 1890002, $d3 => 1890003, $d4 => 1890004);
foreach ($bugs as $id => $bug) {
  try {
    act($id, $admin, array(array('type' => 'bugzilla.bug-id', 'value' => (string)$bug)));
  } catch (Exception $ex) {
    echo "bug id for D{$id}: ".$ex->getMessage()."\n";
  }
}

// Web sessions, to log in by setting the phsid cookie.
foreach (array($admin, $alice, $bob, $carol, $dave) as $user) {
  $token = id(new PhabricatorAuthSessionEngine())
    ->establishSession(PhabricatorAuthSession::TYPE_WEB, $user->getPHID(), false);
  echo $user->getUsername()."\tphsid=".$token."\n";
}
