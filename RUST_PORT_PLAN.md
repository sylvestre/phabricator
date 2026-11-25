# Plan: porting Mozilla Phabricator to Rust (axum)

## 1. Starting point

| What | Size |
|---|---|
| PHP in `src/` + `moz-extensions/` | ~490k lines (moz-extensions: ~14k) |
| Applications in `src/applications/` | 40. Largest: diffusion 40k, differential 39k, transactions 26k, repository 24k, auth 21k, calendar 20k, harbormaster 20k |
| Conduit API methods | ~150 (11 of them Mozilla-specific) |
| DAO / storage classes | ~220, spread over one MySQL database per application (`phabricator_<app>`) |
| Schema migrations | 374 `patches/` + 1081 `autopatches/` |
| Front end | ~190 JS files (Javelin) + ~130 CSS files, served by Celerity |
| Tests | ~390 files under `__tests__/` |
| External runtime pieces | arcanist/libphutil (cloned in the `Dockerfile`), pygments, hg/git binaries, MySQL 5.7 |

Phabricator is no longer maintained upstream. Several of the large pieces are
generic frameworks (Lisk ORM, PHID, policy, transactions/editor, search engine,
Remarkup, Celerity). Each feature is written on top of them, so **the
frameworks have to be ported first and must behave exactly like the PHP ones**.

## 2. Go / no-go questions (answer before Phase 1)

1. **Is a rewrite the right investment?** Compare it against (a) keeping PHP and
   moving only the hot or Mozilla-specific paths to Rust services, or (b)
   migrating to another review tool. The plan below lets us stop after any
   phase and still have something useful. Phases 0–2 are worth doing even if
   the full port is abandoned.
2. **What is actually used?** Measure from nginx access logs, the
   `phabricator_conduit.conduit_methodcalllog` table and per-application row
   counts. Applications Mozilla doesn't use (likely calendar, almanac,
   passphrase, phame/ponder-style leftovers, parts of harbormaster) are
   **dropped, not ported**. Commit 0efe619d46 already removed Maniphest, Macro
   and Aphlict, so this trimming has already started.
3. **Which clients must keep working unchanged:** `moz-phab`/arc, Lando,
   the email service (`feed.for_email.*`), BMO, bots, and the review-helper
   service. Their Conduit calls are the compatibility contract.

## 3. Strategy: strangler fig on a shared database

- **The Rust and PHP stacks run side by side** against the **same MySQL
  databases**, with the same schema. Nothing is migrated in one big cutover.
- **nginx routes by path.** It sends `/api/<method>` and selected web routes to
  the Rust service and everything else to PHP. Routing is a per-route allowlist
  that can be flipped back instantly.
- **PHP owns the schema until the end.** `bin/storage upgrade` stays the only
  migration tool. Rust treats the schema as an external contract, checked in CI
  against a database built from `quickstart.sql` + patches. Schema ownership
  moves to Rust (sqlx migrations) only in the last phase.
- **Reads before writes.** Read paths are ported first because they are
  low-risk and easy to compare. Writes go through the transaction editor, which
  is ported once and reused everywhere.
- **Parity is checked by machine, not by eye.** Every ported endpoint runs in
  "shadow" mode first, which proves it matches PHP before it takes traffic.

## 4. Target architecture

Cargo workspace:

