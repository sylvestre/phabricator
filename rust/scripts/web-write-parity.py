#!/usr/bin/env python3
"""Compares what PHP and Rust write when they get the same post of a web
form, on the development stack.

    rust/scripts/web-write-parity.py [--sides php,rust] [--show]
        [--csrf-page /D1] <phsid> <scenarios.json>...

`phsid` is the session cookie of the user who posts the forms (log in to
the stack and copy it from the browser). A scenario posts one form:

    {"name": "accept D3", "path": "/differential/revision/edit/3/comment/",
     "fields": {"editengine.actions": [{"type": "accept", "value": true,
                "initialValue": null}], "comment": "", "comment_metadata": "{}"},
     "sql": ["INSERT INTO phabricator_draft.draft_versioneddraft ..."]}

`fields` are the form's fields; a list or an object is sent JSON-encoded
(as `editengine.actions` is). `__form__` and a `__csrf__` token are added
unless the scenario gives its own. The token comes from the `__csrf__`
input of `--csrf-page`, fetched from PHP before the first snapshot. `{D3}`
in a path or a field is the PHID of revision D3, `{@alice}` the PHID of
user alice. The optional `sql`
statements run on both sides before the post, and are undone with the
rest.

For each scenario, the script dumps every `phabricator_*` database, posts
the form to the first side, records the rows added and removed and the
answer (its status and `Location`), restores the databases it changed,
then does the same on the second side, and compares, once the values
each run makes up are normalized, as `write-parity.py` does (it reuses
its code). The session's own row is ignored: both sides extend it when
they load the user.

A side is `php` (through nginx, with X-Phab-Skip-Rust) or `rust` (the
Rust service itself, from inside the PHP container, without following
redirects). When Rust answers 421, it left the form to PHP: the scenario
is reported `left to PHP`. `--sides php,php` checks the harness.

The daemons are stopped while it runs, so queued tasks stay in the queue
and are compared too; they are started again at the end.
"""

import argparse
import importlib.util
import json
import os
import re
import subprocess
import sys
import time
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location(
    "write_parity", os.path.join(HERE, "write-parity.py"))
WP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(WP)

RUST_URL = "http://phab-rust:8080"
# The container which posts to the Rust service: it must be on the stack's
# network and have PHP.
CLIENT = os.environ.get("WEB_PARITY_CLIENT", WP.PHP)

# The sessions are compared: the settings actions delete them and take them
# out of high security. Both sides extend the posting session when they
# load the user, so its expiry is left out (`session_rows()`). Conduit
# tokens are compared too: the posts make none of their own.
WP.IGNORED.discard("phabricator_user.phabricator_session")
WP.IGNORED.discard("phabricator_conduit.conduit_token")

# `phabricator_session` rows: (id, userPHID, type, sessionKey,
# sessionStart, sessionExpires, ...).
SESSION_EXPIRES = re.compile(r"^(\(\d+,'[^']*','[^']*','[^']*',\d+,)\d+")
# A new token's secret (`api-` and 28 random characters).
TOKEN_SECRET = re.compile(r"'(api|cli|clr)-[a-z2-7]{28}'")
# A new address's verification code, and the link mail carries it in.
VERIFICATION_CODE = re.compile(r"'[a-z2-7]{24}'")
VERIFICATION_LINK = re.compile(r"(emailverify\\*/)[a-z2-7]{24}")
# What the activity log records about the request, which differs between
# PHP behind nginx and the Rust service called directly: the host, the
# user agent and the remote address (the column after the dates).
LOG_HOST = re.compile(r'(\\"host\\":\\")[^\\"]*')
LOG_AGENT = re.compile(r'(\\"user_agent\\":)(?:null|\\"[^\\"]*\\")')
LOG_ADDRESS = re.compile(r"(,\d+,\d+,)'[^']*'(,(?:'[^']*'|NULL)\);?$)")


