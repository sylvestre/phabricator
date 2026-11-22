# Deploying the Rust service

How to build, run and roll out `phab-server`, the Rust service that takes
over Phabricator one route at a time. [`README.md`](README.md) explains the
design, [`TESTING.md`](TESTING.md) the tests a component needs before it
serves traffic.

## What gets deployed

Three pieces, which ship separately:

| Piece | Where | Ships with |
|---|---|---|
| `phab-server` image | `rust/Dockerfile` | Built by hand (see "Production" below) |
| Routing rules | `rust/nginx/*.conf`, `*.inc` | Mounted at `/app/nginx/rust/` |
| Shadow list | `PHAB_SHADOW_METHODS` of the service | The service's environment |

nginx (`nginx/site.conf`) includes `/app/nginx/rust/*.conf`. With no file
there, every request goes to PHP and the Rust service is inert. That is
the state of a PHP image built from the root `Dockerfile`: it copies
`nginx/` but not `rust/nginx/`.

## Before deploying

Run from `rust/`, all must pass:

```sh
eval "$(scripts/test-mysql.sh)"   # without it, database tests are skipped
cargo +nightly fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
scripts/test-nginx-routing.sh      # after any change to rust/nginx/ (needs Docker)
```

A green `cargo test` without `PHAB_TEST_MYSQL_URL` proves nothing about
queries or the policy loader: set `PHAB_TEST_REQUIRE_MYSQL=1` to make
skipped database tests fail, as CI does.

## Build the image

```sh
docker build -t mozilla/phabricator-rust rust
```

The image is `debian:trixie-slim` with the `phab-server` binary, running as
uid 10001 and listening on 8080. It installs BusyBox because PHP's image
realigns changesets with BusyBox `diff`; don't drop it, or diff pages stop
matching PHP.

## Development stack

From the repository root:

```sh
docker compose -f docker-compose.yml -f rust/docker-compose.rust.yml up -d --build phab-rust
```

The overlay adds the `phab-rust` service, mounts `rust/nginx` into the
nginx container and shares the PHP container's `/app` read-only, so both
stacks read the same `conf/local/local.json`. `scripts/seed-dev-stack.sh`
fills the database with users and revisions. To reach the stack from
other machines, add `rust/docker-compose.tailscale.yml` (its header lists
the one-time setup).

After changing `rust/nginx/`, reload nginx (adding or removing a file
needs it too):

```sh
docker compose -f docker-compose.yml -f rust/docker-compose.rust.yml \
  exec phabricator.test nginx -s reload
```

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `PHAB_CONFIG_DIR` | `/app/phabricator/conf/local` | Where `local.json` is; the `config_entry` table is read after it, as PHP does |
| `PHAB_LISTEN` | `0.0.0.0:8080` | Listen address |
| `PHAB_PHP_URL` | none | PHP's base URL, for shadow mode and the forms of the Rust pages |
| `PHAB_SHADOW_METHODS` | none | Comma-separated Conduit methods answered by PHP and only compared by Rust |
| `PHAB_DIFF_COMMAND` | `busybox diff` in the image | `diff` used to realign changesets |
| `PHAB_SEARCHFOX_TREES` | none | `repo=tree` pairs whose files link to Searchfox |
| `SENTRY_DSN` | none | Error reporting; off without it |
| `RUST_LOG` | | Log level (`info`) |

The database credentials come from Phabricator's own configuration, not
from the service's environment.

In `rust/nginx/proxy.inc`, `resolver 127.0.0.11` is Docker's DNS and
`phab-rust:8080` the service's address. Outside Docker Compose, change both
to the platform's resolver and service name. Keep the variable in
`proxy_pass`: with a literal host name, nginx refuses to start when the
name doesn't resolve, which takes PHP down with it.

## Rolling out a Conduit method

Never skip a step, and never move a method past shadow mode without the
user's go-ahead.

1. **Shadow.** Add the method to the `location` regex of
   `rust/nginx/rust-routes.conf` *and* to `PHAB_SHADOW_METHODS`, restart the
   service, reload nginx. PHP still answers; Rust compares and logs the
   differences.
