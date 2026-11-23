#!/bin/sh
# Checks the nginx routing between PHP and the Rust service with stand-ins:
# php-fpm serving a script which echoes the request, and a stub "Rust"
# service which answers, or answers 421 when the body or the query string
# contains "fallback".
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
cp "$repo"/rust/nginx/*.conf "$repo"/rust/nginx/*.inc "$work/nginx/rust/"
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
    def do_GET(self):
        if 'fallback' in self.path:
            self.send_response(421)
            self.send_header('content-length', '0')
            self.end_headers()
            return
        body = ('rust ' + self.path).encode()
        self.send_response(200)
        self.send_header('content-length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
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
check "skip header goes to PHP" "php /api/conduit.ping x=1" -X POST -H 'X-Phab-Skip-Rust: 1' -d 'x=1' "$url"
check "other methods go to PHP" "php /api/maniphest.search x=1" -X POST -d 'x=1' "http://127.0.0.1:$port/api/maniphest.search"

site="http://127.0.0.1:$port"
optin='phab_ui=rust'
check "pages go to PHP without the opt-in" "php / " "$site/"
check "pages go to Rust with the opt-in" "rust /" -b "$optin" "$site/"
check "other opt-in cookies go to PHP" "php / " -b 'phab_ui=php' "$site/"
check "page fallbacks are replayed against PHP" "php / " -b "$optin" "$site/?fallback=1"
check "skip header goes to PHP for pages" "php / " -b "$optin" -H 'X-Phab-Skip-Rust: 1' "$site/"
check "revisions go to Rust with the opt-in" "rust /D1" -b "$optin" "$site/D1"
check "review actions go to Rust with the opt-in" "rust" -X POST -d 'x=1' -b "$optin" "$site/D1/action"
check "comments go to Rust with the opt-in" "rust" -X POST -d 'x=1' -b "$optin" "$site/differential/revision/edit/1/comment/"
check "comments Rust leaves are replayed with their body" "php /differential/revision/edit/1/comment/ x=fallback" -X POST -d 'x=fallback' -b "$optin" "$site/differential/revision/edit/1/comment/"
check "comments go to PHP without the opt-in" "php /differential/revision/edit/1/comment/ x=1" -X POST -d 'x=1' "$site/differential/revision/edit/1/comment/"
check "other edit forms go to PHP" "php /differential/revision/edit/1/ x=1" -X POST -d 'x=1' -b "$optin" "$site/differential/revision/edit/1/"
check "subscribing to revisions goes to Rust" "rust" -X POST -d 'x=1' -b "$optin" "$site/subscriptions/add/PHID-DREV-abc/"
check "unsubscribing goes to Rust" "rust" -X POST -d 'x=1' -b "$optin" "$site/subscriptions/delete/PHID-DREV-abc/"
check "subscribing to other objects goes to PHP" "php /subscriptions/add/PHID-TASK-abc/ x=1" -X POST -d 'x=1' -b "$optin" "$site/subscriptions/add/PHID-TASK-abc/"
check "the queue goes to Rust with the opt-in" "rust /differential/" -b "$optin" "$site/differential/"
check "pages not ported go to PHP" "php /differential/diff/1/ " -b "$optin" "$site/differential/diff/1/"
check "profiles go to Rust with the opt-in" "rust /p/alice/" -b "$optin" "$site/p/alice/"
check "projects go to Rust with the opt-in" "rust /tag/build/" -b "$optin" "$site/tag/build/"
check "project boards go to PHP" "php /project/board/1/ " -b "$optin" "$site/project/board/1/"
check "profile subpages go to PHP" "php /p/alice/calendar/ " -b "$optin" "$site/p/alice/calendar/"
check "the feed goes to Rust with the opt-in" "rust /feed/?after=12" -b "$optin" "$site/feed/?after=12"
check "feed queries go to PHP" "php /feed/query/all/ " -b "$optin" "$site/feed/query/all/"
check "buildables go to Rust with the opt-in" "rust /B12" -b "$optin" "$site/B12"
check "builds go to Rust with the opt-in" "rust /harbormaster/build/3/1/?l=0" -b "$optin" "$site/harbormaster/build/3/1/?l=0"
check "build plans go to Rust with the opt-in" "rust /harbormaster/plan/2/" -b "$optin" "$site/harbormaster/plan/2/"
check "the buildable list goes to Rust with the opt-in" "rust /harbormaster/" -b "$optin" "$site/harbormaster/"
check "Harbormaster commands go to PHP" "php /harbormaster/build/restart/3/ " -b "$optin" "$site/harbormaster/build/restart/3/"
check "settings go to Rust with the opt-in" "rust /settings/user/alice/page/datetime/" -b "$optin" "$site/settings/user/alice/page/datetime/"
check "the first settings panel goes to Rust" "rust /settings/user/alice/" -b "$optin" "$site/settings/user/alice/"
check "settings saves go to Rust" "rust" -X POST -d 'x=1' -b "$optin" "$site/settings/user/alice/page/emailpreferences/"
check "settings saves PHP refuses are replayed with their body" "php /settings/user/alice/page/datetime/ x=fallback" -X POST -d 'x=fallback' -b "$optin" "$site/settings/user/alice/page/datetime/"
check "settings panels not ported go to PHP" "php /settings/user/alice/page/ssh/ " -b "$optin" "$site/settings/user/alice/page/ssh/"
check "global settings go to PHP" "php /settings/builtin/global/ " -b "$optin" "$site/settings/builtin/global/"
check "the switch always goes to Rust" "rust /rust-ui/on?next=/D1" "$site/rust-ui/on?next=/D1"
check "Rust static files go to Rust" "rust /rsrc-rust/abc/phab.css" "$site/rsrc-rust/abc/phab.css"
docker stop routing-rust >/dev/null
docker exec routing-nginx nginx -s reload >/dev/null 2>&1
sleep 1
check "Rust service down goes to PHP" "php /api/conduit.ping x=1" -X POST -d 'x=1' "$url"
check "pages go to PHP while Rust is down" "php / " -b "$optin" "$site/"

exit "$failures"