def settings_values(table, values):
    """Normalizes what a run of the settings actions makes up."""
    if table == "phabricator_user.phabricator_session":
        return SESSION_EXPIRES.sub(r"\1<expires>", values)
    if table == "phabricator_conduit.conduit_token":
        return TOKEN_SECRET.sub(r"'\1-<secret>'", values)
    if table == "phabricator_user.user_email":
        return VERIFICATION_CODE.sub("'<code>'", values)
    if table == "phabricator_metamta.metamta_mail":
        return VERIFICATION_LINK.sub(r"\1<code>", values)
    if table == "phabricator_user.user_log":
        values = LOG_HOST.sub(r"\1<host>", values)
        values = LOG_AGENT.sub(r"\1<agent>", values)
        return LOG_ADDRESS.sub(r"\1'<address>'\2", values)
    return values


_ROWS = WP.rows
_NORMALIZE = WP.normalize


def session_rows(snapshot):
    return {(table, SESSION_EXPIRES.sub(r"\1<expires>", values))
            if table == "phabricator_user.phabricator_session"
            else (table, values)
            for table, values in _ROWS(snapshot)}


def normalize_settings(changes, known, started, finished):
    changes = {(table, settings_values(table, values))
               for table, values in changes}
    return _NORMALIZE(changes, known, started, finished)


WP.rows = session_rows
WP.normalize = normalize_settings

CSRF = re.compile(r'name="__csrf__" value="([^"]+)"')
REVISION = re.compile(r"\{D([1-9][0-9]*)\}")
USER = re.compile(r"\{@([A-Za-z0-9_.-]+)\}")

# Posts a form without following redirects; prints the status line, the
# Location and the body.
POST = r"""
$in = json_decode(stream_get_contents(STDIN), true);
$context = stream_context_create(array('http' => array(
  'method' => 'POST',
  'header' => "Content-Type: application/x-www-form-urlencoded\r\n".
    'Cookie: phsid='.$in['phsid']."\r\n",
  'content' => $in['body'],
  'follow_location' => 0,
  'ignore_errors' => true,
  'timeout' => 60,
)));
$body = @file_get_contents($in['url'], false, $context);
echo json_encode(array(
  'headers' => isset($http_response_header) ? $http_response_header : array(),
  'body' => (string)$body,
));
"""


def revision_phid(match):
    out = WP.sh(["docker", "exec", WP.DB, "sh", "-c",
                 f"{WP.MYSQL} -N -e \"SELECT phid FROM "
                 "phabricator_differential.differential_revision WHERE id = "
                 f"{int(match.group(1))}\" 2>/dev/null"]).decode().strip()
    if not out:
        raise SystemExit(f"no revision D{match.group(1)}")
    return out


def user_phid(match):
    out = WP.sh(["docker", "exec", WP.DB, "sh", "-c",
                 f"{WP.MYSQL} -N -e \"SELECT phid FROM "
                 "phabricator_user.user WHERE userName = "
                 f"'{match.group(1)}'\" 2>/dev/null"]).decode().strip()
    if not out:
        raise SystemExit(f"no user {match.group(1)}")
    return out


def expand(text):
    return USER.sub(user_phid, REVISION.sub(revision_phid, text))


def csrf_token(phsid, page):
    out = WP.sh(["curl", "-s", "-H", "X-Phab-Skip-Rust: 1", "-x", WP.PROXY,
                 "-b", f"phsid={phsid}", f"{WP.STACK}{page}"]).decode(
                     "utf-8", "replace")
    found = CSRF.search(out)
    if not found:
        raise SystemExit(f"no CSRF token on {page}: is the session valid?")
    return found.group(1)


def form_body(scenario, token):
    fields = {"__form__": "1", "__csrf__": token}
    for key, value in scenario.get("fields", {}).items():
        if not isinstance(value, str):
            value = json.dumps(value)
        fields[key] = expand(value)
    return urllib.parse.urlencode(fields)


def answer(headers):
    """The status and the Location of an answer's header lines."""
    status = None
    location = None
    for line in headers:
        if line.upper().startswith("HTTP/"):
            status = int(line.split()[1])
        elif line.lower().startswith("location:"):
            location = line.split(":", 1)[1].strip()
    return status, location


