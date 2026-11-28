#!/usr/bin/env python3
"""Compares what a real Conduit client writes through PHP and through Rust,
on the development stack.

    rust/scripts/client-parity.py [--show] --methods m1,m2 <token> <scenarios.json>...

A scenario runs a client (moz-phab, lrv, ...) in a Git working copy:

    {"name": "submit a new revision",
     "setup": [["git", "commit", "--allow-empty", "-m", "Bug 1 - x r?bob"]],
     "command": ["moz-phab", "submit", "--yes", "--no-wip-prompt", "HEAD~1"]}

For each side, the script resets the working copy to its first commit,
runs `setup` and `command` there with fixed Git dates (so both sides make
the same commits), records the rows added and removed, as
`write-parity.py` does, and restores the databases. The PHP side runs with
nginx as it is; the Rust side runs with `--methods` added to the write
location of `rust/nginx/rust-routes.conf`, which is put back afterwards.

The client's home holds an `.arcrc` with `token` for the stack, a Git
identity, and moz-phab's configuration without updates nor telemetry. Its output is
compared too (lines naming revisions and diffs are normalized).

The daemons are stopped while it runs, and started again at the end.
"""

import argparse
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location(
    "write_parity", os.path.join(HERE, "write-parity.py"))
WP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(WP)

ROUTES = os.path.join(HERE, "..", "nginx", "rust-routes.conf")
# The write location: the `location` line after the comment block about
# write methods.
WRITE_LOCATION = re.compile(r"(location ~ \^/api/\((?:differential\\\.creatediff)[^)]*)\)")
# Commits made on both sides must be identical.
GIT_DATES = {"GIT_AUTHOR_DATE": "2026-11-20T12:00:00+00:00",
             "GIT_COMMITTER_DATE": "2026-11-20T12:00:00+00:00"}
MOZ_PHAB_CONFIG = """[updater]
self_last_check = 9999999999
self_auto_update = False
get_pre_releases = False

[submit]
auto_submit = True

[telemetry]
enabled = False

[error_reporting]
report_to_sentry = False
"""
# moz-phab asks about telemetry until it knows the user.
MOZ_PHAB_USER_INFO = {"is_employee": False, "last_check": 9999999999,
                      "user_code": "00000000-0000-0000-0000-000000000000",
                      "installation_id": "00000000-0000-0000-0000-000000000001"}
# Clients which ignore `http_proxy` (moz-phab 2.26 calls urllib3 itself)
# reach the stack through this: Python's resolver answers the stack's name
# with the web container's address, and nothing else, so crash reports
# (moz-phab's Sentry) and other requests leaving the machine fail.
SITECUSTOMIZE = """import socket

_getaddrinfo = socket.getaddrinfo


def getaddrinfo(host, *args, **kwargs):
    if host == "phabricator.test":
        return _getaddrinfo({address!r}, *args, **kwargs)
    if host in ("localhost", "127.0.0.1", "::1"):
        return _getaddrinfo(host, *args, **kwargs)
    raise socket.gaierror(socket.EAI_NONAME, "client-parity: " + str(host))


socket.getaddrinfo = getaddrinfo
"""
# What a client prints that each run makes up.
OUTPUT_NOISE = [
    (re.compile(r"D\d+"), "D<n>"),
    (re.compile(r"\b[0-9a-f]{7,40}\b"), "<hash>"),
    (re.compile(r"\d+(?:\.\d+)? ?s(?:ec)?\b"), "<time>"),
    # Spinner frames: how many depends on how long each call takes.
    (re.compile(r"(?<=\.\.)[-\\|/ \x08]+$", re.M), ""),
]


def reload_nginx():
    WP.sh(["docker", "exec", WP.WEB, "nginx", "-t"])
    WP.sh(["docker", "exec", WP.WEB, "nginx", "-s", "reload"])
    time.sleep(1)


def route_to_rust(original, methods):
    pattern = "|".join(re.escape(m) for m in methods)
    routed, count = WRITE_LOCATION.subn(
        lambda match: f"{match.group(1)}|{pattern})", original, count=1)
    if count != 1:
        raise SystemExit("no write location in rust-routes.conf")
    return routed