```
crates/
  phab-core        PHIDs (PHID-XXXX-…), ids, epoch times, config (reads the same
                   conf/local JSON + phabricator_config.config_entry), errors
  phab-db          sqlx MySQL pools per phabricator_<app> DB; Lisk-equivalent row
                   mapping (JSON + PHP-serialized columns), edge tables, cursor paging
  phab-policy      Viewer, capabilities, global policies (public/users/admin/no-one),
                   custom PHID-PLCY policies, object policies (obj.*), Spaces,
                   Mozilla rules (PhabricatorDifferentialReviewersPolicyRule)
  phab-auth        session cookie (weakDigest-compatible lookup), CSRF tokens,
                   Conduit API tokens, BMO OAuth provider, MFA, high-security mode
  phab-transactions transaction storage, editor pipeline (validate → apply →
                   Herald → feed → mail → search index → workers), comments, inlines
  phab-conduit     method registry, request decoding (params JSON / form / api.token),
                   {result,error_code,error_info} envelope, SearchEngine framework
                   (constraints, attachments, order, cursors), EditEngine framework
  phab-herald      rule storage + evaluation, actions (incl. ReviewHelper action)
  phab-remarkup    Remarkup parser/renderer + markup cache (phabricator_cache)
  phab-diff        changeset parsing, intraline diff, rendering, syntax highlight (syntect)
  phab-search      Ferret full-text engine (MySQL tables)
  phab-files       file storage engines (MySQL blob, local disk, S3), chunking
  phab-mail        outbound mail (metamta), mail tags/preferences
  phab-worker      task queue on phabricator_worker (lease/claim/archive),
                   runs only an allowlisted set of task classes
  phab-repo        hg/git via subprocess, commit import/parse pipeline
  phab-web         axum router, extractors (Viewer, CSRF, Conduit), templates
                   (askama), Celerity compatibility (resource map + packaging)
  apps/{differential,diffusion,people,project,feed,dashboard,settings,...}
  moz/{email,lando,bugzilla,reviewhelper,mergecheck,motd,logging}
bins/
  phab-server      axum HTTP (web + Conduit)
  phab-worker      background task runner
  phab-cli         admin commands (equivalent of bin/* workflows that are kept)
```

Main crates and libraries: `axum`, `tokio`, `tower-http`, `sqlx` (mysql),
`serde`/`serde_json`, `askama`, `syntect`, `similar` (diff), `tracing` +
`sentry` (replaces `SentryLoggerPlugin`), `reqwest` (BMO, Lando, review-helper),
`lettre` (mail), `object_store`/`aws-sdk-s3` (files).

Points that are easy to get wrong:

- **PHP-serialized columns.** `LiskDAO::SERIALIZATION_PHP` is still used (for
  example by `PhabricatorMarkupCache` and `HeraldTranscript`). Rust needs a
  reader for PHP's `serialize` format. While PHP still reads these tables, Rust
  either writes PHP-serialized data or does not write them at all.
