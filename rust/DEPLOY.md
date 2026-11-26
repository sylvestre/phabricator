# Deploying the Rust service: quick guide

The one-page version, for whoever runs the deployment. [`AGENTS.md`](AGENTS.md)
has the details (every variable, the task workers, the rollout of each
method and page); [`TESTING.md`](TESTING.md) what must pass first.

## How it fits

`phab-server` runs next to PHP and uses PHP's own MySQL databases and
configuration: no copy, no migration. nginx sends it the Conduit methods and
pages listed in `rust/nginx/*.conf`. Everything else stays with PHP, and
anything Rust can't answer exactly it hands back to PHP (HTTP 421, before
writing anything). Without the nginx files, the service gets no traffic.

```
browser / moz-phab / Lando ──> nginx ──> PHP (php-fpm)          everything else
                                  └────> phab-server :8080      routes in rust/nginx/*.conf
                                              │
                               same MySQL, same conf/local/local.json
```

## 1. Check

From `rust/`, with a test MySQL (`eval "$(scripts/test-mysql.sh)"`):

```sh
cargo +nightly fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
PHAB_TEST_REQUIRE_MYSQL=1 cargo test --workspace
scripts/test-nginx-routing.sh        # needs Docker
```

## 2. Build

```sh
docker build -t mozilla/phabricator-rust rust
```

The image (Debian trixie-slim, uid 10001) holds `phab-server` and
`phab-worker`, and listens on 8080. Keep BusyBox in it: changesets are
realigned with BusyBox `diff`, as in PHP's image.

## 3. Run phab-server

It needs, read-only, PHP's `conf/local/local.json` (the database credentials
come from there, then from the `config_entry` table, as in PHP), and network
access to MySQL.

```sh
docker run -d --name phab-rust \
  -v <php app dir>/phabricator/conf/local:/app/phabricator/conf/local:ro \
  -e PHAB_PHP_URL=https://<php internal url> \
  -e PHAB_SHADOW_METHODS=<methods, see step 5> \
  -e PHAB_MIME_COMMAND="file --brief --mime" \
  -e SENTRY_DSN=<dsn> -e RUST_LOG=info \
  mozilla/phabricator-rust
```

| Variable | Needed | Meaning |
|---|---|---|
| `PHAB_CONFIG_DIR` | set in the image | Where `local.json` is (`/app/phabricator/conf/local`) |
| `PHAB_PHP_URL` | yes | PHP's URL, for shadow mode and the forms of Rust pages |
| `PHAB_SHADOW_METHODS` | yes, at first | Methods PHP still answers; Rust only compares |
| `PHAB_MIME_COMMAND` | for commit import | How file types are detected |
| `SENTRY_DSN`, `RUST_LOG` | recommended | Error reporting, log level |
| `PHAB_RUST_LOGIN` | **no** | Rust's own login: off until its security review |

Repository working copies (`/var/repo`, read-only) are needed only for the
`diffusion.*` methods and pages.

## 4. Wire nginx

1. Put `rust/nginx/*.conf` and `rust/nginx/*.inc` at `/app/nginx/rust/` in
   the nginx container. PHP's `nginx/site.conf` already includes
   `/app/nginx/rust/*.conf`.
2. In `proxy.inc`, replace `127.0.0.11` (Docker's DNS) and `phab-rust:8080`
   with the platform's resolver and the service's address. Keep the
   variable in `proxy_pass`: with a literal name, nginx refuses to start
   when it doesn't resolve, which would take PHP down too.
3. `nginx -t && nginx -s reload`.

## 5. First deployment: compare only

Start with nothing answered by Rust:

- Put every method of the read location of `rust-routes.conf` (the first
  `location`) in `PHAB_SHADOW_METHODS`: PHP answers, Rust compares.
- Leave out the write location (the `location` after the "Write methods"
  comment): every write stays with PHP.
- The pages of `rust-web.conf` only go to Rust for people who set the
  `phab_ui=rust` cookie (`/rust-ui/on`); keep that to volunteers.
- Don't run `phab-worker` yet: PHP's `phd` keeps every task.

Then move methods out of shadow mode, and writes into nginx, one at a time,
once they show no differences ([`AGENTS.md`](AGENTS.md), "Rolling out a
Conduit method").

## 6. Check it runs

- `GET /status` on the service: 200 with `"database": "ok"` (503 without
  MySQL), and per method how many shadowed calls matched or differed.
- `/__heartbeat__` goes through Rust when it is up, PHP when it is down.
- The `X-Phab-Fallback` response header says why Rust handed a request to
  PHP.

## 7. Roll back

No rebuild needed:

- One method or page: remove it from `rust/nginx/*.conf`, reload nginx.
- A method giving bad answers: put it back in `PHAB_SHADOW_METHODS`,
  restart the service.
- Everything: remove the files from `/app/nginx/rust/` and reload nginx.
  (Stopping the service works too, but each request then waits for the 2 s
  connect timeout before PHP answers.)

## Not there yet

- No CI job builds or pushes this image: `build-and-push.yaml` builds only
  the PHP one.
- The PHP image doesn't ship `rust/nginx/`: the deployment has to provide
  the files to nginx.
- The policy code (`crates/phab-policy`) decides who sees what: have it
  reviewed before the first production deployment.
