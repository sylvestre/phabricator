#!/usr/bin/env python3
"""Compares what PHP and Rust write when they run the same queued task, on
the development stack.

    rust/scripts/worker-parity.py [--sides php,rust] [--show] [--feed N]
        [--metamta N] [--publish N] [--rust-worker CMD] [--repository ID
        [--repository-path PATH] [--repository-origin PATH]
        [--rust-repository CMD]] [scenarios.json]...

A scenario queues one task: `{"name": ..., "class": "FeedPublisherWorker",
"data": {"key": 7301...}, "priority": 2000, "objectPHID": null, "setup":
["UPDATE ..."]}` (priority, objectPHID and setup optional; the setup
statements run on both sides before the task is queued, and are undone with
the rest). `--feed N` adds a FeedPublisherWorker scenario for each of the N
newest feed stories, one for a story whose key matches nothing, and one whose
data has no key. `--metamta N` adds PhabricatorMetaMTAWorker scenarios for
the N newest mails, each put back in the queue: as configured (the stack has
no mailers: voided), with a `test` mailer in `cluster.mailers` (recipients
get Mozilla's mail by default: voided without recipients), and with a test
mailer and `email.default` 0 (sent), plus one for a mail which does not
exist. `--publish N` adds a PhabricatorApplicationTransactionPublishWorker
scenario for each of the N newest publish tasks of revisions in
`worker_archivetask`, requeued with their data as stored (`rawData`, which a
scenario may give instead of `data`), as configured and with
`email.default` 0 (Phabricator's own mail, so notifications of mailed users
are read).

`--repository ID` (3 for R3, rFAB, on the dev stack) adds the import
pipeline of that Git repository, whose working copy (`--repository-path`,
`/var/repo/3/`) has the history of `scripts/seed-dev-git-repo.sh`, as a bare
clone of R2's (`--repository-origin`, `/var/repo/2`, its remote). Each
scenario first deletes the repository's commits, refs and status messages
and sets its details: observed (`remote-uri` the origin) or hosted, both
importing. Then:

- `bin/repository update`, `discover` and `refs` (a scenario with a
  `"command"`, run by `bin/repository` in the PHP container and by
  `--rust-repository`, `phab-worker repository` in the Rust one; `"before"`
  commands run with PHP on both sides first): an update from no commit and
  one with nothing new, a discovery and a ref update of the hosted
  repository (whose update installs hooks, so it is not run);
- for each of the five commits, recorded as discovery leaves them (with
  fixed PHIDs), the message parser, the change parser, and the publish
  step, held (importing) and not (left to PHP); plus the message parser of
  an unreachable commit, of one already parsed, and of a commit which does
  not exist.

The commands change the working copy (`git fetch`, `git config`), which the
script doesn't restore: give it one which already has the origin's refs, as
the dev stack's R3 has, so both sides start from the same one.

For each scenario and side, from the same snapshot of every `phabricator_*`
database, the script queues the task with the SQL of
`PhabricatorWorker::scheduleTask()` (so both sides get the same task and
data IDs), runs it, records the rows added and removed, and restores the
databases. Then it compares the two sides once what each run makes up is
normalized, as `write-parity.py` does (PHIDs, the current time), plus:

- the lease columns of the task itself (`leaseOwner`, `leaseExpires`) and
  its `duration`: `bin/worker execute` clears the lease before running,
  while `phab-worker` leases the task as its daemon does;
- `cache_markupcache`, `user_cache` and the daemon logs, ignored as
  `write-parity.py` ignores them (PHP renders the summary of a comment
  story into the markup cache while loading it; Rust doesn't);
- the chronological key of a new feed story, whose lower 32 bits are
  random: `<chronological key N>`, by order of appearance.

A side is `php` (`bin/worker execute --id`, in the PHP container) or `rust`
(`--rust-worker`, `phab-worker --once --id` in the Rust container by
default; the image must ship `phab-worker`). When Rust leaves a task to PHP
(exit status 3) the scenario is reported `left to PHP`, and the script
checks that Rust wrote nothing. The daemons are stopped while it runs and
started again at the end.
"""

import argparse
import importlib.util
import json
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location(
    "write_parity", os.path.join(HERE, "write-parity.py"))