- **Sessions.** A user must stay logged in when a request moves between PHP and
  Rust. The cookie format, `PhabricatorHash::weakDigest`/`digestForIndex`
  (HMAC keyed by the installation's secret) and the session table semantics
  (expiry, partial sessions, high-security) must match exactly.
- **The HTML/JS contract.** For the first web phases, the existing Javelin JS
  and CSS stay unchanged. Rust templates must produce the same DOM structure
  and the same `JX.initBehaviors(...)` / `data-sigil` / `data-meta` payloads.
  Rewriting the front end is a separate, optional phase.
- **Worker task class names.** PHP enqueues tasks by PHP class name. The Rust
  worker claims only the classes it implements. A task class must never be
  handled by both stacks at once.
- **Cache invalidation.** Rust writes must invalidate the same caches PHP
  relies on: markup cache, user cache (`phabricator_user.user_cache`) and
  policy/edge-derived caches.

## 5. Parity harness (built in Phase 0, used in every phase)

- **Conduit shadowing.** A tower layer (or an nginx `mirror`) copies read-only
  Conduit requests to Rust. The two JSON responses are normalized (key order,
  floats, cursor tokens) and compared, and mismatches are logged.
- **Fixture DB.** A Docker MySQL is seeded from `quickstart.sql` and then filled
  with scenarios by the *PHP* code (users, projects, revisions with reviewers,
  inlines, policies). Rust integration tests run against it.
- **Port the PHP tests.** The existing `__tests__` are ported alongside the
  code, especially the moz-extensions ones (merge-conflict, result buckets,
  email models, review-helper fixtures) and the framework tests (policy,
  Remarkup, diff parsing).
- **Web flows.** Playwright scenarios (log in, view a revision, comment,
  accept) run against both stacks.
- **Remarkup and diff rendering.** Golden-file tests over a corpus of real
  comments and diffs taken from production (with sensitive content removed).

## 6. Phases

Sizes are relative (S < M < L < XL). Each phase ends with production traffic on
the ported routes and the PHP versions still available for fallback.

### Phase 0: Foundations (M)
- Workspace, CI (fmt, clippy, tests against the fixture DB), Docker image, a
  `docker-compose` service next to `phabricator`, and nginx path routing.
- `phab-core` and `phab-db`: config loading compatible with PHP config,
  connection pools, PHID types for the applications we keep, edges, cursors.
- `phab-auth` (read side): API-token auth, session cookie validation.
- `phab-policy`: a full port of the policy filter and its tests. **This is a
  security boundary, so it gets a separate review.**
- `/status` health check, tracing, Sentry.
- Parity harness (§5).

### Phase 1: Read-only Conduit (L)
Highest value for the least risk: this is mostly machine traffic, and parity
is easy to check.
- Framework: method registry, SearchEngine (constraints, attachments, ordering,
  cursors), `phid.*`, `edge.search`, `transaction.search`.
- `conduit.ping`, `user.whoami`, `user.search`, `project.search`,
  `differential.revision.search` (including the Mozilla attachments
  `MozillaExtraReviewerDataSearchEngineAttachment` and
  `ExternalAccountsSearchEngineAttachment`), `differential.diff.search`,
  `diffusion.repository.search`, `diffusion.commit.search`.
- Mozilla methods: `feed.query_id`, `feed.for_email.query[_id]`,
  `feed.for_email.status[_id]`, `bugzilla.account.search`,
  `bmoexternalaccount.search`, `policy.query`,
  `differential.custom.revision.visibility`.
- The email feed needs the full `moz-extensions/src/email/model` and `resolve`
  logic, which renders revision events for the email service.

### Phase 2: Mozilla background workers (M)
- `phab-worker` with lease/claim semantics that match the PHP task queue.
- `RevisionMergeConflictWorker`, `ReviewHelperAutoRequestWorker` and the
  `ReviewHelperService` HTTP client, plus the `recheck-merge-conflicts`
  management workflow in `phab-cli`.
- These workers write only Mozilla-owned state or call external services, so
  they don't depend on the full editor yet. Any worker that does write
  transactions waits for Phase 3.

### Phase 3: Transaction editor and write Conduit (XL, the core of the port)
- Port `PhabricatorApplicationTransactionEditor` with its full pipeline:
  validation, transaction types, comments and inlines, the subscriber and
  mention engines, Herald (rules, conditions, actions including the Mozilla
  review-helper action), feed stories, mail generation (metamta), search
  indexing, and worker scheduling.
- Event listeners from moz-extensions: Lando, risk analyzer, testing policy,
  new-changes link, review-helper.
- Custom fields: the Bugzilla bug ID field and validator, merge-conflict
  status, uplift request.
- Endpoints: `differential.creatediff`, `differential.setdiffproperty`,
  `differential.revision.edit`, `differential.createcomment`,
  `project.edit`, `user.edit`, `policy.create`, `reviewhelper.request`.
- Contract tests driven by real `moz-phab` and Lando runs against both stacks.
  The Rust editor goes live per transaction type, starting with Differential.

### Phase 4: Web UI, read side (XL) — superseded by §9 (new Rust UI)
- axum web layer: Viewer extractor, CSRF, page chrome (main menu, MOTD,
  `DifferentialRevisionWarning`), Celerity resource map and packaging.
- Remarkup (`phab-remarkup`, including `ReviewHelperFrontmatterRemarkupRule`)
  and diff rendering (`phab-diff`, using syntect instead of pygments).
- Pages: home and dashboards, the Differential revision view and review queue
  (with the result buckets from Bug 2085987: "required action / not
  accepted"), people, projects, the feed, search, files.
- BMO OAuth login (`PhabricatorBMOAuthProvider`) and logout. Password login
  for admins.

### Phase 5: Web UI, write side (L) — superseded by §9 (new Rust UI)
- Comment, inline-comment, action and accept/request-changes forms (all
  through the Phase 3 editor), settings panels, subscriptions, flags, drafts,
  the review-helper controllers, and project and policy editing.

### Phase 6: Repositories and Diffusion (L)
- `phab-repo`: hg/git mirroring and pulls (note the `hgext/` integration),
  commit discovery, the message/change/owners parse workers, and Diffusion
  browse, history and blame views.
- Decide whether repository hosting or SSH (`resources/sshd`) is still needed
  or can be dropped.
- Harbormaster: port it only if the inventory in §2 shows it is used.

### Phase 7: Admin, remainder, decommission (M)
- Config UI or a config CLI, the auth provider admin, the subset of `bin/`
  workflows that is still needed, and daemon management.
- Move schema ownership to sqlx migrations: freeze the PHP patch list and
  take a baseline dump. Remove PHP, Celerity's PHP side and arcanist from the
  image.
- Optional follow-up: replace Javelin with a modern front end, page by page.

## 7. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Policy bugs leak private revisions (security bugs) | Port policy first with exhaustive tests; shadow-compare visibility on every read endpoint; security review before each phase goes live |
| Subtle editor semantics (Herald ordering, mail, feed) | Port as one unit; compare the transaction, feed and mail rows the two stacks produce for the same scripted actions |
| `moz-phab`/Lando break on small JSON differences | Conduit shadowing plus contract tests using the real clients |
| Remarkup or diff rendering differences | Golden corpus from production; cache keys versioned separately so the two renderers don't share cached output |
| Two stacks writing the same rows concurrently | Writes go through one stack per object type at a time; task-class allowlist for workers |
| PHP-serialized data | A dedicated, fuzz-tested PHP `serialize` codec in `phab-db` |
| Scope creep from unused applications | The §2 inventory decides; unused applications are deleted from PHP before porting |
| Long dual-stack period | Every phase ships on its own and can be rolled back per route |

## 8. First concrete steps

1. Run the usage inventory (§2) and agree on which applications are dropped.
2. Create the `rust/` workspace with `phab-core`, `phab-db`, `phab-auth`
   (tokens only) and `phab-policy`, plus the fixture DB in CI.
3. Ship `conduit.ping` and `user.whoami` through nginx routing, then
   `feed.query_id` in shadow mode. That proves the full path: config, DB, auth,
   policy, envelope and parity harness.

## 9. Removing PHP: roadmap with a new Rust UI (decided 2026-11-13)

**Decision.** The goal is now that nobody sees PHP. Rust renders its **own**
web interface instead of reproducing PHP's HTML, Javelin JS and Celerity
CSS. This replaces Phases 4 and 5 above and drops the "HTML/JS contract"
point of §4: no DOM parity, no Celerity, no Javelin. URLs stay the same
(`/D123`, `/p/alice/`, `/feed/`, `/differential/`), because BMO, mail,
moz-phab and bookmarks link to them.

**Where we are (2026-11-13).** Phase 0 is done. Five Conduit methods are
ported (`conduit.ping`, `user.whoami`, `feed.query_id`,
`bugzilla.account.search`, `bmoexternalaccount.search`). The dev stack runs
both services side by side. What is left is about 490k lines of PHP:
infrastructure and the view framework are 81k, and the biggest applications
are diffusion 40k, differential 39k, transactions 26k, repository 24k, auth
21k, calendar 20k, harbormaster 20k, files 17k, search 17k and config 15k.
moz-extensions adds 14k (differential 6.4k, email 3.4k, reviewhelper 2k).

### 9.1 Principles

- **Users stop seeing PHP before PHP is gone.** The new UI comes first.
  Until the Rust transaction editor exists, the Rust UI sends its writes to
  PHP's Conduit API *as the logged-in user*: the session cookie plus a CSRF
  token computed like `PhabricatorUser::getCSRFToken()`. PHP keeps running,
  but only as a hidden backend, and the Rust pages are the only interface.
  It is then replaced from behind (W7 onwards) without users noticing.
- **Opt-in rollout per user.** An nginx `map` on a `phab-ui=rust` cookie
  sends the ported pages to Rust. Each Rust page links "open in the old
  interface" until the PHP page is deleted. Then the opt-in becomes the
  default, and finally the only option.
- **Parity is about data, not pixels.** A Rust page must show the same
  objects, the same visibility (policy) and the same actions as PHP. The
  checks are Playwright flows that compare what is visible and actionable,
  plus golden tests for Remarkup and diff *semantics* (links, mentions,
  code, changed lines), not their HTML.
- **Stack:** axum, askama templates, one small hand-written stylesheet, and
  plain JS (no framework) for the few interactive pieces: inline comment
  drafts, diff context expansion, typeaheads and comment preview.
  `#[non_exhaustive]` view models keep the templates separate from the
  queries.
- **Delete what isn't used.** Before porting an application, check whether
  it is used (§2: access logs, `conduit_methodcalllog`, row counts). Unused
  applications are removed from PHP first and never ported.

### 9.2 Milestones

Sizes are rough estimates for one developer, in weeks (w), and are meant
for ordering the work, not as a promise. Each milestone ships on its own
behind nginx routes and can be rolled back the same way.

| # | Milestone | PHP it removes from view | Needs | Size |
|---|---|---|---|---|
| W0 | Web foundation | — | — | 2w |
| W1 | Remarkup renderer | — | W0 | 4w |
| W2 | Read-only pages: people, projects, feed | `/p/`, `/project/`, `/feed/` | W0, W1 | 3w |
| W3 | Diff rendering | — | W0 | 4w |
| W4 | Revision page, read-only | `/D123` (viewing) | W1, W3 | 5w |
| W5 | Review queue and home, Mozilla buckets | `/`, `/differential/` | W4 | 2w |
| W6 | Review actions through PHP Conduit | `/D123` (writing) | W4, CSRF | 4w |
| W7 | Login with BMO, sessions, logout | `/auth/`, `/login/` | W0 | 3w |
| — | **Milestone A: daily review work never shows PHP** | | W2–W7 | ~27w |
| W8 | Remaining read Conduit (search engines) | API reads | Phase 1 | 6w |
| W9 | Transaction editor and Herald | — (backend) | W1 | 12w |
| W10 | Write Conduit for moz-phab and Lando | API writes | W9 | 6w |
| W11 | Workers and daemons (`phd`) | background jobs | W9 | 6w |
| W12 | Email feed, mail and notifications | `feed.for_email.*`, metamta | W9, W11 | 5w |
| W13 | Repositories (only what is still needed) | diffusion import | W11 | 4–10w |
| W14 | Settings, admin and config (CLI first) | `/settings/`, `/config/`, `bin/*` | W9 | 4w |
| W15 | Schema ownership and removing PHP | everything | all | 3w |
| — | **Milestone B: no PHP left** | | | ~75–80w |

#### W0: Web foundation (2w)

- `phab-web` crate: an axum router for HTML pages next to `/api/`, and a
  `Viewer` extractor built from the `phsid` session cookie (the read side
  is already in `phab-auth`; logged-out users are redirected to PHP's login
  until W7).
- CSRF tokens compatible with PHP (`PhabricatorUser::getCSRFToken()` and
  `validateCSRFToken()`), so forms posted to either stack keep working.
- A page shell: header, search box, user menu, the MOTD (moz-extensions
  `motd`), error pages (404, policy denied, "not ported: open in old UI"),
  and static assets served with content hashes.
- nginx: the opt-in cookie map, plus a route list for pages like
  `rust-routes.conf`.
- Exit: a logged-in user with the cookie sees a Rust placeholder home page,
  and everyone else sees PHP unchanged.

#### W1: Remarkup (4w)

- `phab-remarkup`: a parser and an HTML renderer for the rules Mozilla
  content uses. That covers headers, lists, bold/italic/monospace, code
  blocks (highlighted with syntect), quotes, tables and links, object
  references (`D123`, `rREPO…`, `{F123}`, `@user`, `#project`), `bug
  123456` links, and the review-helper frontmatter rule
  (`ReviewHelperFrontmatterRemarkupRule`).
- Object references resolve under policy: hidden objects render as
  unlinked text, as in PHP.
- A golden corpus of real comments with sensitive content removed. Tests
  compare the semantics: text, link targets, mentions and code blocks.
- Its own cache table, or none at first. It must not share cache keys with
  `PhabricatorMarkupCache`.

#### W2: People, projects and feed pages (3w)

- `/p/<user>/`: the profile, recent activity and projects.
- `/project/…`: name, members, watchers and the revisions tagged with it.
- `/feed/`: story titles rendered as text, which needs a port of
  `getTitleForFeed()` for the Differential transaction types. That code is
  shared with the email feed (W12).
- Exit: these pages pass Playwright parity flows on a seeded database.

#### W3: Diff rendering (4w)

- Load diffs, changesets and hunks, including the compressed and chunked
  hunk storage of `DifferentialHunk`.
- Parse unified diffs, align context, compute intraline changes (ported
  from `PhutilEditDistanceMatrix` or replaced by `similar` with the same
  granularity), and highlight syntax with syntect. Binary, image, moved
  and copied files are shown as such.
- Unified and side-by-side views, with server-rendered context expansion.
- Exit: a golden corpus of real diffs gives the same changed lines and
  intraline ranges as PHP.

#### W4: Revision page, read-only (5w)

- Header: status, author, reviewers with their state (including the
  "blocking" and project reviewers that Mozilla's extra reviewer data
  exposes), the Bugzilla bug field, the stack (dependencies), the Lando
  link (moz-extensions `lando`), `DifferentialRevisionWarning`, the merge
  conflict status and the uplift request field.
- Summary and test plan through Remarkup, the diff (W3) with inline
  comments in place, and the timeline of transactions with comments. The
  timeline titles reuse W2's transaction text.
- History: switching diffs, and diffing one diff against another.
- Exit: on a seeded corpus every timeline entry, inline comment and
  reviewer state that PHP shows is also shown, under the same policy.

#### W5: Review queue and home (2w)

- The Differential dashboard with the Mozilla result buckets (Bug
  2085987: "required action" / "not accepted"), as the home page and
  `/differential/`.

#### W6: Review actions (4w)

- Comment, accept, request changes, resign, commandeer, abandon, reclaim
  and add reviewers or subscribers, plus inline comments with drafts. All
  of them go through PHP's `differential.revision.edit` and
  `differential.createinline` as the user (§9.1).
- Comment preview through the W1 renderer.
- Exit: Playwright runs the review flow (comment, inline, accept) in the
  Rust UI, and the resulting transactions, mail and feed rows match the
  same flow in the old UI.

#### W7: Login (3w)

- The BMO OAuth flow (`PhabricatorBMOAuthProvider`), account linking
  through the external accounts (already read in `phab-app-people`),
  session creation (the write side of `phab-auth`: key generation and the
  stored digest), logout, and the high-security / MFA prompt where Mozilla
  uses it.
- Password login for administrators only.
- Exit: PHP's `/auth/` and `/login/` pages are no longer reachable.

**Milestone A (~27w):** a Mozilla reviewer logs in, reads their queue,
reviews and lands without ever seeing a PHP page. PHP still runs behind
Conduit, the workers and mail.

#### W8–W15: replacing the hidden PHP

- **W8 (6w):** the rest of Phase 1. That is the SearchEngine framework
  (constraints, attachments, order, cursors), `*.search` for revisions,
  diffs, users, projects, repositories and commits, `transaction.search`,
  `edge.search` and the Mozilla attachments.
- **W9 (12w):** the transaction editor of Phase 3 with its full pipeline:
  validation, transaction types, comments and inlines, subscribers and
  mentions, Herald rules, conditions and actions (including review
  helper), feed stories, mail generation, search indexing, and the
  moz-extensions event listeners (Lando, risk analyzer, testing policy,
  new-changes link). From this point W6 switches from PHP Conduit to the
  Rust editor, one transaction type at a time.
- **W10 (6w):** the writes moz-phab and Lando make:
  `differential.creatediff`, `differential.setdiffproperty`,
  `differential.revision.edit`, `differential.createcomment`, and
  `reviewhelper.request`. Contract tests run real moz-phab and Lando
  against both stacks.
- **W11 (6w):** `phab-worker`, with the same lease and claim semantics as
  the PHP task queue. It takes over task classes one at a time (merge
  conflicts, review helper, Herald, mail, search indexing) and finally
  replaces `phd`.
- **W12 (5w):** `feed.for_email.*`, ported from moz-extensions
  `email/model` and `email/resolve` (the email service contract), plus
  outbound mail (metamta) and notifications.
- **W13 (4–10w):** repositories. First decide whether hosting, SSH and
  Diffusion browsing are still needed. Mozilla browses code on
  hg.mozilla.org and Searchfox, so probably not. Revisions do need the
  repository objects, and closing revisions on landing needs either commit
  import or Lando closing them through Conduit. The low estimate drops
  Diffusion; the high one ports commit import.
- **W14 (4w):** settings that people use (email preferences, MFA, API
  tokens), and administration: a CLI replacing the `bin/` workflows that
  are kept, a config CLI, and auth provider management.
- **W15 (3w):** freeze the PHP patch list, take a baseline schema dump and
  move schema ownership to sqlx migrations. Then remove PHP, arcanist and
  libphutil from the image, along with the PHP-serialized cache columns
  (`PhabricatorMarkupCache`, `HeraldTranscript`) once nothing reads them.

**Milestone B (~75–80w for one developer):** no PHP. Work in parallel
shortens it: W1, W3 and W8 are independent of each other, and so are W7
and W9.

### 9.3 Dropped, pending the usage inventory (§2)

These are removed from PHP before the port, unless the inventory shows
they are used: calendar (20k), almanac (10k), passphrase (3k), harbormaster
(20k; check first whether Treeherder or other CI posts build results),
dashboards beyond the home page (8k), Diffusion browsing (see W13), the
config web UI (15k; replaced by a CLI), and the Phame/Ponder leftovers.
Together that is ~75k lines that are never ported.

### 9.4 New risks of a new UI

| Risk | Mitigation |
|---|---|
| Reviewers lose features they rely on (keyboard shortcuts, live preview, typeaheads) | List the features used, using the opt-in group as testers; port shortcuts for the review flow in W4/W6 |
| Bots or scripts scrape PHP HTML | Search the access logs for non-browser agents on HTML routes before W4; point them at Conduit |
| Writing through PHP Conduit as the user differs from the web forms (e.g. drafts, "submit inline comments with the comment") | W6 exit test compares the resulting transactions and mail; anything missing waits for W9 |
| Two renderers of Remarkup and diffs drift apart during the overlap | Golden corpora run in CI against both until PHP's renderers are deleted |
| The opt-in cookie leaks policy differences | Rust pages use the same ported policy filter, and the parity flows include private revisions and custom policies |

### 9.5 Next steps

1. Run the §2 usage inventory: per-route counts from nginx logs, Conduit
   method counts from `conduit_methodcalllog`, and row counts per
   application. It confirms §9.3 and orders W8.
2. Build a seeded development database by driving the PHP stack: users,
   projects, revisions with reviewers, inline comments, policies, stacks
   and BMO accounts. W2–W6 and phab-replay need it.
3. Start W0, then W1 and W3 in parallel.

### 9.6 Where it stands (2026-11-25)

The §9.5 steps are done. By milestone (details in `RUST_PORT_STATUS.md`, counts in
`rust/PORT_INVENTORY.md`):

| Milestone | State |
|---|---|
| W0–W5 | done |
| W6 | revision comments, inlines, subscribe, settings actions, project members and dashboard names are written from Rust; the other forms are left to PHP |
| W7 | done behind `PHAB_RUST_LOGIN` (off by default); waits for the security review |
| W8 | done: 76 read methods shadowed at 0 differences |
| W9 | done for the parity corpus, Herald included (personal rules, repetition policies, blocking reviewers) |
| W10 | ported; `creatediff`, `createrawdiff`, `setdiffproperty` and the verified flag, project, policy, user, file, Diffusion and dashboard writes are routed; the rest waits for moz-phab, Lando and CI contract runs |
| W11 | `phab-worker` runs publish, feed, mail, search, Harbormaster and Git import; `phd` runs the rest |
| W12 | `feed.for_email.*` and outbound mail ported |
| W13 | Git import, the `diffusion.*` methods and pages, `diffusion.commit.edit`; close-on-commit is left |
| W14 | settings pages and actions, read-only admin pages, `phab-cli`; config and auth provider admin are left |
| W15 | not started; only when asked |

Next: close-on-commit, then the contract runs and routing of the Conduit writes, then
the remaining web routes by application (Diffusion, Transactions, People, Files,
Dashboard, Project), then `phd`'s remaining task classes.
