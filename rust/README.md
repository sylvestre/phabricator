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

Pages share the session cookie with PHP, and their CSRF tokens are the ones
PHP makes and accepts (`phab_auth::csrf`), so forms and Conduit calls posted
to PHP from a Rust page work.

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
| `crates/apps/feed` | `feed.query_id` |
| `bins/phab-server` | The axum HTTP service |
| `bins/phab-replay` | Replays scenario files against both stacks |

## Development

```sh
cd rust
cargo test                       # unit tests; database tests are skipped
eval "$(scripts/test-mysql.sh)"  # starts MySQL 5.7 in Docker
cargo test                       # now also runs the database tests
cargo clippy --all-targets --all-features -- -D warnings
```

Run the whole stack with the Rust service:

```sh
docker compose -f docker-compose.yml -f rust/docker-compose.rust.yml up
```

The service reads the same configuration as PHP: `PHAB_CONFIG_DIR`
(default `/app/phabricator/conf/local`) for `local.json`, then the
`config_entry` table. `PHAB_LISTEN` sets the address (default
`0.0.0.0:8080`), `SENTRY_DSN` enables error reporting and `RUST_LOG` the log
level. `GET /status` reports the database state and the ported methods.
