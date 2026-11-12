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
   nginx.

PHP keeps owning the schema: `bin/storage upgrade` is the only migration
tool. The tables and columns the Rust code uses are listed in
`crates/phab-db/src/schema.rs` and checked against
`resources/sql/quickstart.sql`.

## Layout

| Crate | Role |
|---|---|
| `crates/phab-core` | PHIDs, `PhabricatorHash` digests, configuration stack |
| `crates/phab-db` | MySQL access, edges, PHP `serialize()` codec, schema contract, test databases |
| `crates/phab-policy` | Port of `PhabricatorPolicyFilter` (security boundary) |
| `crates/phab-auth` | Conduit API tokens and session cookies |
| `crates/phab-conduit` | Conduit request decoding, dispatch and response envelope |
| `bins/phab-server` | The axum HTTP service |

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
