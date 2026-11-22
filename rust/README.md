# Rust port

A Rust service that takes over Phabricator one component at a time. It runs
next to the PHP application, against the same MySQL databases, and nginx
decides per route which stack answers.

## How a request flows

1. nginx sends the routes listed in `rust/nginx/*.conf` to the `phab-rust`
   service and everything else to PHP (`nginx/site.conf` includes
   `/app/nginx/rust/*.conf`).
2. If the Rust service can't answer exactly like PHP (a parameter, an
   authentication mechanism or an object type that is not ported yet), it
   replies `421 Misdirected Request` with an `X-Phab-Fallback` header saying
   why, and nginx replays the request against PHP.
3. To give a route back to PHP, remove it from `rust/nginx/*.conf` and reload
   nginx. nginx also needs a reload when a file is added there.

## Web pages

The Rust service renders its own interface (`crates/phab-web`), not PHP's
HTML. Users opt in: `/rust-ui/on?next=/` sets the `phab_ui=rust` cookie and
`/rust-ui/off` clears it, and `rust/nginx/rust-web.conf` sends the ported
pages to Rust only when the cookie is set. Every page links back to the old
interface. Viewers Rust can't serve yet (logged out, multi-factor prompt
pending, account not activated) get a `421` and see the PHP page.

Pages ported so far:

| Route | Page |
|---|---|
| `/`, `/differential/` | Review queue, with Mozilla's buckets (`?bucket=`) |
| `/D<id>` | Revision: header, reviewers, bug, stack, secure revision warnings, build status, Lint and Unit fields, unit test results, lint/unit/draft warnings, summary, test plan, timeline, active diff with its inline comments |
| `/p/<user>/` | Profile, with the user's recent activity |
| `/tag/<slug>/`, `/project/view/<id>/`, `/project/profile/<id>/` | Project |
| `/feed/` | Feed |
| `/B<id>` | Buildable: properties, lint messages, unit test results, builds and their targets, timeline |
| `/harbormaster/build/<id>/`, `/harbormaster/build/<id>/<run>/` | Build: runs, targets with their artifacts, messages and logs, timeline |
| `/harbormaster/plan/<id>/` | Build plan: steps, behaviors, Herald rules, recent builds, timeline |
| `/harbormaster/`, `/harbormaster/plan/` | The default lists of buildables and of active plans |

Stories, transactions and parts of a page that aren't ported are left out
with a link to the old interface.

Pages share the session cookie with PHP, and their CSRF tokens are the ones
PHP makes and accepts (`phab_auth::csrf`), so forms and Conduit calls posted
to PHP from a Rust page work. Changes to objects still go through PHP: the
revision page's form posts to `/D<id>/action`, which sends the
comment and the action to PHP's `differential.revision.edit` as the user
(`phab_web::php`, `PHAB_PHP_URL`).

PHP keeps owning the schema: `bin/storage upgrade` is the only migration
tool. The tables and columns the Rust code uses are listed in
`crates/phab-db/src/schema.rs` and checked against
`resources/sql/quickstart.sql`.

## Checking parity with PHP

A method is moved in three steps:

1. **Shadow.** List it in `rust/nginx/rust-routes.conf` and in the service's
   `PHAB_SHADOW_METHODS`. The service forwards each call to PHP
   (`PHAB_PHP_URL`, with `X-Phab-Skip-Rust` so nginx sends it to PHP),
   returns PHP's answer, computes its own without writing anything and logs
   the differences. `GET /status` counts matching, differing and skipped
   calls per method.
2. **Replay.** `phab-replay <php-url> <rust-url> rust/parity/*.json` sends
   the calls of the scenario files to both stacks and reports differences;
   it exits with 1 if any.
3. **Serve.** Once shadow traffic shows no differences, remove the method
   from `PHAB_SHADOW_METHODS`: the Rust answer goes to clients.

`rust/scripts/test-nginx-routing.sh` checks the nginx side with stand-ins.
[`TESTING.md`](TESTING.md) describes the whole testing strategy and what
each ported component needs before it serves traffic.

## Layout

| Crate | Role |
|---|---|
| `crates/phab-core` | PHIDs, `PhabricatorHash` digests, configuration stack |
| `crates/phab-db` | MySQL access, edges, PHP `serialize()` codec, schema contract, test databases |
| `crates/phab-policy` | Port of `PhabricatorPolicyFilter` (security boundary) |
| `crates/phab-auth` | Conduit API tokens, session cookies and CSRF tokens |
| `crates/phab-conduit` | Conduit request decoding, dispatch and response envelope |
| `crates/phab-parity` | Compares PHP and Rust answers |
| `crates/phab-remarkup` | Remarkup engine, checked against PHP's corpus and a PHP oracle |
| `crates/phab-web` | Web viewer, page shell, static files and the opt-in switch |
| `crates/phab-diff` | Differential diffs, changesets and hunks: storage, line alignment, intraline changes |
| `crates/apps/conduit` | `conduit.ping` |
| `crates/apps/people` | `user.whoami`, `bugzilla.account.search`, `bmoexternalaccount.search` |
| `crates/apps/differential` | Revisions, their details, transactions and comments, feed titles, review queue buckets |
| `crates/apps/feed` | `feed.query_id`, and the feed stories with their titles and handles |
| `crates/apps/project` | Projects: loading by PHID or hashtag, and their policy |
| `bins/phab-server` | The axum HTTP service |
| `bins/phab-replay` | Replays scenario files against both stacks |

## Development

```sh
cd rust
cargo test                       # unit tests; database tests are skipped
eval "$(scripts/test-mysql.sh)"  # starts MySQL 5.7 in Docker
cargo test                       # now also runs the database tests
cargo +nightly fmt               # rustfmt.toml needs nightly rustfmt
cargo clippy --all-targets --all-features -- -D warnings
```

Run the whole stack with the Rust service:

```sh
docker compose -f docker-compose.yml -f rust/docker-compose.rust.yml up
```

`scripts/seed-dev-stack.sh` resets the development database to the demo
(`docker/phabricator/demo.sql.gz`, which has the `admin` user) and adds users,
projects and revisions in every review state, printing a `phsid` cookie per
user. With the cookie set, open `http://phabricator.test/rust-ui/on`
through the proxy on `localhost:1090`.

To use the stack from other machines, `rust/docker-compose.tailscale.yml`
puts it on a tailnet (its header says how to log the node in), and
`./bin/auth recover <user> --force-full-session`, run in the PHP container,
prints a one-time login link: BMO login isn't set up on the stack.
`rust/scripts/browser-check.mjs` checks the revision page's scripts in
headless Firefox.

The service reads the same configuration as PHP: `PHAB_CONFIG_DIR`
(default `/app/phabricator/conf/local`) for `local.json`, then the
`config_entry` table. `PHAB_LISTEN` sets the address (default
`0.0.0.0:8080`), `SENTRY_DSN` enables error reporting, `RUST_LOG` the log
level, `PHAB_DIFF_COMMAND` the `diff` used to realign changesets (the
image uses `busybox diff`, like PHP's) and `PHAB_PHP_URL` where PHP answers,
for shadow mode and the forms of the Rust pages. `PHAB_SEARCHFOX_TREES`
maps repositories, by short name or callsign, to Searchfox trees
(`mozilla-central=mozilla-central`): their files link to Searchfox, which
Mozilla uses instead of Diffusion. `GET /status` reports the database state
and the ported methods.