WP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(WP)

RUST = "phabricator-phab-rust-1"
RUST_WORKER = f"docker exec {RUST} phab-worker --once --id {{id}}"
RUST_REPOSITORY = f"docker exec {RUST} phab-worker repository {{args}}"
# `phab-worker --once`: the handler left the task to PHP.
DECLINED = 3
# Columns of the task's own row which depend on how it was leased.
LEASE_COLUMNS = {"leaseOwner", "leaseExpires", "duration"}
# A feed story's chronological key: the time, shifted, plus a random number.
CHRONOLOGICAL_KEY = re.compile(r"(?<![\w.-])\d{19}(?![\w.])")
PUBLISH_WORKER = "PhabricatorApplicationTransactionPublishWorker"


def query(sql):
    """Rows of a query on the stack's MySQL, as lists of strings. Names are
    qualified with their database; MySQL 5.7 still wants a default one to
    resolve the aliases of multi-table DELETEs."""
    out = WP.sh(["docker", "exec", "-i", WP.DB, "sh", "-c",
                 f"{WP.MYSQL} -N phabricator_repository 2>/dev/null"],
                stdin=sql.encode())
    return [line.split("\t") for line in out.decode().splitlines()]


def quote(value):
    if value is None:
        return "NULL"
    if isinstance(value, int):
        return str(value)
    escaped = str(value).replace("\\", "\\\\").replace("'", "\\'")
    return f"'{escaped}'"


def php_json(value):
    """PHP's `json_encode()`: escaped slashes, `\\uXXXX` for non-ASCII."""
    return json.dumps(value, separators=(",", ":")).replace("/", "\\/")


def task_data(scenario):
    """The `data` column: `rawData` as given, or `data` encoded like PHP."""
    if "rawData" in scenario:
        return scenario["rawData"]
    return php_json(scenario["data"])


def enqueue(scenario):
    """`PhabricatorWorker::scheduleTask()`: the data row, an ID from the
    `worker_activetask` counter, the task row. Returns the task ID."""
    now = int(time.time())
    sql = (
        "USE phabricator_worker; "
        f"INSERT INTO worker_taskdata (data) VALUES "
        f"({quote(task_data(scenario))}); "
        "SET @data = LAST_INSERT_ID(); "
        "INSERT INTO lisk_counter (counterName, counterValue) VALUES "
        "('worker_activetask', LAST_INSERT_ID(1)) ON DUPLICATE KEY UPDATE "
        "counterValue = LAST_INSERT_ID(counterValue + 1); "
        "SET @id = LAST_INSERT_ID(); "
        "INSERT INTO worker_activetask (id, taskClass, leaseOwner, leaseExpires, "
        "failureCount, dataID, failureTime, priority, objectPHID, dateCreated, "
        "dateModified, containerPHID) VALUES (@id, "
        f"{quote(scenario['class'])}, NULL, NULL, 0, @data, NULL, "
        f"{int(scenario.get('priority', 2000))}, "
        f"{quote(scenario.get('objectPHID'))}, {now}, {now}, NULL); "
        "SELECT @id"
    )
    return int(query(sql)[-1][0])


def run(side, task_id, rust_worker):
    """Runs the task; `None` if Rust left it to PHP."""
    if side == "php":
        done = subprocess.run(
            ["docker", "exec", WP.PHP, "/app/phabricator/bin/worker",
             "execute", "--id", str(task_id)], capture_output=True, text=True)
        # A failing task exits non-zero after writing its rows.
        return done.returncode
    if side == "rust":
        done = subprocess.run(rust_worker.format(id=task_id), shell=True,
                              capture_output=True, text=True)
        if done.returncode == DECLINED:
            return None
        if done.returncode not in (0,):
            sys.stderr.write(done.stdout + done.stderr)
        return done.returncode
    raise ValueError(side)


def split_values(values):
    """The fields of one `(...)` tuple of a mysqldump INSERT."""
    body = values.strip().rstrip(";")
    assert body.startswith("(") and body.endswith(")"), values[:80]
    body = body[1:-1]
    fields, current, quoted, escaped = [], [], False, False
    for char in body:
        if escaped:
            current.append(char)
            escaped = False
        elif char == "\\" and quoted:
            current.append(char)
            escaped = True
        elif char == "'":
            current.append(char)
            quoted = not quoted
        elif char == "," and not quoted:
            fields.append("".join(current))
            current = []
        else:
            current.append(char)
    fields.append("".join(current))
    return fields


