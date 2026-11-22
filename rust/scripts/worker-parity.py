#!/usr/bin/env python3
"""Compares what PHP and Rust write when they run the same queued task, on
the development stack.

    rust/scripts/worker-parity.py [--sides php,rust] [--show] [--feed N]
        [--rust-worker CMD] [scenarios.json]...

A scenario queues one task: `{"name": ..., "class": "FeedPublisherWorker",
"data": {"key": 7301...}, "priority": 2000, "objectPHID": null}` (priority
and objectPHID optional). `--feed N` adds a FeedPublisherWorker scenario for
each of the N newest feed stories, one for a story whose key matches nothing,
and one whose data has no key.

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
  story into the markup cache while loading it; Rust doesn't).

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
# `phab-worker --once`: the handler left the task to PHP.
DECLINED = 3
# Columns of the task's own row which depend on how it was leased.
LEASE_COLUMNS = {"leaseOwner", "leaseExpires", "duration"}


def query(sql):
    """Rows of a query on the stack's MySQL, as lists of strings."""
    out = WP.sh(["docker", "exec", "-i", WP.DB, "sh", "-c",
                 f"{WP.MYSQL} -N 2>/dev/null"], stdin=sql.encode())
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


def enqueue(scenario):
    """`PhabricatorWorker::scheduleTask()`: the data row, an ID from the
    `worker_activetask` counter, the task row. Returns the task ID."""
    now = int(time.time())
    sql = (
        "USE phabricator_worker; "
        f"INSERT INTO worker_taskdata (data) VALUES "
        f"({quote(php_json(scenario['data']))}); "
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


def run_side(side, scenario, saved, before, known, names, rust_worker):
    started = int(time.time())
    task_id = enqueue(scenario)
    queued = WP.rows(WP.dump())
    initial = WP.counters()
    status = run(side, task_id, rust_worker)
    finished = int(time.time())
    moved = {row[0] for row in WP.counters() ^ initial}
    after = WP.rows(WP.dump())
    changed = WP.changed_databases(before, after) | WP.TOUCHED | moved
    if status is None:
        wrote = (after ^ queued)
        WP.restore(saved, changed)
        return None, wrote
    added = WP.normalize(mask_lease(after - queued, task_id, names), known,
                         started, finished)
    removed = WP.normalize(mask_lease(queued - after, task_id, names), known,
                           started, finished)
    WP.restore(saved, changed)
    return (status, added, removed), set()


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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--sides", default="php,rust")
    parser.add_argument("--show", action="store_true",
                        help="print every row change of the first side")
    parser.add_argument("--feed", type=int, default=0,
                        help="add scenarios for the N newest feed stories")
    parser.add_argument("--rust-worker", default=RUST_WORKER,
                        help="command running one task, {id} its ID")
    parser.add_argument("files", nargs="*")
    args = parser.parse_args()
    sides = args.sides.split(",")

    scenarios = []
    for path in args.files:
        with open(path) as handle:
            scenarios.extend(json.load(handle))
    if args.feed:
        scenarios.extend(feed_scenarios(args.feed))
    if not scenarios:
        parser.error("no scenarios: give files or --feed")

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
                                         names, args.rust_worker)
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