def client_home(token):
    home = tempfile.mkdtemp(prefix="client-parity-")
    with open(os.path.join(home, ".arcrc"), "w") as arcrc:
        json.dump({"hosts": {f"{WP.STACK}/api/": {"token": token}}}, arcrc)
    os.chmod(os.path.join(home, ".arcrc"), 0o600)
    with open(os.path.join(home, ".moz-phab-config"), "w") as config:
        config.write(MOZ_PHAB_CONFIG)
    os.makedirs(os.path.join(home, ".mozbuild", "moz-phab"))
    with open(os.path.join(home, ".mozbuild", "moz-phab", "user_info.json"),
              "w") as user_info:
        json.dump(MOZ_PHAB_USER_INFO, user_info)
    with open(os.path.join(home, ".gitconfig"), "w") as gitconfig:
        gitconfig.write("[user]\n\tname = Alice\n\temail = alice@example.com\n")
    address = WP.sh(["docker", "inspect", "-f",
                     "{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}",
                     WP.WEB]).decode().split()[0]
    os.mkdir(os.path.join(home, "python"))
    with open(os.path.join(home, "python", "sitecustomize.py"), "w") as site:
        site.write(SITECUSTOMIZE.format(address=address))
    return home


def client_env(home):
    env = dict(os.environ, HOME=home, HTTP_ALLOWED="1",
               http_proxy=WP.PROXY, HTTP_PROXY=WP.PROXY,
               PYTHONPATH=os.path.join(home, "python"), **GIT_DATES)
    env.pop("https_proxy", None)
    env.pop("HTTPS_PROXY", None)
    return env


def run(command, cwd, env):
    done = subprocess.run(command, cwd=cwd, env=env, capture_output=True,
                          text=True, timeout=600)
    return done.returncode, done.stdout + done.stderr


def normalized_output(output):
    for pattern, replacement in OUTPUT_NOISE:
        output = pattern.sub(replacement, output)
    return output


def run_side(scenario, workdir, env, saved, before, known):
    base = run(["git", "rev-list", "--max-parents=0", "HEAD"], workdir, env)[1]
    run(["git", "reset", "-q", "--hard", base.split()[0]], workdir, env)
    run(["git", "clean", "-qfdx"], workdir, env)
    initial = WP.counters()
    started = int(time.time())
    for command in scenario.get("setup", []):
        code, output = run(command, workdir, env)
        if code:
            WP.restore(saved, WP.changed_databases(before, WP.rows(WP.dump()))
                       | WP.TOUCHED | {row[0] for row in WP.counters() ^ initial})
            raise SystemExit(f"setup failed: {command}: {output}")
    code, output = run(scenario["command"], workdir, env)
    finished = int(time.time())
    moved = {row[0] for row in WP.counters() ^ initial}
    after = WP.rows(WP.dump())
    added = WP.normalize(after - before, known, started, finished)
    removed = WP.normalize(before - after, known, started, finished)
    WP.restore(saved, WP.changed_databases(before, after) | WP.TOUCHED | moved)
    return code, normalized_output(output), added, removed, output


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--show", action="store_true")
    parser.add_argument("--methods", required=True,
                        help="the methods the Rust side routes to Rust")
    parser.add_argument("--workdir", required=True,
                        help="a Git working copy with an .arcconfig")
    parser.add_argument("token")
    parser.add_argument("files", nargs="+")
    args = parser.parse_args()
    methods = args.methods.split(",")

    with open(ROUTES) as routes:
        original = routes.read()
    env = client_env(client_home(args.token))
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
                    results = []
                    for side in ("php", "rust"):
                        with open(ROUTES, "w") as routes:
                            routes.write(original if side == "php" else
                                         route_to_rust(original, methods))
                        reload_nginx()
                        results.append(run_side(scenario, args.workdir, env,
                                                saved, before, known))
                    (code_a, out_a, added_a, removed_a, raw_a) = results[0]
                    (code_b, out_b, added_b, removed_b, raw_b) = results[1]
                    same = (code_a == code_b and out_a == out_b
                            and added_a == added_b and removed_a == removed_b)
                    # Both sides failing proves little, unless the
                    # scenario expects the refusal (`"fails": true`).
                    status = ("FAILED" if code_a and code_b
                              and not scenario.get("fails") else
                              "same" if same else "DIFFERENT")
                    print(f"{status}: {scenario['name']} ({len(added_a)} rows "
                          f"added, {len(removed_a)} removed, exit codes "
                          f"{code_a} {code_b})")
                    if args.show or status != "same":
                        print("    php output:\n      " +
                              raw_a.strip().replace("\n", "\n      "))
                    if args.show:
                        WP.show("added", added_a)
                        WP.show("removed", removed_a)
                    if status != "same":
                        failures += 1
                        if out_a != out_b:
                            print("    rust output:\n      " +
                                  raw_b.strip().replace("\n", "\n      "))
                        WP.show("only php added", added_a - added_b)
                        WP.show("only rust added", added_b - added_a)
                        WP.show("only php removed", removed_a - removed_b)
                        WP.show("only rust removed", removed_b - removed_a)
    finally:
        with open(ROUTES, "w") as routes:
            routes.write(original)
        reload_nginx()
        WP.sh(["docker", "exec", WP.PHP, "/app/phabricator/bin/phd", "start"])
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