def columns(table):
    database, name = table.split(".")
    return [row[0] for row in query(
        "SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE "
        f"TABLE_SCHEMA = '{database}' AND TABLE_NAME = '{name}' "
        "ORDER BY ORDINAL_POSITION")]


def mask_lease(changes, task_id, names):
    """Replaces the lease columns of the task's own rows."""
    masked = set()
    for table, values in changes:
        if table in names:
            fields = split_values(values)
            if fields[names[table].index("id")] == str(task_id):
                for column in LEASE_COLUMNS & set(names[table]):
                    fields[names[table].index(column)] = f"<{column}>"
                values = "(" + ",".join(fields) + ");"
        masked.add((table, values))
    return masked


def mask_keys(changes, started, finished):
    """Replaces the chronological keys of the stories made between `started`
    and `finished`."""
    keys = {}

    def key(match):
        value = int(match.group(0))
        if not started - 5 <= value >> 32 <= finished + 5:
            return match.group(0)
        return keys.setdefault(value, f"<chronological key {len(keys) + 1}>")

    return {(table, CHRONOLOGICAL_KEY.sub(key, values))
            for table, values in sorted(changes)}


def run_command(side, arguments, rust_repository):
    """Runs a `bin/repository` workflow; `None` if Rust left it to PHP."""
    if side == "php":
        done = subprocess.run(
            ["docker", "exec", WP.PHP, "/app/phabricator/bin/repository"]
            + arguments, capture_output=True, text=True)
        return done.returncode
    if side == "rust":
        done = subprocess.run(
            rust_repository.format(args=" ".join(arguments)), shell=True,
            capture_output=True, text=True)
        if done.returncode == DECLINED:
            return None
        if done.returncode != 0:
            sys.stderr.write(done.stdout + done.stderr)
        return done.returncode
    raise ValueError(side)


def run_side(side, scenario, saved, before, known, names, rust_worker,
             rust_repository):
    started = int(time.time())
    for statement in scenario.get("setup", []):
        query(statement)
    for arguments in scenario.get("before", []):
        # The same starting point on both sides: PHP runs these.
        run_command("php", arguments, rust_repository)
    command = scenario.get("command")
    task_id = None if command else enqueue(scenario)
    queued = WP.rows(WP.dump())
    initial = WP.counters()
    if command:
        status = run_command(side, command, rust_repository)
    else:
        status = run(side, task_id, rust_worker)
    finished = int(time.time())
    moved = {row[0] for row in WP.counters() ^ initial}
    after = WP.rows(WP.dump())
    changed = WP.changed_databases(before, after) | WP.TOUCHED | moved
    if status is None:
        wrote = (after ^ queued)
        WP.restore(saved, changed)
        return None, wrote
    added = mask_keys(WP.normalize(mask_lease(after - queued, task_id, names),
                                   known, started, finished),
                      started, finished)
    removed = mask_keys(WP.normalize(mask_lease(queued - after, task_id, names),
                                     known, started, finished),
                        started, finished)
    WP.restore(saved, changed)
    return (status, added, removed), set()


# The commits `seed-dev-git-repo.sh` makes, in the order discovery records
# them: what R3 (a bare clone of R2's history) has.
SEED_COMMITS = [
    ("d61f24fe3e14bb3307532a9bc02968d7462cb336", 1794600000),
    ("85948bd7ad4beca2df6fabad0d7f08b90365eff5", 1794600100),
    ("20fb56cd8ff000f2f791dc647906b4c5a9bef267", 1794600300),
    ("59965686ade70d003a571d2352862b01b4c3c3b8", 1794600200),
    ("81bcc2f65c2b5d5a68333fc3df1f7dfe77482aa4", 1794600400),
]
MESSAGE_PARSER = "PhabricatorRepositoryGitCommitMessageParserWorker"
CHANGE_PARSER = "PhabricatorRepositoryGitCommitChangeParserWorker"
PUBLISHER = "PhabricatorRepositoryCommitPublishWorker"