2. **Watch.** `curl http://phab-rust:8080/status` (from inside the network)
   shows, per method, how many calls matched, differed or were skipped.
   Replay the scenario files too:
   `cargo run -p phab-replay -- <php-url> <rust-url> parity/*.json`
   (exits 1 on any difference).
3. **Serve.** Once shadow traffic shows no differences, remove the method
   from `PHAB_SHADOW_METHODS` and restart the service. nginx doesn't change.

Only route read-only methods: nginx replays a request against PHP when the
service times out or is down, which would apply a write twice.

## Rolling out a web page

Pages go to Rust only for users with the `phab_ui=rust` cookie
(`/rust-ui/on`, `/rust-ui/off`); everyone else gets PHP. Add a `location`
with `include /app/nginx/rust/page.inc;` to `rust/nginx/rust-web.conf` and
reload nginx. Writes from Rust pages go through PHP's Conduit API
(`PHAB_PHP_URL`) and must answer failures with 503, which nginx doesn't
replay. The settings pages write preferences themselves, as PHP's editor
does: they answer 421 only before writing, so the replay writes once.

## Running task classes in Rust

`phab-worker` runs the task queue classes ported to Rust
(`bins/phab-worker/src/handlers`) next to PHP's `phd`. Its lease query adds
`taskClass IN (<ported classes>)` to PHP's, so it never leases another
class and the two can run on the same queue. It reads the same
configuration as `phab-server` (`PHAB_CONFIG_DIR`), plus:

| Variable | Default | Meaning |
|---|---|---|
| `PHAB_WORKER_CLASSES` | every ported class | Comma-separated classes to run; the others stay with PHP |
| `PHAB_WORKER_TASKMASTERS` | `1` | Tasks run at once |

To hand a class back to PHP, remove it from `PHAB_WORKER_CLASSES` and
restart: tasks it holds are leased again by PHP once their lease expires.

A class can be ported for only some of its tasks: its handler inspects
each task before the lease and declines the others, which are never
leased, so PHP's taskmasters take them as if `phab-worker` didn't exist
(`Task N (...) left to PHP` in the log). `FeedPublisherWorker` declines
stories about objects other than revisions, projects and users. A task
declined after its lease (its story changed in between) gets its lease
columns back as they were.

`phab-worker --once [--id N]` leases and runs one task, prints the outcome
as JSON and exits (3 when the task was left to PHP).
`rust/scripts/worker-parity.py` compares its rows with PHP's
`bin/worker execute --id` on the dev stack.

## Checking a deployment

- `GET /status` on the service: 200 with `"database": "ok"` and the list of
  ported methods; 503 if MySQL is unreachable (the error itself is only
  in the log).
- `/__heartbeat__` is rewritten to `/api/conduit.ping`, which is routed to
  Rust: it checks the Rust path when the service is up, and PHP when it's
  down (nginx falls back on 502/504).
- The `X-Phab-Fallback` response header says why Rust handed a request to
  PHP (`421`).

## Rolling back

Rolling back is a config change, no rebuild needed:

- One method or page: remove it from `rust/nginx/*.conf`, reload nginx.
- A method serving bad answers: put it back in `PHAB_SHADOW_METHODS`.
- Everything: remove the files from `/app/nginx/rust/` (or stop mounting
  them) and reload nginx. Stopping the service also works: nginx falls
  back to PHP on 502/504, but every request then waits for the 2 s
  connect timeout first.

## Production

There is no automated production deployment yet. `.github/workflows/build-and-push.yaml`
builds and pushes only the PHP image (`phabricator`); nothing builds or
pushes `rust/Dockerfile`, and the PHP image doesn't ship `rust/nginx/`.
Deploying to production needs, first:

1. A CI job that builds and pushes the Rust image.
2. A way to put `rust/nginx/*.conf` at `/app/nginx/rust/` in the nginx
   container, with `proxy.inc` pointing at the platform's resolver and
   service.
3. The service deployed next to PHP, with access to the same configuration
   and MySQL.

Do these only when the user asks, and start with every ported method in
`PHAB_SHADOW_METHODS`. The policy code (`crates/phab-policy`) is a security
boundary: changes to it need a dedicated review before they deploy.
