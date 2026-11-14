#!/usr/bin/env python3
"""Sends the calls of parity scenario files through nginx on the development
stack, where the Rust service shadows them (PHAB_SHADOW_METHODS), then prints
the shadow counters (which add up since the service started) and the
differences the service logged during the run.

    rust/scripts/shadow-replay.py <api token> rust/parity/differential.json...

Unlike phab-replay, this needs no access to the Rust service from the host:
the service compares its answers with PHP's itself.
"""

import json
import subprocess
import sys
import time

PROXY = "http://localhost:1090"
STACK = "http://phabricator.test"


def main():
    token, files = sys.argv[1], sys.argv[2:]
    started = time.strftime("%Y-%m-%dT%H:%M:%S")
    methods = set()
    for path in files:
        with open(path) as scenarios:
            for scenario in json.load(scenarios):
                params = dict(scenario.get("params", {}))
                if scenario.get("token"):
                    params["__conduit__"] = {"token": token}
                methods.add(scenario["method"])
                subprocess.run(
                    ["curl", "-s", "-o", "/dev/null", "-x", PROXY,
                     f"{STACK}/api/{scenario['method']}",
                     "--data-urlencode", "params=" + json.dumps(params),
                     "-d", "output=json", "-d", "__conduit__=1"],
                    check=True)
    status = json.loads(subprocess.run(
        ["docker", "exec", "phabricator-phabricator.test-1",
         "wget", "-qO-", "http://phab-rust:8080/status"],
        capture_output=True, text=True, check=True).stdout)
    for method in sorted(methods):
        print(method, status["shadow"].get(method, "not shadowed"))
    logs = subprocess.run(
        ["docker", "logs", "--since", started, "phabricator-phab-rust-1"],
        capture_output=True, text=True)
    for line in (logs.stdout + logs.stderr).splitlines():
        if "answers differ" in line:
            print(json.loads(line)["fields"]["differences"])


if __name__ == "__main__":
    main()