def repository_scenarios(repository, path, origin):
    """The import pipeline on `repository` (an ID): `bin/repository
    update`, `discover` and `refs` from no imported commit, then each
    parser step of each commit, and the publish step held and not."""
    repository = int(repository)
    phid = query("SELECT phid FROM phabricator_repository.repository "
                 f"WHERE id = {repository}")[0][0]
    callsign = query("SELECT IFNULL(callsign, '') FROM "
                     "phabricator_repository.repository "
                     f"WHERE id = {repository}")[0][0]
    name = callsign or f"R{repository}"
    db = "phabricator_repository"
    reset = [
        f"DELETE p FROM {db}.repository_refposition p JOIN "
        f"{db}.repository_refcursor c ON p.cursorID = c.id "
        f"WHERE c.repositoryPHID = {quote(phid)}",
        f"DELETE FROM {db}.repository_refcursor WHERE repositoryPHID = "
        f"{quote(phid)}",
        f"DELETE FROM {db}.repository_oldref WHERE repositoryPHID = "
        f"{quote(phid)}",
        f"DELETE d FROM {db}.repository_commitdata d JOIN "
        f"{db}.repository_commit c ON c.id = d.commitID "
        f"WHERE c.repositoryID = {repository}",
        f"DELETE p FROM {db}.repository_parents p JOIN "
        f"{db}.repository_commit c ON c.id = p.childCommitID "
        f"WHERE c.repositoryID = {repository}",
        f"DELETE pc FROM {db}.repository_pathchange pc "
        f"WHERE pc.repositoryID = {repository}",
        f"DELETE FROM {db}.repository_commit WHERE repositoryID = {repository}",
        f"DELETE FROM {db}.repository_summary WHERE repositoryID = {repository}",
        f"DELETE FROM {db}.repository_statusmessage "
        f"WHERE repositoryID = {repository}",
    ]

    def details(**values):
        return (f"UPDATE {db}.repository SET localPath = {quote(path)}, "
                f"details = {quote(php_json(values))} WHERE id = {repository}")

    observed = details(**{"remote-uri": origin, "importing": True})
    hosted = details(**{"hosting-enabled": True, "importing": True})
    scenarios = [
        {"name": f"update {name}, observed, from no commit",
         "command": ["update", name], "setup": reset + [observed]},
        {"name": f"update {name} twice (nothing new)",
         "command": ["update", name], "setup": reset + [observed],
         "before": [["update", name]]},
        {"name": f"discover {name}, hosted, from no commit",
         "command": ["discover", name], "setup": reset + [hosted]},
        {"name": f"refs {name}, hosted, after PHP's discovery",
         "command": ["refs", name], "setup": reset + [hosted],
         "before": [["discover", name]]},
    ]

    # The commits as discovery leaves them, with fixed PHIDs.
    def commits(status):
        values = ", ".join(
            f"({900 + index}, {repository}, 'PHID-CMIT-parityfab{index:05d}', "
            f"'{identifier}', {epoch}, NULL, 'none', '', {status})"
            for index, (identifier, epoch) in enumerate(SEED_COMMITS))
        data = ", ".join(f"({900 + index}, '', '', '[]')"
                         for index in range(len(SEED_COMMITS)))
        parents = ", ".join(
            f"({900 + child}, {900 + parent if parent is not None else 0})"
            for child, parent in [(0, None), (1, 0), (2, 1), (3, 1), (4, 2),
                                  (4, 3)])
        return [
            f"INSERT INTO {db}.repository_commit (id, repositoryID, phid, "
            "commitIdentifier, epoch, authorPHID, auditStatus, summary, "
            f"importStatus) VALUES {values}",
            f"INSERT INTO {db}.repository_commitdata (commitID, authorName, "
            f"commitMessage, commitDetails) VALUES {data}",
            f"INSERT INTO {db}.repository_parents (childCommitID, "
            f"parentCommitID) VALUES {parents}",
        ]

    for index, (identifier, _) in enumerate(SEED_COMMITS):
        commit = f"PHID-CMIT-parityfab{index:05d}"
        data = {"commitPHID": commit, "via": "discovery"}
        for label, class_name, status, setup in (
                ("message", MESSAGE_PARSER, 1024, observed),
                ("changes", CHANGE_PARSER, 1025, observed),
                ("publish, held (importing)", PUBLISHER, 1027, observed),
                ("publish, not held (left to PHP)", PUBLISHER, 1027,
                 details(**{"remote-uri": origin}))):
            scenarios.append({
                "name": f"{label} of {name} {identifier[:12]}",
                "class": class_name, "data": data, "priority": 2000,
                "objectPHID": commit,
                "setup": reset + [setup] + commits(status)})
    commit = "PHID-CMIT-parityfab00000"
    scenarios.append({
        "name": f"message of an unreachable {name} commit",
        "class": MESSAGE_PARSER,
        "data": {"commitPHID": commit, "via": "discovery"},
        "objectPHID": commit, "setup": reset + [observed] + commits(3072)})
    scenarios.append({
        "name": f"message of {name} commit already parsed",
        "class": MESSAGE_PARSER,
        "data": {"commitPHID": commit, "via": "discovery"},
        "objectPHID": commit, "setup": reset + [observed] + commits(1025)})
    scenarios.append({
        "name": "message of a commit which does not exist",
        "class": MESSAGE_PARSER, "data": {"commitPHID": "PHID-CMIT-nope"}})
    return scenarios


