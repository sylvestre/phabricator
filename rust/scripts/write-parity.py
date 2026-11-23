#!/usr/bin/env python3
"""Compares what two runs of the same Conduit write do to the database of
the development stack.

    rust/scripts/write-parity.py [--sides php,rust] [--show] <api token> <scenarios.json>...

For each scenario, the script dumps every `phabricator_*` database, runs the
call on the first side, records the rows it added and removed, restores the
databases it changed, then does the same on the second side. Then
it compares the two sets of row changes, once the values each run makes up
(new PHIDs, the current time, random keys) are normalized.

A side is `php` (through nginx, with X-Phab-Skip-Rust) or `rust` (the Rust
service itself). `--sides php,php` checks the harness: PHP against itself
must not differ.

The daemons are stopped while it runs, so queued tasks stay in the queue
and are compared too; they are started again at the end.
"""

import argparse
import hashlib
import json
import re
import subprocess
import sys
import time

PROXY = "http://localhost:1090"
STACK = "http://phabricator.test"
DB = "phabricator-phabdb-1"
PHP = "phabricator-phabricator-1"
WEB = "phabricator-phabricator.test-1"
MYSQL = "mysql -uroot -ppassword"
# How much of a differing row to print.
WIDTH = int(__import__("os").environ.get("WRITE_PARITY_WIDTH", "300"))

# Databases restored after every run, whose ignored tables change.
TOUCHED = {"phabricator_conduit", "phabricator_user", "phabricator_cache"}

# Tables whose rows are bookkeeping, not the edit.
IGNORED = {
    "phabricator_conduit.conduit_methodcalllog",
    "phabricator_conduit.conduit_token",
    "phabricator_daemon.daemon_log",
    "phabricator_daemon.daemon_logevent",
    "phabricator_cache.cache_general",
    "phabricator_cache.cache_markupcache",
    "phabricator_user.user_cache",
    "phabricator_system.system_actionlog",
}

# Transaction PHIDs have shorter random parts than the others.
PHID = re.compile(r"PHID-[A-Z]{4}(?:-[A-Z]{4})?-[a-z0-9]{10,24}")
INTEGER = re.compile(r"(?<![\w.-])\d{9,10}(?![\w.])")
# Timings inside serialized Herald transcripts.
ELAPSED = re.compile(r'(elapsed\\?";d:)[0-9.Ee+-]+')
# The host column of Herald transcripts, after the time.
HOST = re.compile(r"^(\(\d+,_binary '[^']*',<now>,)'[^']*'")
# Random keys: transaction groups.
GROUP_ID = re.compile(r'(core\.groupID\\?":\\?")[a-z0-9]{32}')
# A user's availability cache and its TTL: PHP rebuilds a stale one
# whenever it loads the user (`rebuildAvailabilityCache()`), whatever the
# edit; Calendar is not ported, so it holds no availability.
AVAILABILITY = re.compile(r"'\{\\\"until\\\":[^']*\}',(?:\d+|NULL)")
# A new project's random mail key.
MAIL_KEY = re.compile(r"'[a-z0-9]{20}'")
# A new revision's random mail key (`Filesystem::readRandomCharacters(40)`).
REVISION_MAIL_KEY = re.compile(r"(?:_binary )?'[a-z2-7]{40}'")
# A new file's random secret and mail keys, the handles of chunked data,
# and the content hash of a chunked file allocated without a hash.
FILE_KEY = re.compile(r"'[a-z2-7]{20}'")
CHUNK_HANDLE = re.compile(r"'[0-9a-zA-Z._]{12}'")
CHUNK_HASH = re.compile(r"_binary '[0-9a-f]{38}-C(?:\\0)*'")
# The alphabet of `PhabricatorHash::digestForIndex()`.
INDEX_DIGEST = (
    "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ._"
)


def digest_for_index(value):
    digest = hashlib.sha1(value.encode()).digest()
    return "".join(INDEX_DIGEST[byte & 0x3F] for byte in digest[:12])


# How long something took, like a Herald transcript's duration.
DURATION = re.compile(r"(?<![\w.])\d+\.\d{5,}(?![\w.])")


def sh(command, stdin=None):
    return subprocess.run(
        command, input=stdin, capture_output=True, check=True,
        shell=isinstance(command, str)).stdout


