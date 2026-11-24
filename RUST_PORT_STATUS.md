# Fabrique (the Rust port): status and handoff (2026-11-13)

Implements `RUST_PORT_PLAN.md`: Phase 0 plus the plan's §8 first steps,
moved over one component at a time. Branch: `rust-port`. Nothing is pushed.

## Commits on `rust-port` (oldest first)

1. `rust: add the Cargo workspace and phab-core`: PHIDs, `PhabricatorHash`, Config stack
2. `rust: add phab-db`: MySQL, edges, PHP serialize codec, schema contract, `TestDatabase`
3. `rust: add phab-policy`: `PhabricatorPolicyFilter` port (pure core + DB loader)
4. `rust: add phab-auth`: Conduit API tokens and session cookies
5. `rust: add phab-conduit`: decoding (PhutilQueryStringParser), dispatch, envelope, call log
6. `rust: add phab-server and route requests to it through nginx`: axum, `/status`, 421 fallback
7. `rust: serve conduit.ping from Rust`: first component, `rust/nginx/rust-routes.conf`
8. `rust: keep PHP serving when the Rust service is missing`: runtime resolver, 504
9. `rust: add the parity harness`: phab-parity, shadow mode, phab-replay, X-Phab-Skip-Rust
10. `rust: port user.whoami, in shadow mode`
11. `rust: run the Rust checks in CI`: `.github/workflows/ci.yml` rust job, dependabot cargo
12. `rust: add the Differential objects needed by feed`: subjects and shouldHideForFeed
13. `rust: port feed.query_id, in shadow mode`: crate `crates/apps/feed`
14-26. Style pass (Google Rust style guide, see below), the docs, test
    databases built with the newer autopatches, then
27. `rust: port the BMO account searches, in shadow mode`:
    `bugzilla.account.search`, `bmoexternalaccount.search` (people crate)