def feed_scenarios(count):
    keys = query(
        "SELECT story.chronologicalKey FROM phabricator_feed.feed_storydata story "
        "JOIN phabricator_feed.feed_storyreference ref "
        "ON ref.chronologicalKey = story.chronologicalKey "
        "GROUP BY story.chronologicalKey ORDER BY story.chronologicalKey DESC "
        f"LIMIT {int(count)}")
    scenarios = [{"name": f"feed story {key}", "class": "FeedPublisherWorker",
                  "data": {"key": int(key)}} for (key,) in keys]
    scenarios.append({"name": "feed story which does not exist",
                      "class": "FeedPublisherWorker", "data": {"key": 1}})
    scenarios.append({"name": "feed task without a key",
                      "class": "FeedPublisherWorker", "data": {}})
    return scenarios


def config_entry(key, value):
    """A `config_entry` row setting `key`, which both sides read after
    `local.json`."""
    phid = "PHID-CONF-parity" + key.replace(".", "").replace("-", "")[:14]
    return (
        "INSERT INTO phabricator_config.config_entry (namespace, configKey, "
        "value, isDeleted, phid, dateCreated, dateModified) VALUES "
        f"('default', {quote(key)}, {quote(php_json(value))}, 0, "
        f"{quote(phid)}, 1, 1)")


def metamta_scenarios(count):
    ids = [int(row[0]) for row in query(
        "SELECT id FROM phabricator_metamta.metamta_mail ORDER BY id DESC "
        f"LIMIT {int(count)}")]
    mailers = config_entry("cluster.mailers",
                           [{"key": "test-mailer", "type": "test"}])
    legacy = config_entry("email.default", "0")
    scenarios = []
    for mail_id in ids:
        requeue = ("UPDATE phabricator_metamta.metamta_mail SET status = "
                   f"'queued' WHERE id = {mail_id}")
        for label, setup in (("as configured", []),
                             ("with a test mailer", [mailers]),
                             ("with a test mailer and legacy mail",
                              [mailers, legacy])):
            scenarios.append({"name": f"mail {mail_id} {label}",
                              "class": "PhabricatorMetaMTAWorker",
                              "data": mail_id, "priority": 1000,
                              "setup": [requeue] + setup})
    scenarios.append({"name": "mail which does not exist",
                      "class": "PhabricatorMetaMTAWorker", "data": 999999})
    return scenarios