def post(side, scenario, phsid, token):
    path = expand(scenario["path"])
    body = form_body(scenario, token)
    if side == "php":
        out = WP.sh(["curl", "-s", "-o", "/dev/null", "-D", "-",
                     "-H", "X-Phab-Skip-Rust: 1", "-x", WP.PROXY,
                     "-b", f"phsid={phsid}",
                     "-H", "Content-Type: application/x-www-form-urlencoded",
                     "--data-binary", body, f"{WP.STACK}{path}"])
        return answer(out.decode("utf-8", "replace").splitlines())
    if side == "rust":
        request = json.dumps({"url": f"{RUST_URL}{path}", "phsid": phsid,
                              "body": body})
        out = WP.sh(["docker", "exec", "-i", CLIENT, "php", "-r", POST],
                    stdin=request.encode())
        status, location = answer(json.loads(out)["headers"])
        if status == 421:
            # Rust leaves the form to PHP, and wrote nothing.
            return None
        return status, location
    raise ValueError(side)


def run_side(side, scenario, phsid, token, saved, before, known):
    initial = WP.counters()
    started = int(time.time())
    for statement in scenario.get("sql", []):
        WP.sh(["docker", "exec", "-i", WP.DB, "sh", "-c",
               f"{WP.MYSQL} 2>/dev/null"], stdin=expand(statement).encode())
    result = post(side, scenario, phsid, token)
    finished = int(time.time())
    moved = {row[0] for row in WP.counters() ^ initial}
    after = WP.rows(WP.dump())
    changed = WP.changed_databases(before, after) | WP.TOUCHED | moved
    if result is None:
        WP.restore(saved, changed)
        return None
    added = WP.normalize(after - before, known, started, finished)
    removed = WP.normalize(before - after, known, started, finished)
    WP.restore(saved, changed)
    return result, added, removed


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--sides", default="php,rust")
    parser.add_argument("--show", action="store_true",
                        help="print every row change of the first side")
    parser.add_argument("--csrf-page", default="/D1",
                        help="the PHP page whose form gives the CSRF token")
    parser.add_argument("phsid")
    parser.add_argument("files", nargs="+")
    args = parser.parse_args()
    sides = args.sides.split(",")

    token = csrf_token(args.phsid, args.csrf_page)
    WP.sh(["docker", "exec", WP.PHP, "/app/phabricator/bin/phd", "stop"])
    saved = WP.snapshots()
    snapshot = WP.dump()
    before = WP.rows(snapshot)
    known = set(WP.PHID.findall(snapshot.decode("utf-8", "replace")))
    failures = 0
    try:
        for path in args.files:
            with open(path) as scenarios:
                for scenario in json.load(scenarios):
                    results = [run_side(side, scenario, args.phsid, token,
                                        saved, before, known)
                               for side in sides]
                    if None in results:
                        print(f"left to PHP: {scenario['name']}")
                        continue
                    (answer_a, added_a, removed_a) = results[0]
                    (answer_b, added_b, removed_b) = results[1]
                    same = (added_a == added_b and removed_a == removed_b
                            and answer_a == answer_b)
                    status = "same" if same else "DIFFERENT"
                    print(f"{status}: {scenario['name']} "
                          f"({len(added_a)} rows added, "
                          f"{len(removed_a)} removed, answers {answer_a} "
                          f"{answer_b})")
                    if args.show:
                        WP.show("added", added_a)
                        WP.show("removed", removed_a)
                    if not same:
                        failures += 1
                        WP.show(f"only {sides[0]} added", added_a - added_b)
                        WP.show(f"only {sides[1]} added", added_b - added_a)
                        WP.show(f"only {sides[0]} removed",
                                removed_a - removed_b)
                        WP.show(f"only {sides[1]} removed",
                                removed_b - removed_a)
    finally:
        WP.sh(["docker", "exec", WP.PHP, "/app/phabricator/bin/phd", "start"])
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