Commit conventions used: subject `rust: ...`, back-dated to the previous
evening (2026-11-12, 19:07 to 23:34), following the user's coreutils-claude
rules. Back-dating stopped at 23:59:58 on 2026-11-12: from now on commits keep
their actual dates (the user's decision, 2026-11-13).

## Rust interface (§9 of the plan), 2026-11-13

Commits after `46cad8efff`, on `rust-port`:

- **CSRF** (`phab_auth::csrf`): tokens PHP makes and accepts; checked on
  the dev stack (PHP accepted a Rust token for a session Conduit call).
- **W0** (`crates/phab-web`): viewer from `phsid`, page shell, MOTD,
  hashed static files, error pages, opt-in with `phab_ui=rust`
  (`/rust-ui/on`, `/rust-ui/off`), `rust/nginx/rust-web.conf` +
  `page.inc` + `proxy.inc`. Viewers Rust can't serve get a 421 and PHP.
- **W1** (`crates/phab-remarkup`): the whole generic engine on PCRE2,
  byte-exact with PHP on the 86-file corpus and 30k random documents
  (`tests/oracle/generate.php` + `PHAB_REMARKUP_ORACLE`); Phabricator
  rules D123/{D123}, #project, @user, bug links, review-helper
  frontmatter; two-phase prepare/finish so references resolve under
  policy (`phab_web::remarkup`). Finished on 2026-11-15, see "Remarkup,
  finished" below.
- **W3** (`crates/phab-diff`, by a sub-agent): hunk storage, parser,
  intraline, realign, view model; 29 PHP golden cases. Parity blocker:
  PHP realigns with BusyBox `diff`, the Rust image has GNU diff; install
  busybox and use `DiffCommand::parse("busybox diff")` before the page
  relies on realignment.
- **Projects** (`crates/apps/project`): loading by PHID/slug and the
  project policy (milestones, parents, materialized members).
- **W2**: `/p/<user>/` (profile, roles, review queue count, blurb,
  projects, recent activity), `/tag/<slug>/`, `/project/view|profile/<id>/`,
  and `/feed/` (by a sub-agent, cherry-picked): titles of Differential and
  core transactions checked against PHP itself (62 cases,
  `crates/apps/differential/tests/oracle/titles.php`), handles under
  policy, PHP's paging. Stories about other objects, and titles not
  ported, are counted and left out with a link to the old interface.
- **W4**: `/D<id>`: header, reviewers, Bugzilla bug, stack, Lando link,
  secure revision warnings, summary, test plan, timeline (feed titles,
  not PHP's `getTitle()` wording), active diff (unified, intraline,
  BusyBox diff in the image) with published inline comments. Left:
  inlines of older diffs (line adjustment maps), diff history and
  diff-vs-diff, merge conflict status, uplift request, syntax highlight.
- **W5**: `/` and `/differential/`: the queue with the three buckets
  (stock, Mozilla's with-needs-changes and not-accepted), `?bucket=`.
- **W6, part 1**: the revision page's form sends a comment and an action
  to PHP's `differential.revision.edit` as the user (`phab_web::php`,
  `PHAB_PHP_URL`). Checked on the dev stack: a revision made through
  PHP Conduit shows in Rust, and a comment posted from the Rust form
  reaches PHP and shows in the Rust timeline. Left: inline comment
  drafts, reviewers and subscribers, comment preview.
- **Not started**: W7 (BMO login, sessions; needs the BMO dev service and
  a security review), W8-W15.

The dev stack has revision D1 (made through PHP Conduit with the
`rusttest` token) and the web session `rustwebsessiontoken` for
`rusttest` (inserted by SQL). Note for scripts: in zsh, `path` is
`PATH`.

Dates on pages are UTC until user preferences are ported.

## Session of 2026-11-14: hardening, then "no PHP" (user's direction)

The user's direction (2026-11-14): **no PHP in the end**; don't add new
fallbacks to PHP, replace the existing ones; external crates are welcome
to go faster (syntect for code, Searchfox instead of Diffusion). An
evaluation of the port found the issues fixed by the first commits below.
Commits after `8b56cab441`, oldest first:

- diffs rendered in `spawn_blocking`, `diff` killed after 10 s
  (`DiffCommand::with_timeout`, output to a temp file);
- PHP clients time out (2 s connect, 45 s), 55 s request timeout,
  `CatchPanicLayer`, graceful shutdown; failed writes answer **503**
  (nginx replays 502/504 to PHP, which 404s on `/D123/action`);
  `/status` no longer returns the DB error;
- `WebUser::require_application()` in every handler (installed, app
  policy), Spaces "no access" page, MFA enrollment page
  (`security.require-multi-factor-auth`, factors read each time);
- `remarkup::render_all()`: a page's comments resolve their references
  together;
- PCRE: `(*LIMIT_MATCH=1000000)(*LIMIT_DEPTH=100000)` prefix = PHP's
  limits (the pcre2 crate has no API for them, `unsafe` is forbidden);
- milestones in their parent's space; `need.saturating_mul(10)` in
  feed.query_id;
- tests for the secure revision warnings and hidden/draft inlines;
- syntax highlighting with **syntect** (`phab_web::highlight`, hl-* CSS
  classes, 256 KiB limit like PHP); intraline runs no longer cut chars;
- repository line on the revision page (`phab_app_differential::
  repository`) and **Searchfox** links (`PHAB_SEARCHFOX_TREES`,
  `slug-or-callsign=tree`, set in docker-compose.rust.yml only: check the
  real repository slugs before setting it in production);
- Rust pages for disabled and unapproved accounts (email verification
  still falls back: resending needs mail, W12);
- comment preview (`/rust-ui/preview`, first script `assets/phab.js`);
- add reviewers (`name!` = blocking) and subscribers from the action
  form, resolved server side; typeahead `/rust-ui/typeahead`.
- inline comments and replies written on the diff (kept in localStorage,
  published as this fork's `inline` transactions with the action form);
- `rust/scripts/browser-check.mjs`: headless **Firefox** over WebDriver
  BiDi (the user wants Firefox, never Chromium), passes on D2.

Then: subscribers shown, "×" to remove reviewers and subscribers, Edit
on kept inline comments. W6 left: editing or deleting *published*
inline comments; PHP-side drafts made in the old interface (PHP
publishes them automatically with the next comment). Next big steps: W7 login (needs
the BMO dev service and a security review: ask the user first), W9.

- `phab_conduit::pager` shared by feed.query_id and the search methods;
- `phab_policy::QueryMemory`: PHP's filter keeps the custom policies of
  the first page of a query (`+=` in loadCustomPolicies), so per-object
  rules (subscribers, reviewers, project members) reject objects of later
  pages. Found by shadow mode, reproduced on purpose;
- **differential.revision.search** in shadow mode: all fields and
  attachments, constraints ids, phids, authorPHIDs, repositoryPHIDs,
  statuses, created/modified dates, the orders newest/oldest/updated/
  outdated (and aliases), search limits (1..100). 89 scenarios in
  `parity/differential.json` via `scripts/shadow-replay.py <token>
  parity/differential.json`: 85 match, 4 left to PHP on purpose.
  Left to PHP: responsiblePHIDs, reviewerPHIDs, subscribers, projects,
  query (fulltext), affectedPaths, saved/builtin queryKeys, order
  vectors, relevance, usernames and functions (viewer(), open()) in
  constraint values, cursors the viewer can't see.
  Only alice was used for the shadow runs (her API token is in
  /tmp/alice-api-token on pulau); other viewers would widen coverage.
  PHP oracle trick: a php script in the container (see /tmp/fields.php
  pattern) lists the engine's fields, attachments and orders.
- The tailscale container must not be named `phab-rust` (Docker DNS).

Remaining fallbacks to PHP (421): logged out, partial session (MFA
prompt), unverified email, missing CSRF key, no `PHAB_PHP_URL`, unported
Conduit methods/parameters.

Known: the page tests take ~2 min (each builds ~9 databases from
quickstart.sql, in parallel); failed tests leak `phabtest*` databases
(drop them by hand, `phabe2e*` is the e2e namespace to keep).

## Remarkup, finished (2026-11-15)

Every rule of `PhabricatorMarkupEngine::newMarkupEngine()` is ported,
except those of the applications §9.3 drops (calendar `E1`, credentials
`K1`, builds `B1`, dashboard panels `W1`: their monograms stay text).

- Engine (`phab-remarkup`): `${{{eval}}}`, `{nav}`, `{key}`,
  `@{config:...}`, `{icon}`, `:emoji:`, YouTube (when
  `remarkup.enable-embedded-youtube`), `{img}`, `{PHID-...}`, `H1`, `S1`,
  `rCALLSIGN`, `R1`, commits, `F1`/`{F1, size=full}` (images, video,
  audio, links), `{src}`, links to the install (`https://phab/D1`,
  Diffusion browse links), code blocks highlighted by syntect through
  `Config::highlighter` (console and invisible stay built in; mail stays
  plain). Tables from the PHP sources in `src/generated/` by
  `data/generate.py` (emoji, Font Awesome icons and colours, config keys,
  PHID types). Keys, emoji and eval were compared with PHP on 421
  documents (byte-exact); the 5000-document oracle still passes.
- Loading under policy (`phab_web::remarkup`): new crates
  `phab-app-files` (view policy, author, files attached to a visible
  revision, thumbnails of a visible file; URIs and preview sizes like
  PHP) and `phab-app-herald` (Herald rules: adapter application visible,
  object rules need their project/repository; spaces);
  `phab_app_differential::commit` (`withIdentifiers()`: scoped and bare
  hashes, ambiguity) and repository identifiers (ID, monogram, PHID,
  callsign, short name). `HRUL` is now a PHID type of the policy crate.
- Font Awesome 4.7 (font + CSS from webroot) is served in `phab.css`.

Left, on purpose or for later:

- File contents are still PHP routes: `/file/data/`, `/file/download/`,
  `/file/xform/` (thumbnails not made yet), `/file/imageproxy/` (`{img}`
  always goes through the proxy; PHP's cache of fetched images is not
  read). They move when Files is ported.
- `{src}` without a Searchfox tree, and `@{config:...}`, link to PHP
  pages (Diffusion, config UI); W13/W14 decide.
- `{PHID-...}` shows revisions, projects, users, repositories and files;
  other types stay as written (PHP would show them).
- Files attached to objects other than revisions don't count towards
  visibility (safe side). Commit rewrite hints (`♻`) are not shown.
  Links to the install don't add their objects to the mentioned PHIDs.
- HTML differs from PHP by design for the Phabricator rules (own
  classes, no hovercards/Javelin metadata); the generic rules stay
  byte-exact.

## W8, read Conduit (2026-11-17)

Shared pieces in `phab-conduit`: `search` (constraints with the Conduit
parameter types and PHP's errors, `strict` from form-encoded calls,
attachments, `IdOrder`, the response, `Clauses` for joins and
conditions) and `order` (`formatOrderClause()` and
`buildPagingClauseFromMultipleColumns()`, nullable columns included,
byte-for-byte PHP's SQL). PHP detail worth knowing: string-list and
checkbox fields take an empty list (no constraint); PHID, ID and
datasource fields refuse it. Valid constraint keys are the field keys
(`members`, `parents`), not `memberPHIDs`.

Shadow mode on the dev stack (all in `rust/parity/*.json`, routed in
`nginx/rust-routes.conf` and `PHAB_SHADOW_METHODS`):

| Method | Matched | Differed | Left to PHP |
|---|---|---|---|
| `user.search` | 30 | 0 | 2 |
| `project.search` | 34 | 0 | 3 |
| `diffusion.repository.search` | 22 | 0 | 2 |
| `differential.diff.search` | 15 | 0 | 1 |
| `edge.search` | 12 | 0 | 2 |
| `differential.revision.search` | 117 | 0 | 8 |
| `transaction.search` | 25 | 0 | 5 |
| `phid.query` | 8 | 0 | 1 |
| `phid.info` | 7 | 0 | 0 |
| `phid.lookup` | 7 | 0 | 2 |
| `diffusion.commit.search` | 38 | 0 | 6 |
| `differential.changeset.search` | 13 | 0 | 3 |
| `differential.querydiffs` | 11 | 0 | 0 |
| `differential.getrawdiff` | 11 | 0 | 0 |
| `differential.query` | 27 | 0 | 4 |
| `user.query` | 15 | 0 | 0 |
| `project.query` | 15 | 0 | 2 |
| `file.search` | 14 | 0 | 2 |
| `file.info` | 11 | 0 | 0 |
| `user.find` | 5 | 0 | 4 |
| `differential.getcommitpaths` | 9 | 0 | 4 |
| `repository.query` | 27 | 0 | 3 |
| `conduit.getcapabilities` | 3 | 0 | 0 |
| `differential.getalldiffs` | 7 | 0 | 2 |
| `differential.getdiff` | 20 | 0 | 2 |
| `differential.getrevisioncomments` | 8 | 0 | 1 |
| `differential.getrevision` | 8 | 0 | 4 |
| `diffusion.querycommits` | 18 | 0 | 2 |
| `differential.custom.revision.visibility` | 10 | 0 | 2 |
| `policy.query` | 10 | 0 | 4 |

Left to PHP on purpose: fulltext `query` and `relevance`, saved queries,
order vectors; `user.search` availability; `project.search` member and
watcher usernames; repository `uris`/`projects` constraints and
`uris`/`metrics` attachments; `edge.search` sources other than
revisions and commits, invalid cursors; cursors the viewer can't see
(PHP's policy exception). The dev data is small (one repository, no
subprojects or milestones); diff 8, which has no revision, was made
through PHP (`differential.createrawdiff`, `setdiffproperty`) to cover
`arc:onto` and `local:commits`.

`transaction.search` covers revisions (`objectIdentifier` as a `DREV`
PHID or `D123`, `objectType: "DREV"`): every Differential modular type's
Conduit type and fields (reviewer operations, inline diff/path/
suggestion, update and close commits), the core comment, create,
subscribers and project-edge fallbacks, comment versions (removed ones
withheld), `groupID`. Other object types, unknown or `null` constraints
and cursors on hidden transactions go to PHP. The dev database got an
edited comment (transaction 85), a removed one (87) and a suggestion
(comment 14), inserted by hand, to cover those paths.

`phid.*` (new crate `phab-app-phid`) gives handles of users, revisions,
diffs, projects and repositories under policy; `phid.lookup` takes
PHIDs, `D123`, `@user` and plain `#hashtag`s. Other types and names
(repositories' `rVCT`, tasks) go to PHP.

Integration tests against MySQL: `differential/tests/transaction_search.rs`
and `phid/tests/phid.rs`.

`diffusion.commit.search`: `ids`, `phids`, `repositories`, `authors`,
`statuses`, `unreachable`, `permanent` and `identifiers`, every order but
`relevance`, the fields and the `auditors` attachment. Commits with
audits, `responsible`, `auditors`, `ancestorsOf`, `query`, `subscribers`
and `projects` go to PHP. The dev repository rVCT got five hg commits
(committed in /repo/1 and discovered with `bin/repository discover`);
commit 3 was marked unreachable by hand. Tests:
`diffusion/tests/commit_search.rs`.

`differential.revision.search` now also takes `reviewerPHIDs` (resigned
reviewers don't count), `responsiblePHIDs` (PHP's union of authored and
reviewed, users expanded to their projects), `subscribers` (any) and
`projects` (all; projects with subprojects or milestones go to PHP, as
they also match descendants). Tests: `differential/tests/revision_search.rs`.

`differential.changeset.search`: `diffPHIDs`, `ids`, `phids`, both
orders, the fields. PHP fails ("data is not actually attached") on any
page holding a changeset of a diff without a revision, so those pages,
Subversion repositories and diffs with a source control path go to PHP.
Tests: `differential/tests/changeset_search.rs`.

`differential.querydiffs` (legacy, used by Lando and moz-phab): the
diffs with their changes, hunks (through `phab-diff`), properties and
authorship. Its integers are MySQL's strings, as the Lisk objects keep
them. Tests: `differential/tests/query_diffs.rs`.

`differential.getrawdiff`: `ArcanistBundle::toGitPatch()` for text
changes (`phab_diff::git_patch`: adds, deletes, renames, copies, mode
changes, hunks split with 5 lines of context, tab-terminated paths with
spaces). Binary and image files and multicopies go to PHP. Diff 9 was
made through PHP `differential.createrawdiff` with an add, a rename, a
mode change, a delete, a long hunk and missing final newlines.

`differential.query` (legacy) goes through the `revision.search` query:
`ids`, `phids`, `authors`, `reviewers`, `ccs`/`subscribers`,
`responsibleUsers` (not expanded to projects, unlike the search engine),
legacy `status` constants (accepted counts as closed with
`differential.close-on-accept`), `order` (by update by default),
`limit` and `offset` (counted before revisions without an active diff
are dropped). `commitHashes`, `branches`, `paths` and `none()` go to
PHP. Project and depends-on edges load in one query, as PHP's do: edges
created at the same second come in MySQL's order for that query.
Tests: `differential/tests/legacy_query.rs`.

`user.query` (legacy): users by name, email, real name, PHID and ID,
newest first, with limit and offset, as `user.whoami` describes them
(without the email) plus availability. PHP rebuilds an expired
availability cache from calendar events and writes it; Rust reads a
valid cache, treats users invited to nothing as available, and leaves
the others to PHP. Tests: `people/tests/query.rs`.

`project.query` (legacy) goes through the `project.search` query by
name: `ids`, exact `names`, `phids`, `slugs` (with the slug map),
`colors`, legacy `status` constants, materialized `members` and `limit`.
It declares no cursors, so PHP rejects `after`/`before` as undefined
parameters. `icons` (PHP passes an empty list to `withIcons()`) and
`offset` go to PHP. Tests: `project/tests/legacy_query.rs`.

`file.search` (in `phab-app-files`, which now depends on differential
for the attached objects): `authorPHIDs`, `explicit`, created dates,
`ids`, `phids`, both orders, the fields. Visibility follows
`PhabricatorFileQuery::loadPage()`: the file's policy, its author, an
attached revision or repository the viewer sees, or the original of a
thumbnail (which must be visible too). Other attached objects,
thumbnails of thumbnails and `name` (ngrams) go to PHP. Tests:
`files/tests/search.rs`. `file.info` (legacy) uses the same visibility,
finds deleted files too, and returns MySQL's strings. A file the viewer
can't see goes to PHP: `executeOne()` raises a policy exception.
`differential.getrawdiff` does the same for a hidden diff without a
revision; a diff of a hidden revision is not found, as in PHP.

`user.find` (deprecated) maps usernames to PHIDs, newest first; empty
or unusual alias lists go to PHP (`qsprintf()` throws on an empty list).
Tests: `people/tests/find.rs`.

`differential.getcommitpaths` lists the files of the revision's latest
diff (whatever that diff's policy). Hidden revisions (PHP's `executeOne()`
raises a policy exception) and revisions without a diff (PHP fails) go to
PHP. Tests: `differential/tests/query_diffs.rs`.

`repository.query` (frozen) runs `diffusion.repository.search`'s query
by ID by default (`getDefaultOrderVector()`), with the `order`, `limit`
and cursor parameters `newQueryForRequest()` adds; IDs are MySQL's
strings. `remoteURIs`, order vectors and remote URIs `PhutilURI` would
rewrite (passwords, queries, Git-style `user@host:path`) go to PHP. Dev
data gained repository 2 (git mirror, staging URI, a UUID). Tests: `diffusion/tests/legacy_query.rs`.

The full workspace suite passed on 2026-11-18 after `cargo clean` (the
disk had filled up; `rust/target` had reached 61G).

`differential.getrevision` names commits as their handles do, rewritten
ones with the `♻` hint (`DiffusionCommitHintQuery`); hidden or missing
commits go to PHP. Dev data: D6 has commits 1 and 2, D7 the unreachable
commit 3, rewritten to commit 4.

`diffusion.querycommits` uses `diffusion.commit.search`'s query by ID
(`getDefaultOrderVector()`), with the commit ref read as
`getCommitRef()` does: the `ref` detail's keys win, even null ones, and
the other details complete it. Commits without data get PHP's empty
record. A hidden `repositoryPHID`, and a page cut short while `names` are
mapped (PHP maps the extra row too), go to PHP. Dev data: commit 2 got
the newest date, so the ID and date orders differ.

`differential.custom.revision.visibility` (moz-extensions) reads as the
omnipotent user and returns only the revision ID and its state:
"public", "unprocessed" (the view policy still grants #secure-revision)
or "secure". Dev data: D50 (public) and D51 (a processed custom policy,
`PHID-PLCY-parityprocessed`), each with its own diff (950, 951).

`policy.query` (moz-extensions, `crates/apps/policy`) describes custom
policies in ID order, as MySQL reads them; global, project and user
policies, and more than the limit, go to PHP. Dev data:
`PHID-PLCY-aaaparity` sorts first by PHID but has the highest ID.

Subversion is not ported (only Git matters; Mercurial keeps its two
existing lines, the `default` branch and short hashes, for the dev
repository): `isSVN()` branches were removed.

Not done in W8: commit audits, `transaction.search`
for objects other than revisions, and the `differential.revision.search`
`query` (fulltext), `affectedPaths`, functions like `viewer()`, order
vectors and saved queries.

## Code style

The user asked for the Google Rust style guide
(https://google.github.io/styleguide/rustguide.html):

- `rustfmt.toml` sets `imports_granularity = "Module"` and friends, which
  need nightly rustfmt: format with `cargo +nightly fmt`; CI checks with
  nightly too.
- Call functions through their module (`loader::load_facts()`,
  `call::dispatch()`); import types unqualified.
- `lib.rs` only declares modules and re-exports; enums instead of bool
  parameters; named structs instead of tuples; `expect` over `unwrap`.
- `missing_docs` is on, and Clippy runs with `-D warnings`: document every
  public item, naming the PHP class or method it mirrors.
- PHP value semantics (truthiness, `%s` casts) live in `phab_core::php`.

Not done from the style review: `ApplicationPolicies` is still rebuilt
from the configuration three times per request (store it in `Context`),
and two policy-crate type changes that need a separate parity review:
`Viewer` as an enum (anonymous / user / omnipotent) and
`PolicySubject::space: Option<Option<Phid>>` as an enum.

## `feed.query_id` (done, shadowed)

`crates/apps/feed`: `story.rs` loads pages (`load_page`) and filters them
(`visible_stories`); `query_id.rs` has the parameters, the
`PhabricatorPolicyAwareQuery::execute()` loop and the cursor pager. The
design notes that were here are now in the code and its doc comments.

What is left to PHP (`MethodError::Unsupported`), so shadow traffic will
show how much the Rust side really answers:

- any page with a story about something other than a DREV, or with an
  XACT that isn't XACT-DREV (Mozilla feed has other objects, so expect
  many fallbacks until those object types are ported);
- `core:customfield` and mentioned-by edge transactions, when no other
  transaction of the story is visible;
- stories without an `objectPHID` (renderView() would fail and PHP skips
  them) and story data that isn't a dictionary;
- `view` html, html-summary or text when stories are returned;
- limits other than integers or integer strings, cursors other than
  digit strings or integers, non-string views.

Known gap: PHP calls `renderView()` on every story and skips the ones
that throw. Rust assumes rendering succeeds. Look for this in the shadow
diffs.

Tests: `crates/apps/feed/tests/query_id.rs` covers paging forwards and
backwards, string versus int limits, invalid cursors, ERR-UNKNOWN-TYPE,
fallbacks and overheating. `rust/parity/feed.json` has replay scenarios.
They need a PHP stack with feed data; they have not been run.

## BMO account searches (done, shadowed)

`crates/apps/people/src/{external_account,bugzilla}.rs`. Left to PHP: no
or several BMO provider configs, an account without identifiers, empty
lists, parameters that aren't lists of strings or integers. Accounts and
identifiers are visible to every logged-in user, so no policy filtering.

## Test databases

`quickstart.sql` is a 2019 dump. `TestDatabase` now also applies the `.sql`
autopatches missing from its `patch_status`, so tests and the schema
contract see the current schema (fixtures must fill the newer NOT NULL
columns, e.g. `differential_transaction_comment.attributes`,
`user_email.phid`).

## Running the whole stack (done 2026-11-13)

```sh
docker compose -f docker-compose.yml -f rust/docker-compose.rust.yml up -d --build phabricator.test
curl -x http://localhost:1090 http://phabricator.test/api/conduit.ping     # tinyproxy -> nginx -> Rust
curl -x http://localhost:1090 -H 'X-Phab-Skip-Rust: 1' http://phabricator.test/api/conduit.ping  # PHP
docker compose -f docker-compose.yml -f rust/docker-compose.rust.yml exec phabricator.test \
  wget -qO- http://phab-rust:8080/status                                   # shadow counters
```

The dev database starts empty. A test user `rusttest` with token
`api-rusttestrusttestrusttestrust` was inserted by SQL. With it,
`user.whoami`, `feed.query_id` and `bugzilla.account.search` matched PHP
in shadow mode (feed and bugzilla on empty data only).

Found while running it:

- MySQL 5.7 needed the `nofile` ulimit in `docker-compose.yml` (fixed).
- nginx resolves `fastcgi_pass phabricator:9000` once at startup. After
  the PHP container is recreated, nginx answers 502 until it restarts.
  This affects the PHP stack itself, not the Rust routes, and is not fixed.
- Non-JSON PHP answers were counted as shadow differences (fixed).
- `conduit.ping` answers each container's own host name, so PHP and Rust
  differ there by design.
- Next: seed revisions and feed stories (through PHP, e.g. with arc), then
  run `phab-replay` with `PHAB_REPLAY_TOKEN` set to the token above.

## How to build and test

```sh
cd rust
eval "$(scripts/test-mysql.sh)"   # MySQL 5.7 in Docker on 127.0.0.1:13306 (container phab-rust-test-mysql, still running)
export PHAB_TEST_REQUIRE_MYSQL=1
cargo +nightly fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test
scripts/test-nginx-routing.sh     # nginx + php-fpm + stub service
```

Gotchas found along the way:

- The file-writing tool decodes `\uXXXX` escapes inside content. Write
  expected JSON escapes through python with `chr(92)`.
- MySQL 5.7 in Docker needs `--ulimit nofile=262144:262144` (the script
  does it).
- Test fixtures must fill NOT NULL columns without defaults: `subtype` on
  project, `options` on differential_reviewer, `activeDiffPHID` on
  revisions, `isNewFile`/`lineNumber`/`lineLength`/`hasReplies` on comments.
- `pkill -f phab-server` kills the calling shell too; kill by PID.
- Scratch files outside the repo: `/tmp/e2e.sql` and `/tmp/e2econf` (an
  end-to-end namespace `phabe2e_*` in the test MySQL),
  `/tmp/arcanist` (clone of libphutil, useful for reading PHP helpers).

## Not done yet (beyond feed)

- `rust/Dockerfile` builds (checked 2026-11-13).
- Running shadow mode and `phab-replay rust/parity/feed.json` against a
  real stack, to see the fallback rate and any differences.
- The remaining Phase 1 methods: `*.search`, `feed.for_email.*`,
  `policy.query`, `differential.custom.revision.visibility` and others.
- Phases 2 to 7 of the plan.

## W9, transaction editor (started 2026-11-18)

Write parity: `rust/scripts/write-parity.py [--sides php,rust] [--show]
<token> rust/parity/writes/*.json`. For each scenario it dumps the
`phabricator_*` databases, runs the write on one side, records the rows it
added and removed, restores the databases it changed, and does the same on
the other side; then it compares the changes once PHIDs made by the run,
the current time, durations (also inside serialized Herald transcripts) and
transaction group IDs are normalized. The daemons are stopped meanwhile,
so queued tasks are compared too. `--sides php,php` matches on
`parity/writes/comments.json` (about 50 s per scenario).

A PHP comment on D1 (`differential.revision.edit`, `comment`) writes: the
revision row (`dateModified`), the transaction and its comment, a Herald
transcript (even when no rule matches), and the publish worker task
(`PhabricatorApplicationTransactionPublishWorker`, with the editor state:
mail recipients, feed PHIDs, Herald header). Plan: the Rust editor applies
transactions and Herald, then queues that same PHP worker, so mail, feed
and search indexing stay in PHP until W11 and W12.

The first write: `differential.revision.edit` with one `comment`
(`crates/apps/differential/src/edit.rs`). It plans the edit (every read
and check, Herald, the publish state) and only then writes, in one MySQL
transaction: the revision's `dateModified`, the transaction and comment,
the reviewer's `lastCommentDiffPHID`, the Herald transcript (byte for byte
PHP's, see `crates/apps/herald/tests/transcript.rs`), the rules' applied
state and saved header, and PHP's publish task. PHP's daemons publish the
task (checked: archived as a success, with its feed story and mail).
`crates/apps/herald/src/engine.rs` ports `HeraldEngine` for global rules
repeating on every edit, the `new-object`, `rule` and `projects` fields
and the `email.must-encrypt` action. Left to PHP: comments with remarkup
references or mentions, status changes, draft inline states, buildables,
project watchers and subprojects, other Herald rules, fields and effects.
Accepts and requests for changes (`accept` or `reject` as the defaults,
alone or with a comment) act for the actor and the project reviewers they
belong to (accepts leave out the reviewers who already accepted), and the review status
change they cause is a `differential.revision.status` transaction, as in
`updateReviewStatus()`; so is a stale status a comment fixes. Accepts PHP
refuses (closed, draft, own revision, already accepted), accepts by
non-reviewers and lists of reviewers go to PHP. `resign` only affects the
actor; the author's `plan-changes`, `abandon`, `request-review` and `reclaim` set
the status (abandoning a revision with a repository, which may queue merge
conflict checks, goes to PHP, as do drafts). "Request Review" adds the
`differential.revision.void` transaction which voids accepts (sticky ones
survive "Changes Planned") and rejections; its value is keyed by reviewer
row ID, as `mpull()` leaves it. Write parity on
`parity/writes/accept.json`: 3 the same, 2 left to PHP; on
`parity/writes/reject.json`: 2 the same, 2 left to PHP; on
`parity/writes/actions.json`: 8 the same, 3 left to PHP. Dev data: D51
is abandoned. Dev data: alice
reviews D7.

A new `title` needs `CAN_EDIT` (`storage::can_edit()`); an unchanged
title (no transaction, yet PHP saves and publishes) goes to PHP. Write
parity on `parity/writes/fields.json`: 4 the same, 1 left to PHP.

A comment subscribes its author when nothing else does
(`applyImplicitCC()`: a `core:subscribers` transaction first, and both
edges), and the users it `@mentions` who can see the revision and aren't
subscribed or implicitly subscribed (`buildSubscribeTransaction()`); the
two merge into one transaction (`combineTransactions()`, mentions first),
which comes before the comment when there are only mentions. Mentions are
found with `phab-remarkup` (not in code) and listed newest user first, the
`mention` stamp keyed by PHID. Subscribing others without `CAN_EDIT` makes
PHP throw (revisions have no `CAN_INTERACT`): left to PHP. Write parity on
`parity/writes/mentions.json`: 7 the same.

A comment referencing one revision the actor can see (`D123`, not itself)
adds the "mentions" edge (52) with a `core:edge` transaction before the
comment, then the inverse edit of the mentioned revision
(`applyInverseEdgeTransactions()`): edge 51, its `core:edge` transaction
of the same group, its `dateModified`, and its own publish task after the
revision's, with the parent's `mustEncrypt` and no Herald. References to
other objects (commits, files, tasks) and to several revisions go to PHP.
Write parity on `parity/writes/references.json`: 3 the same, 1 left to
PHP. Dev data: D50's edit policy is
"users". Write parity on `parity/writes/comments.json`: 8 the same, 3 left
to PHP (two hidden revisions, a mention), 0 different. Dev data: D50 is
authored by another user and has a subscriber. The method is not routed by
nginx yet.

A new `summary` or `testPlan` needs `CAN_EDIT` and writes the column with
a `differential.revision.summary` or `differential.revision.testplan`
transaction; an unchanged text goes to PHP. The users it newly mentions
(mentioned in the new text, not in the old one) are subscribed like a
comment's mentions, and so are the revisions it newly references (with
their inverse edits); `Depends on D2` (parent revisions) goes to PHP. A
subscription made of mentions only is written after the other
transactions, as PHP's IDs show. Write parity on
`parity/writes/text-fields.json`: 4 the same, 1 left to PHP; on every
`parity/writes/*.json`: 40 the same, 12 left to PHP, 0 different.

`reviewers.add`, `reviewers.remove` and `reviewers.set` with user and
project PHIDs or `blocking(PHID)` (`crates/apps/differential/src/edit/reviewers.rs`)
need `CAN_EDIT` and write a `differential.revision.reviewers`
transaction, its value the reviewer map in PHP's array order
(`generateNewValue()`: reviewers who stay keep their accept or rejection
unless they become blocking or stop being). Rows of removed reviewers are
deleted, changed ones updated, new ones inserted, and the status
transaction follows when the reviewers call for one. Mail state:
`applyOldRecipientLists()` appends who the edit took off the lists to
`mailToPHIDs` / `mailCCPHIDs` and to the `removed` stamp, not to the feed.
Left to PHP: names and other datasource functions, the author, unknown
users, hidden projects, edits without effect, reviewer edits with a
subscription. Write parity on `parity/writes/reviewers.json`: 9 the same,
2 left to PHP; on every `parity/writes/*.json`: 49 the same, 14 left
to PHP, 0 different.

`subscribers.add`, `.remove` and `.set` (users and projects) and
`projects.add`, `.remove` and `.set` (`edit/subscribers.rs`, `edit/tags.rs`,
`edit/lists.rs` for `getPHIDList()`): subscribing or unsubscribing others
needs `CAN_EDIT`, tags too. Subscriptions write `core:subscribers` with the
whole lists; `PhabricatorSubscriptionsEditor::save()` removes the
"unsubscribed" edge (23/24) of who it subscribes and adds one for who it
unsubscribes, the new edges of the revision numbered in that order. Tags
write `core:edge` (41, inverse 42 in the project database) in the slim
format PHP stores edge transactions in: the removed and the added PHIDs
(`getModernOldEdgeTransactionData()`). The mention edge transactions now
use it too: they used to write the whole lists, wrong once a revision had
mentioned something. Herald then sees the new tags (dropping
secure-revision drops `mustEncrypt`). Left to PHP: names, unknown users,
projects the actor can't see (PHP doesn't check: it tags them), subprojects
and milestones (`applyProjectConflictRules()`), projects with watchers,
edits without effect, subscriber edits with implicit subscriptions. Write
parity on `parity/writes/subscribers-projects.json`: 9 the same, 2 left to
PHP; `references.json` gained a revision which mentioned one before and a
revision mentioned before (5 the same, 1 left to PHP). On every
`parity/writes/*.json`: 60 the same, 16 left to PHP, 0 different.

"Depends on D123" in a new summary or test plan (`edit/dependencies.rs`,
a port of `DifferentialCustomFieldDependsOnParser` without regexes) adds the
"depends on" edge (5) with its `core:edge` transaction and the inverse edit
of the parent (edge 6, its own transaction and publish task), like a
mention, which the dependency comes before. The revisions a dependency
names are not mentioned (`addUnmentionablePHIDs()`); hidden ones are
neither. Mentions and dependencies share `Link` and `plan_link()`. Left to
PHP: several dependencies, cycles (PHP refuses them), reverts. Write parity
on `parity/writes/text-fields.json`: 7 the same, 2 left to PHP; on every
`parity/writes/*.json`: 63 the same, 17 left to PHP, 0 different.

Links now go to several revisions (`Link{edge_type, phid, removed, added,
targets}`): a comment mentioning D7 and D2, or "Depends on D7 and D6", writes
one `core:edge` transaction listing every added PHID, the new edges numbered
seq 0, 1, ... in the order of `DifferentialRevisionQuery` (updated, then ID),
and for each target its inverse transaction, `dateModified` and publish task
in that order. Revisions linked already are dropped. `parents.add`, `.remove`
and `.set`, and the same for `children` (`edit/stack.rs`), edit edges 5 and 6
with PHIDs (monograms go to PHP) and need `CAN_EDIT`: the transaction lists
the removed and the added PHIDs in the request's order, removed targets get an
inverse transaction from `[source]` to `[]`. Hidden revisions to add, the
revision itself and cycles go to PHP.

Comments, summaries and test plans are no longer filtered by a character
heuristic (any `:`, `#`, `{`, URL or digit went to PHP): the remarkup engine,
configured like the pages (`markup::engine_config()`, so self links such as
`http://phabricator.test/D2` count), lists the references, and only commits
(`rVCT…`) and files (`F1`, `{F1}`, which attach) go to PHP; projects, Herald
rules, Spaces and repositories are not mentionable. `T123` is plain text since
Maniphest was removed.

Files referenced in remarkup (`F1` or `{F1}`, outside quotes:
`PhabricatorEmbedFileRemarkupRule`'s attach intent) get the implicit
`core:file` transaction (`edit/files.rs`): old value `[]`, new value
`{"PHID-FILE-…":"reference"}` in order, metadata `attach.implicit` and the
group, and a `file_attachment` row (mode `reference`, the actor as attacher).
Files the old summary or test plan referenced already are dropped. Files the
revision has an attachment for already go to PHP. The files application
depends on Differential, so the editor finds visible files through a
`FileFinder` the files crate registers (`RevisionEdit::with_files()`); without
one (Differential's own tests) file references go to PHP.

Commits referenced in remarkup (`rVCT152e0f678058`, `R2:…`, bare hashes) get
the mention edge (`edit/commits.rs`): newest commit ID first
(`DiffusionCommitQuery`), the inverse edge in `phabricator_repository.edge`,
the inverse `core:edge` transaction in `phabricator_audit.audit_transaction`
(`PHID-XACT-CMIT-…`), no `dateModified` change, and a publish task with the
audit editor's state: mail to the author and the actor, the commit's
subscribers in CC, Diffusion stamps with the full-hash monogram, `custom`
`rawPatch` = base64(`N;`), `affectedFiles` null, `auditorPHIDs` []. Left to
PHP: commits with audit requests or a status other than `none` (the audit
editor would recompute it), commits which don't publish (importing or
herald-disabled repository, not on a permanent ref, not mostly imported),
non-Git repositories, commits whose projects have watchers, and a text
referencing both revisions and commits (the object query's order is unknown).
Linked objects' publish tasks carry the saved Herald header as PHP's
sub-editors load it: `null` when the object has no `herald_savedheader` row
(it was `"none"`; the dev revisions all have rows, so parity hadn't shown it).
The dev stack's Mercurial `rVCT` commits go to PHP (Git only); the Git
scenarios use commits 101–103 of `R2`, which `rust/scripts/seed-dev-git.sql`
adds (`seed-dev-stack.sh` loads it).

Explicit subscriber edits now merge with the comment's implied subscription
(`combineTransactions()` / `mergePHIDOrEdgeTransactions()`): one
`core:subscribers` transaction whose `+` is the mentions, the explicit
additions, then the actor, resolved by `getPHIDList()` (old kept by a set, the
set, the additions, less the removals). It stays first when it holds an
implicit CC, and sits at the explicit edit's place otherwise. Reviewer edits
combine with a subscription too: PHP plans the subscription from the
reviewers before the edit, so a mentioned future reviewer is still subscribed.
Scenarios: `parity/writes/combined.json` (12, all the same).

Several actions in one edit are ported (`Wanted.order`, `Actions`): PHP's
`sortTransactions()` keeps the request's order and moves only the comment
last, so the transactions are written and listed in that order (title then
summary, accept then title, subscribers then tags...). Left to PHP: stack
edits with other actions (their edge is written with the links), "Request
Review" with others (its void is written last), and reviews or author actions
with a reviewer edit (both change the reviewers). Scenarios:
`parity/writes/several.json` (13: 11 the same, 2 left to PHP).

Reverts in a summary or test plan ("Reverts D2", "Backs out commit abcdef",
`DifferentialCustomFieldRevertsParser`) are ported (`edit/reverts.rs`). The
hand-written parser of "Depends on" became a shared
`PhabricatorCustomFieldMonogramParser` (`edit/monograms.rs`) with the two
grammars. Reverted objects get edge 55 (inverse 56) and their inverse edit:
commits first, newest first, then revisions in `DifferentialRevisionQuery`
order; the revision itself and hidden ones are dropped. Reverted objects are
not then mentioned (a reverted commit's bare hash isn't a mention either).
The edge transactions come in PHP's order: dependencies, reverts, mentions.
Reverting a commit that has a revision is left to PHP (PHP reverts the
revision too). The keyword guard on summaries ("revert", "back", "undo")
is gone. Scenarios: `parity/writes/reverts.json` (14, all the same).

Merge conflict rechecks (moz-extensions' `RevisionMergeConflictWorker`) are
queued by the Rust editor (`edit/merge_checks.rs`): abandoning a revision,
or an edge of type 5/6 (parents, children, "Depends on"), queues a
`RevisionMergeConflictWorker` task (priority 3000, data `revisionPHID` and
`diffPHID`) for the revision and everything stacked above it that is open with
an active diff, newest first, when `merge.conflict.enabled` is on and the
revision's Git repository is listed. Linked revisions' sub-editors queue
theirs first; all come before the publish tasks. Before this, Rust only
guarded abandon and would have skipped the checks on stack edits. The dev
stack now enables the check for `R2`, and D1 lands in `R2`
(`seed-dev-git.sql`, `seed-dev-stack.sh`). Scenarios:
`parity/writes/merge-checks.json` (8, all the same).

Herald (H1–H3 of the roadmap):
- Fields (`edit/herald_fields.rs`), computed only when a rule reads them, as
  PHP's adapter returns them: title, summary, test plan (the edit's new
  text), author, author projects (materialized members, by name, keyed by
  project ID), reviewers (keyed by PHID), repository and its projects,
  `revision.status`, `cc`, `contentsource` (`conduit`),
  `herald.acting-user`, `always`, and the active diff's affected paths and
  content, added and removed lines (changesets in ID order, hunks joined
  by newlines, keyed by absolute path). Revisions have no Spaces here, so
  `space` is not a revision field.
- Conditions `is`/`!is` (PHP 8 `==`, numeric strings compared as numbers),
  `regexp`/`!regexp` and `regexp-pair` on PCRE2 itself
  (`phab_remarkup::preg::compile_php()`), modifiers `imsxUS`; `A`, `D`,
  `u`, invalid patterns and PCRE limit errors go to PHP
  (`Evaluation::unsupported`).
- Dev rules 3–8 (`rust/scripts/seed-dev-herald.sql`) read every field on
  every edit; rules 5, 7 and 8 fire on accepted revisions, empty test plans
  and dom/base paths. Rules are inserted only when Rust supports them, since
  one unsupported rule sends every edit to PHP.
- Inserting rules while write-parity runs shows up as a difference (the
  script restores its start snapshot): load rules between runs.
- Actions `subscribers.add` and `subscribers.remove` (`edit/herald_actions.rs`)
  log as `applySubscribe()` does (`do.standard.*`, `do.previously-unsubscribed`
  listing every unsubscriber, `do.autosubscribed`, `do.subscribed` keyed by
  PHID) against the revision as the edit leaves it. Their queued changes
  become the Herald editor's `core:subscribers` transaction, written after
  the transcript: author `PHID-APPS-PhabricatorHeraldApplication`, edit policy
  `no-one`, content source `herald`, metadata `herald:transcriptID` and the
  group. The publish task lists it and sees its effect. When both the edit
  and Herald add subscribers, their edges tie on date and number, and MySQL
  returns them by PHID descending (the `(src, type, dateCreated, seq)` index
  read backwards). Left to PHP: several targets in one action (the object
  query orders them), and adding subscribers when the edit unsubscribes
  someone. Other actions are not ported yet. Dev rules 9 and 10;
  scenarios `parity/writes/herald-actions.json` (10, all the same).
- Actions `differential.reviewers.add` and `.blocking` log as
  `applyReviewers()` (`do.authors`, `do.standard.no-effect` for reviewers
  with a stronger status, the standard target checks,
  `do.add-reviewers`/`do.add-blocking-reviewers`). Unlike subscriptions, the
  reviewer transactions they queue are not merged: each is its own Herald
  transaction whose old value is the reviewers before Herald (old values
  are read before any applies), applied in turn to the rows. A review
  status Herald's reviewers would change goes to PHP. Dev rules 11 and 12;
  `parity/writes/herald-actions.json` now has 21 scenarios, all the same.
- Actions `projects.add`/`projects.remove` (global rules only; merged into
  one `core:edge` Herald transaction whose metadata names `edge:type` first;
  new tags go through `tags::check_added()`), `email.other` (fills
  `heraldEmailPHIDs` in insertion order, logs `do.send`) and `nothing`
  (`do.nothing`). Herald scenarios: 31, all the same.
- Known timing case: "Herald subscribes dave with a comment on D50" differs
  only when the machine is loaded enough for PHP's main editor and its
  Herald editor to write their subscriber edges in different seconds (then
  the tie Rust reproduces doesn't happen). It matched 3 times out of 3 when
  re-run on a quiet machine.

Ported in parallel by three agents, cherry-picked onto rust-port, verified
with the full MySQL suite (752 tests) and shadow replay (0 differences):
audit.query, internal.commit.search, flag.query, remarkup.process (where the
pages' engine and a libphutil-only engine agree), the Harbormaster search and
query methods (not queryautotargets, which creates rows in PHP), and
feed.for_email.query/query_id/status/status_id. Shadowed methods must also be
in `nginx/rust-routes.conf`'s regex, or nginx never sends them to Rust. The
`remarkup` app crate depends on `phab-web` for the renderer (to move to a
shared crate).
- Personal rules (H5): loaded alongside global ones (object rules still go
  to PHP). A personal rule's author must be activated (`isUserActivated()`:
  not disabled, approved, email verified when
  `auth.require-email-verification`) or the rule result is `owner`, and
  must see the revision or it is `view-policy`, checked in PHP's order after
  `version` and `empty`. Actions check `supportsRuleType()`: the self
  actions (`subscribers.self.*`, `differential.reviewers.self.*`,
  `email.self`, which forces mail) only in personal rules, tags only in
  global ones, the others anything but personal; otherwise the effect is
  `applied: false` with PHP's raw `[do.standard.wrong-rule-type, type]`
  log. Dev rules 18–22; Herald scenarios: 37.
- The same timing case shows for tags: "Herald tags D50 while the edit tags
  it too" differs when PHP's two editors write a second apart (machine
  under load).
- Herald checks policies against the revision as the edit leaves it: a
  personal rule's author, and user targets of actions, are checked with
  `storage::visible_after_edit()`, which loads the policy facts and then sets
  "subscribed" and "reviewing" from the post-edit subscribers and reviewers
  (the secure-revision custom policy allows subscribers and reviewers).
  Checking the stored revision instead gave `view-policy` for dave right
  after an edit subscribed him (found by write parity).
- Drafts forbid Herald's mail actions (`email.other`, `email.self`); edits
  of drafts with such rules go to PHP.
- Repetition policies: `first` rules which applied to the revision before
  (`herald_ruleapplied`) get `already-applied` without evaluation; `change`
  rules which match again get `last-match` (a match without effects, which
  rule conditions read as not applying). Then, as `applyEffects()`: rows of
  every non-`first` rule are deleted, and `first`/`change` rules which acted,
  plus `change` rules which matched again, are inserted (INSERT IGNORE).
  Dev rules 23 and 24, with D7 pre-applied; Herald scenarios: 42.
- Herald actions on several targets: `loadStandardTargets()` dedupes them
  (`array_fuse()`) and loads them through `PhabricatorObjectQuery`, which
  groups PHIDs by type in the order types come and returns each type in its
  query's order: users newest first, projects by name. Logs and queued
  transactions follow that order. Dev rules 25 and 26; Herald scenarios: 45.
- Harbormaster pages (agent, merged): `/B<id>`, build, plan and list pages,
  unit results, the revision's Lint and Unit fields and warnings.
  `rust/scripts/seed-dev-harbormaster.php` seeds a plan, revisions D52/D53
  with buildables B1/B2 and builds 1/2 (lint, unit results, a log); its step
  creation needed custom field transactions (fixed).

## W7 login: security review list (2026-11-22)

Rust login is off unless `PHAB_RUST_LOGIN=1` (every login route answers 421,
`auth.logout` isn't registered). Not routed in nginx yet. Needs human review:

- phab-auth/src/session.rs: `SessionDigest::digest`, `anonymous_token`, `establish_session`,
  `upgrade_partial_session`, `logout_session`, `terminate_login_sessions`; `load_user_for_session` exposes `key_digest`
- phab-auth/src/csrf.rs: `CsrfSecret::for_logged_out`, `is_valid_token`
- phab-auth/src/oauth.rs: `client_state`, `is_valid_state` (constant time), `exchange_code`, `load_account`
- phab-auth/src/password.rs: `bcrypt_hash`, `bcrypt_cost`, `is_valid_account_password` (admins only)
- phab-auth/src/rate_limit.rs: `is_blocked`, `record`
- phab-auth/src/external_account.rs: `for_identifiers`, `for_user`, `save`
- phab-web/src/login/mod.rs: `Context::new`, `issue_anonymous_session`, `csrf`, `is_form_post`, `refuse_restricted_parameters`
- phab-web/src/login/request.rs: `rawurldecode`, `parse_cookies`, `cookie`, `remote_address` (last XFF hop),
  `cookie_domain` (Host parsing), `push_cookie`, `redirect`, `redirect_external`, `is_valid_local_uri`
- phab-web/src/login/provider.rs: `handle`, `bmo_login`, `continue_with_account`, `login_user`, `escape_html`,
  `password_login`, `admin_login`
- phab-web/src/login/finish.rs: `show_validate`, `show_finish`; logout.rs `handle`; start.rs `show`
- crates/apps/auth/src/lib.rs: `AuthLogout::execute`

Left to PHP: MFA (TOTP not ported), registration page, linking/refresh, failed password attempts,
`conduit.connect`/`conduit.getcertificate`. Open question: production `X-Forwarded-For` trust.

## 2026-11-23: verified batch

Merged and checked against PHP (write parity, shadow replay):
flag.edit/delete, project.create/edit, differential.creatediff/createrawdiff/setdiffproperty,
file.upload/allocate/uploadchunk/querychunks/download/uploadhash, user.edit/enable/disable,
policy.create, harbormaster.sendmessage/createartifact, 16 diffusion.* Git queries,
getcommitmessage/parsecommitmessage, portal.search; pages for Herald, dashboards/portals,
Diffusion (Git), settings; phab-worker with FeedPublisherWorker; W7 login (off).

Full write parity: 409 scenarios, 326 same, 83 left to PHP, 0 different (two same-second
timing cases pass alone). Workspace tests: 996 passed.

Open: `merge.conflict.status` (DifferentialMergeConflictStatusField) is missing from Rust's
revision.search / differential.query / getrevision since merge conflicts are enabled on the
dev stack; being ported. New write methods are not routed in nginx yet (need a location
without the 502/504 replay to PHP).

Routed to Rust (nginx write location, no 502/504 replay): flag.edit/delete, project.create/edit,
policy.create, user.edit/enable/disable, file.* writes, differential.creatediff/setdiffproperty.
Contract run: moz-phab 2.15.3 submit to R3 (FAB, `/var/repo/3`, created for contract runs)
gave identical diff rows from PHP (D954) and Rust (D955). Revision creation is refused by
PHP's Mozilla checks (alice has no BMO account; test plan required). Run moz-phab with an
isolated HOME (`.moz-phab-config`: self_auto_update False, report_to_sentry False) and
HTTP_ALLOWED=1 against http://phab-rust-1.tail153c3.ts.net/.

## W7 login, part 2: security review list (2026-11-23)

Still off unless `PHAB_RUST_LOGIN=1`; nothing routed in nginx. TOTP MFA on `/auth/finish/` and
the hisec prompt, BMO registration (`/auth/register/<key>/`), account link/refresh/confirmlink.
Needs human review (all under `rust/crates/`):
- phab-core/src/hash.rs `hmac_sha1_raw`
- phab-auth/src/totp.rs: `TotpSecret::new`, `key`, `code`, `window_contains`, `valid_timestep`
- phab-auth/src/rate_limit.rs `TRY_FACTOR`
- phab-auth/src/mfa.rs: `is_answered`, `is_reused`, `is_high_security`, `load_factors`,
  `load_challenges`, `TokenKey::digest`, `attach_response_tokens`, `wait_reason`, `response`,
  `plan`, `insert_challenge`, `update_challenge`, `execute`
- phab-auth/src/register.rs: `is_valid_username`, `is_valid_address`, `username_exists`,
  `email_exists`, `create`, `write_approval_mail`
- phab-auth/src/external_account.rs: `for_secret`, `user_has_account`, `save_on`
- phab-web/src/login/mod.rs: `is_form_post`, `validate_csrf`; finish.rs `show_finish`
- phab-web/src/login/link.rs: `logged_in_context`, `show_link`, `load_linkable_account`,
  `registration_key_matches`, `show_confirm`
- phab-web/src/login/provider.rs: `handle` (logged-in callers), `link_account`,
  `set_account_key_and_continue`; register.rs `handle`; viewer.rs `WebUser::from_headers`
Not yet compared with live PHP (rows matched by reading PHP; TOTP codes checked against PHP 8.4).

## Settings actions: security review list (2026-11-23)

Behind the `phab_ui=rust` opt-in cookie (like every Rust page), not behind `PHAB_RUST_LOGIN`:
Conduit API token generate/view/terminate, session terminate/downgrade, email add/delete/
verify/make-primary (with TOTP hisec), External Editor panel. Review (under `rust/crates/`):
- phab-auth/src/conduit_token.rs: `load_live`, `load_live_for_object`, `create_standard`, `touch`, `terminate`
- phab-auth/src/session_actions.rs: `terminate`, `exit_high_security`
- phab-auth/src/email.rs: `load`, `verification_link`, `removal_has_side_effects`, `lock_user`,
  `reload`, `is_add_rate_limited`, `add`, `remove`, `save_primary_flag`, `make_primary`,
  `send_verification`, `revoke_password_reset_links`, `queue_mail`
- phab-auth/src/rate_limit.rs `record_on`; user_log.rs `insert_on`
- phab-web/src/settings/dialog.rs: `is_form_post`, `require_csrf`, `require_high_security`
- phab-web/src/settings/tokens.rs: `token_owner` (bot/admin authorization), `load_token`,
  `generate`, `view` (shows the secret), `terminate_all`, `terminate`, `terminate_tokens`
- phab-web/src/settings/sessions.rs: `terminate`, `downgrade`; emails.rs `new_address`,
  `delete_address`, `verify_address`, `primary_address`
- phab-web/src/viewer.rs `session_user` (exposes the CSRF secret to the MFA engine);
  login/request.rs `passthrough_parameters`

## 2026-11-24: final verification against PHP

Same dev-stack snapshot on both sides, PHP's daemons stopped:
- Conduit writes (full regression): 368 same, 128 left to PHP, 0 different.
- New Conduit + editor writes (conduit2, editor2): 33 same, 50 left to PHP, 0 different.
- Web form writes: 72 + settings 16 same, 0 different after harness fixes (59 + 7 left to PHP).
- Workers: publish/feed/mail 42 same (26 to PHP); search 45; Harbormaster 37; Git import and
  publish 31 (1 to PHP); update-after-commit 4 (1 to PHP); 0 different.
- Read replay (shadow): 75 methods, 0 differences.
- Workspace tests: 1445 passed, 0 failed (private tmpfs MySQL on :13399).
- Pages (`scripts/page-compare.py`): same data; presentation differs (no advanced search
  forms, ISO dates, no remarkup toolbar, no calendar panel). Feed, profile and notification
  pages omit commit and buildable stories with a "not shown here yet" notice linking to PHP.
Intermittent: same-second edge ordering in two-editor writes (createrevision with a tag,
accept D7, a Harbormaster target); they pass when re-run alone.