def databases():
    return sh(["docker", "exec", DB, "sh", "-c",
               f"{MYSQL} -N -e 'SHOW DATABASES LIKE \"phabricator%\"' "
               "2>/dev/null"]).decode().split()


def dump(names=None, rows_only=True):
    """A dump of these databases: one INSERT per row to compare rows, or
    extended inserts to restore quickly."""
    names = names or databases()
    style = "--skip-extended-insert" if rows_only else "--extended-insert"
    return sh(["docker", "exec", DB, "sh", "-c",
               f"mysqldump -uroot -ppassword {style} --skip-dump-date "
               f"--order-by-primary --databases {' '.join(names)} "
               "2>/dev/null"])


def snapshots():
    """A quick-to-restore dump of each database."""
    return {name: dump([name], rows_only=False) for name in databases()}


def restore(snapshots, changed):
    """Restores the databases a run changed."""
    for name in sorted(changed):
        sh(["docker", "exec", "-i", DB, "sh", "-c", f"{MYSQL} 2>/dev/null"],
           stdin=snapshots[name])


def rows(snapshot):
    """The rows of a dump, as `db.table` and the INSERT line."""
    found = set()
    database = None
    for line in snapshot.decode("utf-8", "replace").splitlines():
        if line.startswith("USE `"):
            database = line[5:line.index("`", 5)]
        elif line.startswith("INSERT INTO `"):
            table = line[13:line.index("`", 13)]
            name = f"{database}.{table}"
            if name not in IGNORED:
                values = line[line.index(" VALUES ") + 8:]
                if name == "phabricator_user.user":
                    values = AVAILABILITY.sub("<availability>", values)
                found.add((name, values))
    return found


def changed_databases(before, after):
    return {table.split(".")[0] for table, _ in before ^ after}


def counters():
    """The AUTO_INCREMENT of every table, by database: a call which fails
    and rolls back still moves them."""
    out = sh(["docker", "exec", DB, "sh", "-c",
              f"{MYSQL} -N -e \"SELECT TABLE_SCHEMA, TABLE_NAME, AUTO_INCREMENT "
              "FROM information_schema.tables WHERE TABLE_SCHEMA LIKE "
              "'phabricator%'\" 2>/dev/null"]).decode()
    return {tuple(line.split("\t")) for line in out.splitlines()}


def call(side, scenario, token):
    params = dict(scenario.get("params", {}))
    params["__conduit__"] = {"token": token}
    data = ["--data-urlencode", "params=" + json.dumps(params),
            "-d", "output=json", "-d", "__conduit__=1"]
    if side == "php":
        out = sh(["curl", "-s", "-H", "X-Phab-Skip-Rust: 1", "-x", PROXY,
                  f"{STACK}/api/{scenario['method']}"] + data)
    elif side == "rust":
        body = subprocess.run(
            ["curl", "-s", "-o", "/dev/null", "-w", "%{url_effective}",
             "--get", "http://x/"] + data, capture_output=True,
            text=True).stdout.split("?", 1)[1]
        done = subprocess.run(
            ["docker", "exec", WEB, "wget", "-S", "-qO-", "--post-data", body,
             f"http://phab-rust:8080/api/{scenario['method']}"],
            capture_output=True)
        if b" 421 " in done.stderr:
            # Rust leaves the call to PHP, and wrote nothing.
            return None
        done.check_returncode()
        out = done.stdout
    else:
        raise ValueError(side)
    return json.loads(out)


# A row whose first columns are its ID and its PHID.
OWN_PHID = re.compile(r"^\(\d+,(?:_binary )?'(?=PHID-)")


def row_order(change):
    """Sorts rows by table, then by their numeric ID when they start with
    one."""
    table, values = change
    match = re.match(r"\((\d+),", values)
    return (table, int(match.group(1)) if match else -1, values)


