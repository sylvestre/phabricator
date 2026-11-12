#!/bin/sh
# Checks the nginx routing between PHP and the Rust service with stand-ins:
# php-fpm serving a script which echoes the request, and a stub "Rust"
# service which answers, or answers 421 when the body contains "fallback".
set -eu

repo=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
network=phab-routing-test
port=${PHAB_ROUTING_TEST_PORT:-18081}

cleanup() {
    docker rm -f routing-php routing-rust routing-nginx >/dev/null 2>&1 || true
    docker network rm "$network" >/dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT

mkdir -p "$work/webroot" "$work/stub" "$work/nginx"
cp -r "$repo/nginx/." "$work/nginx/"
mkdir -p "$work/nginx/rust"
cp "$repo"/rust/nginx/*.conf "$work/nginx/rust/"
cat > "$work/webroot/index.php" <<'PHP'
<?php echo 'php '.$_GET['__path__'].' '.file_get_contents('php://input');
PHP
cat > "$work/stub/stub.py" <<'PY'
import http.server
class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('content-length', 0)))
        if b'fallback' in body:
            self.send_response(421)
            self.send_header('content-length', '0')
            self.end_headers()
            return
        self.send_response(200)
        self.send_header('content-length', '4')
        self.end_headers()
        self.wfile.write(b'rust')
http.server.HTTPServer(('0.0.0.0', 8080), Handler).serve_forever()
PY

docker network create "$network" >/dev/null
docker run -d --name routing-php --network "$network" --network-alias phabricator \
    -v "$work/webroot:/app/phabricator/webroot:ro" php:8.4-fpm-alpine >/dev/null
docker run -d --name routing-rust --network "$network" --network-alias phab-rust \
    -v "$work/stub:/stub:ro" python:3-alpine python /stub/stub.py >/dev/null
docker run -d --name routing-nginx --network "$network" -p "127.0.0.1:$port:80" \
    -v "$work/nginx:/app/nginx:ro" -v "$work/webroot:/app/phabricator/webroot:ro" \
    -v "$repo/docker/phabricator/site.conf:/etc/nginx/conf.d/default.conf:ro" \
    nginx:alpine >/dev/null
sleep 3

failures=0
check() {
    description=$1
    expected=$2
    shift 2
    actual=$(curl -s -m 15 "$@")
    if [ "$actual" = "$expected" ]; then
        echo "ok    $description"
    else
        echo "FAIL  $description: expected '$expected', got '$actual'"
        failures=$((failures + 1))
    fi
}

url="http://127.0.0.1:$port/api/conduit.ping"
check "routed method goes to Rust" "rust" -X POST -d 'x=1' "$url"
check "421 is replayed against PHP" "php /api/conduit.ping x=fallback" -X POST -d 'x=fallback' "$url"
check "other methods go to PHP" "php /api/user.search x=1" -X POST -d 'x=1' "http://127.0.0.1:$port/api/user.search"
docker stop routing-rust >/dev/null
docker exec routing-nginx nginx -s reload >/dev/null 2>&1
sleep 1
check "Rust service down goes to PHP" "php /api/conduit.ping x=1" -X POST -d 'x=1' "$url"

exit "$failures"