def publish_scenarios(count):
    """The newest publish tasks of revisions, requeued with their data."""
    rows = query(
        "SELECT task.id, task.priority, HEX(data.data) "
        "FROM phabricator_worker.worker_archivetask task "
        "JOIN phabricator_worker.worker_taskdata data ON data.id = task.dataID "
        f"WHERE task.taskClass = '{PUBLISH_WORKER}' "
        "AND task.objectPHID LIKE 'PHID-DREV-%' "
        f"ORDER BY task.id DESC LIMIT {int(count)}")
    legacy = config_entry("email.default", "0")
    scenarios = []
    for task_id, priority, data in rows:
        raw = bytes.fromhex(data).decode("utf-8")
        object_phid = json.loads(raw).get("objectPHID")
        for label, setup in (("as configured", []),
                             ("with legacy mail", [legacy])):
            scenarios.append({"name": f"publish task {task_id} {label}",
                              "class": PUBLISH_WORKER, "rawData": raw,
                              "priority": int(priority),
                              "objectPHID": object_phid, "setup": setup})
    return scenarios


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--sides", default="php,rust")
    parser.add_argument("--show", action="store_true",
                        help="print every row change of the first side")
    parser.add_argument("--feed", type=int, default=0,
                        help="add scenarios for the N newest feed stories")
    parser.add_argument("--metamta", type=int, default=0,
                        help="add scenarios for the N newest mails")
    parser.add_argument("--publish", type=int, default=0,
                        help="add scenarios for the N newest publish tasks "
                        "of revisions")
    parser.add_argument("--rust-worker", default=RUST_WORKER,
                        help="command running one task, {id} its ID")
    parser.add_argument("--repository", default=None,
                        help="add the import pipeline of this Git "
                        "repository ID (3 on the dev stack)")
    parser.add_argument("--repository-path", default="/var/repo/3/",
                        help="its working copy")
    parser.add_argument("--repository-origin", default="/var/repo/2",
                        help="the repository it observes")
    parser.add_argument("--rust-repository", default=RUST_REPOSITORY,
                        help="command running a bin/repository workflow, "
                        "{args} its arguments")
    parser.add_argument("files", nargs="*")
    args = parser.parse_args()
    sides = args.sides.split(",")

    scenarios = []
    for path in args.files:
        with open(path) as handle:
            scenarios.extend(json.load(handle))
    if args.feed:
        scenarios.extend(feed_scenarios(args.feed))
    if args.metamta:
        scenarios.extend(metamta_scenarios(args.metamta))
    if args.publish:
        scenarios.extend(publish_scenarios(args.publish))
    if args.repository:
        scenarios.extend(repository_scenarios(
            args.repository, args.repository_path, args.repository_origin))
    if not scenarios:
        parser.error("no scenarios: give files, --feed, --metamta, --publish "
                     "or --repository")

    names = {table: columns(table) for table in (
        "phabricator_worker.worker_activetask",
        "phabricator_worker.worker_archivetask")}
    WP.sh(["docker", "exec", WP.PHP, "/app/phabricator/bin/phd", "stop"])
    saved = WP.snapshots()
    snapshot = WP.dump()
    before = WP.rows(snapshot)
    known = set(WP.PHID.findall(snapshot.decode("utf-8", "replace")))
    failures = 0
    try:
        for scenario in scenarios:
            results = []
            for side in sides:
                result, wrote = run_side(side, scenario, saved, before, known,
                                         names, args.rust_worker,
                                         args.rust_repository)
                if wrote:
                    failures += 1
                    print(f"DIFFERENT: {scenario['name']}: {side} left the "
                          "task to PHP but wrote rows")
                    WP.show(f"{side} changed", wrote)
                results.append(result)
            if None in results:
                print(f"left to PHP: {scenario['name']}")
                continue
            (status_a, added_a, removed_a) = results[0]
            (status_b, added_b, removed_b) = results[1]
            same = added_a == added_b and removed_a == removed_b
            print(f"{'same' if same else 'DIFFERENT'}: {scenario['name']} "
                  f"({len(added_a)} rows added, {len(removed_a)} removed, "
                  f"exit {status_a}/{status_b})")
            if args.show:
                WP.show("added", added_a)
                WP.show("removed", removed_a)
            if not same:
                failures += 1
                WP.show(f"only {sides[0]} added", added_a - added_b)
                WP.show(f"only {sides[1]} added", added_b - added_a)
                WP.show(f"only {sides[0]} removed", removed_a - removed_b)
                WP.show(f"only {sides[1]} removed", removed_b - removed_a)
    finally:
        WP.sh(["docker", "exec", WP.PHP, "/app/phabricator/bin/phd", "start"])
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