def normalize(changes, known, started, finished):
    """Replaces what a run makes up, in the order it appears."""
    names = {}

    def phid(match):
        value = match.group(0)
        if value in known:
            return value
        prefix = value[:value.rindex("-")]
        return names.setdefault(value, f"{prefix}-<{len(names) + 1}>")

    def now(match):
        value = int(match.group(0))
        if started - 5 <= value <= finished + 5:
            return "<now>"
        # Temporary files expire a day or two after they're made.
        for days in (1, 2):
            if started - 5 <= value - days * 86400 <= finished + 5:
                return f"<now+{days}d>"
        return match.group(0)

    # Name the PHIDs of new rows first, by table and row ID, so a row's name
    # doesn't depend on how its random PHID sorts among the rows that point
    # at it (like a mail's recipient edges).
    for table, values in sorted(changes, key=row_order):
        own = OWN_PHID.match(values)
        if own:
            phid(PHID.match(values, own.end()))
    normalized = set()
    for table, values in sorted(changes):
        if table == "phabricator_project.project":
            # The path key comes from the new project's random PHID.
            for value in set(PHID.findall(values)) - set(known):
                key = digest_for_index(value)[:4]
                values = values.replace(f"'{key}'", "'<path key>'")
            values = MAIL_KEY.sub("'<mail key>'", values)
        if table == "phabricator_differential.differential_revision":
            values = REVISION_MAIL_KEY.sub("'<mail key>'", values)
        if table == "phabricator_file.file":
            values = FILE_KEY.sub("'<key>'", values)
            values = CHUNK_HASH.sub("_binary '<chunk hash>'", values)
        if table in ("phabricator_file.file", "phabricator_file.file_chunk"):
            values = CHUNK_HANDLE.sub("'<handle>'", values)
        values = PHID.sub(phid, values)
        values = INTEGER.sub(now, values)
        values = DURATION.sub("<duration>", values)
        values = GROUP_ID.sub(r"\1<group>", values)
        values = ELAPSED.sub(r"\1<duration>", values)
        if table == "phabricator_herald.herald_transcript":
            # The machine which evaluated the rules.
            values = HOST.sub(r"\1'<host>'", values)
        normalized.add((table, values))
    return normalized


def run_side(side, scenario, token, saved, before, known):
    initial = counters()
    started = int(time.time())
    answer = call(side, scenario, token)
    finished = int(time.time())
    moved = {row[0] for row in counters() ^ initial}
    if answer is None:
        restore(saved, moved)
        return None
    after = rows(dump())
    added = normalize(after - before, known, started, finished)
    removed = normalize(before - after, known, started, finished)
    restore(saved, changed_databases(before, after) | TOUCHED | moved)
    return answer, added, removed


def show(label, changes):
    for table, values in sorted(changes):
        print(f"    {label} {table}: {values[:WIDTH]}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--sides", default="php,rust")
    parser.add_argument("--show", action="store_true",
                        help="print every row change of the first side")
    parser.add_argument("token")
    parser.add_argument("files", nargs="+")
    args = parser.parse_args()
    sides = args.sides.split(",")

    sh(["docker", "exec", PHP, "/app/phabricator/bin/phd", "stop"])
    saved = snapshots()
    snapshot = dump()
    before = rows(snapshot)
    known = set(PHID.findall(snapshot.decode("utf-8", "replace")))
    failures = 0
    try:
        for path in args.files:
            with open(path) as scenarios:
                for scenario in json.load(scenarios):
                    results = [run_side(side, scenario, args.token, saved,
                                        before, known) for side in sides]
                    if None in results:
                        print(f"left to PHP: {scenario['name']}")
                        continue
                    (answer_a, added_a, removed_a) = results[0]
                    (answer_b, added_b, removed_b) = results[1]
                    errors = (answer_a.get("error_code"),
                              answer_b.get("error_code"))
                    same = (added_a == added_b and removed_a == removed_b
                            and errors[0] == errors[1])
                    status = "same" if same else "DIFFERENT"
                    print(f"{status}: {scenario['name']} "
                          f"({len(added_a)} rows added, "
                          f"{len(removed_a)} removed, errors {errors})")
                    if args.show:
                        show("added", added_a)
                        show("removed", removed_a)
                    if not same:
                        failures += 1
                        show(f"only {sides[0]} added", added_a - added_b)
                        show(f"only {sides[1]} added", added_b - added_a)
                        show(f"only {sides[0]} removed", removed_a - removed_b)
                        show(f"only {sides[1]} removed", removed_b - removed_a)
    finally:
        sh(["docker", "exec", PHP, "/app/phabricator/bin/phd", "start"])
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
