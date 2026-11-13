"""Writes titles.json, the cases of titles.php and tests/titles.rs."""

import json
import os

ALICE = "PHID-USER-alice"
BOB = "PHID-USER-bob"
HIDDEN = "PHID-USER-hidden"
BUILD = "PHID-PROJ-build"
TWO = "PHID-PROJ-two"


def case(kind, old=None, new=None, metadata=None, author=ALICE):
    return {
        "type": kind,
        "old": old,
        "new": new,
        "metadata": metadata or {},
        "author": author,
    }


cases = [
    case("differential:inline"),
    case("differential:update"),
]
for action in [
    "accept", "reject", "rethink", "abandon", "commit", "request_review",
    "reclaim", "resign", "claim", "reopen", "none", "update", "create",
    "add_ccs", "close",
]:
    cases.append(case("differential:action", new=action))
for kind in [
    "abandon", "accept", "close", "commandeer", "plan", "reclaim", "reject",
    "reopen", "resign", "summary", "testplan",
]:
    cases.append(case("differential.revision." + kind, new=True))
cases += [
    case("differential.revision.request", new=True),
    case("differential.revision.request", new=True,
         metadata={"promotion.source": "author"}),
    case("draft", new=True),
    case("draft", new=False),
    case("differential.revision.title", old="Old <x>", new="New"),
    case("differential.revision.author", old=BOB, new=ALICE),
    case("differential.revision.author", old=ALICE, new=BOB),
    case("differential.revision.author", old=BOB, new=HIDDEN),
    case("differential.revision.status", new="accepted"),
    case("differential.revision.status", new="needs-revision"),
    case("differential.revision.status", new="needs-review"),
    case("differential.revision.reviewers", old={}, new={BOB: "added"}),
    case("differential.revision.reviewers", old={},
         new={BOB: "added", BUILD: "added"}),
    case("differential.revision.reviewers", old={BOB: "added"}, new={}),
    case("differential.revision.reviewers",
         old={BOB: "added", HIDDEN: "added"}, new={}),
    case("differential.revision.reviewers", old={BOB: "added"},
         new={BUILD: "added"}),
    case("differential.revision.reviewers", old={BOB: "added"},
         new={BOB: "blocking"}),
    case("differential.revision.reviewers", old={BOB: "blocking"},
         new={BOB: "added", BUILD: "added"}),
    case("differential.revision.reviewers",
         old={BOB: "blocking", BUILD: "added"},
         new={BOB: "added", BUILD: "blocking"}),
    case("core:create"),
    case("core:comment"),
    case("core:comment", author=HIDDEN),
    case("core:view-policy", old="users", new="admin"),
    case("core:edit-policy", old="users", new="admin"),
    case("core:subscribers", old=[], new=[BOB]),
    case("core:edge", old={}, new={BUILD: {"dst": BUILD}},
         metadata={"edge:type": 41}),
    case("core:edge", old={}, new={BUILD: {"dst": BUILD}, TWO: {"dst": TWO}},
         metadata={"edge:type": 41}),
    case("core:edge", old={BUILD: {"dst": BUILD}}, new={}, metadata={"edge:type": 41}),
    case("core:edge", old={BUILD: {"dst": BUILD}}, new={TWO: {"dst": TWO}},
         metadata={"edge:type": 41}),
    case("core:edge", old={}, new={BOB: {"dst": BOB}}, metadata={"edge:type": 35}),
    case("core:edge", old={}, new={}, metadata={"edge:type": 41}),
    case("core:customfield", old="", new="123",
         metadata={"customfield:key": "differential:bugzilla-bug-id"}),
    case("core:customfield", old="", new="{}",
         metadata={"customfield:key": "differential:uplift-request"}),
    case("core:customfield", old="a", new="b",
         metadata={"customfield:key": "differential:merge-conflict-status"}),
]

path = os.path.join(os.path.dirname(__file__), "titles.json")
with open(path, "w") as output:
    json.dump(cases, output, indent=1)
    output.write("\n")
