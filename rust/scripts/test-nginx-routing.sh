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
check "the revision edit form goes to Rust" "rust" -X POST -d 'x=1' -b "$optin" "$site/differential/revision/edit/1/"
check "edit form posts Rust leaves are replayed with their body" "php /differential/revision/edit/1/ x=fallback" -X POST -d 'x=fallback' -b "$optin" "$site/differential/revision/edit/1/"
check "other edit forms go to PHP" "php /differential/revision/edit/1/update/ x=1" -X POST -d 'x=1' -b "$optin" "$site/differential/revision/edit/1/update/"
check "subscribing to revisions goes to Rust" "rust" -X POST -d 'x=1' -b "$optin" "$site/subscriptions/add/PHID-DREV-abc/"
check "unsubscribing goes to Rust" "rust" -X POST -d 'x=1' -b "$optin" "$site/subscriptions/delete/PHID-DREV-abc/"
check "subscribing to other objects goes to PHP" "php /subscriptions/add/PHID-TASK-abc/ x=1" -X POST -d 'x=1' -b "$optin" "$site/subscriptions/add/PHID-TASK-abc/"
check "the queue goes to Rust with the opt-in" "rust /differential/" -b "$optin" "$site/differential/"
check "raw diffs go to Rust with the opt-in" "rust /D1?download=true" -b "$optin" "$site/D1?download=true"
check "changes since the last action go to Rust" "rust /D1/new/" -b "$optin" "$site/D1/new/"
check "diffs go to Rust with the opt-in" "rust /differential/diff/1/" -b "$optin" "$site/differential/diff/1/"
check "changeset lists go to Rust with the opt-in" "rust /differential/diff/1/changesets/query/all/" -b "$optin" "$site/differential/diff/1/changesets/query/all/"
check "changesets go to Rust with the opt-in" "rust /differential/changeset/?ref=1" -b "$optin" "$site/differential/changeset/?ref=1"
check "changeset requests Rust leaves are replayed with their body" "php /differential/changeset/ ref=1&fallback=1" -X POST -d 'ref=1&fallback=1' -b "$optin" "$site/differential/changeset/"
check "diff creation goes to PHP" "php /differential/diff/create/ " -b "$optin" "$site/differential/diff/create/"
check "pages not ported go to PHP" "php /differential/revision/inlines/1/ " -b "$optin" "$site/differential/revision/inlines/1/"
check "profiles go to Rust with the opt-in" "rust /p/alice/" -b "$optin" "$site/p/alice/"
check "projects go to Rust with the opt-in" "rust /tag/build/" -b "$optin" "$site/tag/build/"
check "project boards go to PHP" "php /project/board/1/ " -b "$optin" "$site/project/board/1/"
check "profile subpages go to PHP" "php /p/alice/calendar/ " -b "$optin" "$site/p/alice/calendar/"
check "the feed goes to Rust with the opt-in" "rust /feed/?after=12" -b "$optin" "$site/feed/?after=12"
check "feed queries go to Rust with the opt-in" "rust /feed/query/all/" -b "$optin" "$site/feed/query/all/"
check "feed stories go to Rust with the opt-in" "rust /feed/7342/" -b "$optin" "$site/feed/7342/"
check "feed transactions go to PHP" "php /feed/transactions/ " -b "$optin" "$site/feed/transactions/"
check "notifications go to Rust with the opt-in" "rust /notification/" -b "$optin" "$site/notification/"
check "notification queries go to Rust with the opt-in" "rust /notification/query/unread/" -b "$optin" "$site/notification/query/unread/"
check "clearing notifications goes to PHP" "php /notification/clear/ " -b "$optin" "$site/notification/clear/"
check "revisions of a user go to Rust with the opt-in" "rust /people/revisions/1/" -b "$optin" "$site/people/revisions/1/"
check "commits of a user go to PHP" "php /people/commits/1/ " -b "$optin" "$site/people/commits/1/"
check "project members go to Rust with the opt-in" "rust /project/members/1/" -b "$optin" "$site/project/members/1/"
check "adding project members goes to PHP" "php /project/members/1/add/ " -b "$optin" "$site/project/members/1/add/"
check "the manage page of projects goes to Rust with the opt-in" "rust /project/manage/1/" -b "$optin" "$site/project/manage/1/"
check "subprojects go to Rust with the opt-in" "rust /project/subprojects/1/" -b "$optin" "$site/project/subprojects/1/"
check "the project list goes to Rust with the opt-in" "rust /project/" -b "$optin" "$site/project/"
check "project queries go to Rust with the opt-in" "rust /project/query/all/" -b "$optin" "$site/project/query/all/"
check "project edit forms go to Rust with the opt-in" "rust /project/edit/1/" -b "$optin" "$site/project/edit/1/"
check "project edits go to Rust with the opt-in" "rust" -X POST -d 'x=1' -b "$optin" "$site/project/edit/1/"
check "project edits Rust leaves are replayed with their body" "php /project/edit/1/ x=fallback" -X POST -d 'x=fallback' -b "$optin" "$site/project/edit/1/"
check "new projects go to PHP" "php /project/edit/ " -b "$optin" "$site/project/edit/"
check "joining projects goes to Rust" "rust" -X POST -d 'x=1' -b "$optin" "$site/project/update/1/join/"
check "leaving projects goes to Rust" "rust /project/update/1/leave/" -b "$optin" "$site/project/update/1/leave/"
check "watching projects goes to Rust" "rust" -X POST -d 'x=1' -b "$optin" "$site/project/watch/1/"
check "unwatching projects goes to Rust" "rust /project/unwatch/1/" -b "$optin" "$site/project/unwatch/1/"
check "archiving projects goes to PHP" "php /project/archive/1/ " -b "$optin" "$site/project/archive/1/"
check "the people list goes to Rust with the opt-in" "rust /people/" -b "$optin" "$site/people/"
check "people queries go to Rust with the opt-in" "rust /people/query/all/" -b "$optin" "$site/people/query/all/"
check "managing users goes to Rust with the opt-in" "rust /people/manage/1/" -b "$optin" "$site/people/manage/1/"
check "people logs go to PHP" "php /people/logs/ " -b "$optin" "$site/people/logs/"
check "panels go to Rust with the opt-in" "rust /W3" -b "$optin" "$site/W3"
check "the dashboard list goes to Rust with the opt-in" "rust /dashboard/" -b "$optin" "$site/dashboard/"
check "panel queries go to Rust with the opt-in" "rust /dashboard/panel/query/all/" -b "$optin" "$site/dashboard/panel/query/all/"
check "editing panels goes to PHP" "php /dashboard/panel/edit/1/ " -b "$optin" "$site/dashboard/panel/edit/1/"
check "favorites go to Rust with the opt-in" "rust /favorites/" -b "$optin" "$site/favorites/"
check "the favorites configure route goes to Rust" "rust /favorites/menu/configure/" -b "$optin" "$site/favorites/menu/configure/"
check "configuring global favorites goes to PHP" "php /favorites/menu/configure/global/ " -b "$optin" "$site/favorites/menu/configure/global/"
check "portal items go to Rust" "rust /portal/view/1/view/2/" -b "$optin" "$site/portal/view/1/view/2/"
check "editing portals goes to PHP" "php /portal/edit/1/ " -b "$optin" "$site/portal/edit/1/"
check "buildables go to Rust with the opt-in" "rust /B12" -b "$optin" "$site/B12"
check "builds go to Rust with the opt-in" "rust /harbormaster/build/3/1/?l=0" -b "$optin" "$site/harbormaster/build/3/1/?l=0"
check "build plans go to Rust with the opt-in" "rust /harbormaster/plan/2/" -b "$optin" "$site/harbormaster/plan/2/"
check "the buildable list goes to Rust with the opt-in" "rust /harbormaster/" -b "$optin" "$site/harbormaster/"
check "build queries go to Rust with the opt-in" "rust /harbormaster/build/query/all/" -b "$optin" "$site/harbormaster/build/query/all/"
check "build logs go to Rust with the opt-in" "rust /harbormaster/log/view/7/" -b "$optin" "$site/harbormaster/log/view/7/"
check "log downloads go to PHP" "php /harbormaster/log/download/7/ " -b "$optin" "$site/harbormaster/log/download/7/"
check "lint messages go to Rust with the opt-in" "rust /harbormaster/lint/1/" -b "$optin" "$site/harbormaster/lint/1/"
check "unit test results go to Rust with the opt-in" "rust /harbormaster/unit/view/3/" -b "$optin" "$site/harbormaster/unit/view/3/"
check "file pages go to Rust with the opt-in" "rust /F12" -b "$optin" "$site/F12"
check "file data goes to Rust with the opt-in" "rust /file/data/key/PHID-FILE-abc/a.png" -b "$optin" "$site/file/data/key/PHID-FILE-abc/a.png"
check "file data goes to PHP without the opt-in" "php /file/data/key/PHID-FILE-abc/a.png " "$site/file/data/key/PHID-FILE-abc/a.png"
check "file uploads go to PHP" "php /file/upload/ " -b "$optin" "$site/file/upload/"
check "typeaheads go to Rust with the opt-in" "rust" -X POST -d '__ajax__=1' -b "$optin" "$site/typeahead/class/PhabricatorPeopleDatasource/"
check "typeaheads Rust leaves are replayed with their body" "php /typeahead/class/PhabricatorPeopleDatasource/ __ajax__=fallback" -X POST -d '__ajax__=fallback' -b "$optin" "$site/typeahead/class/PhabricatorPeopleDatasource/"
check "other typeaheads go to PHP" "php /typeahead/class/PhabricatorSearchDatasource/ __ajax__=1" -X POST -d '__ajax__=1' -b "$optin" "$site/typeahead/class/PhabricatorSearchDatasource/"
check "change details go to Rust with the opt-in" "rust /transactions/detail/PHID-XACT-DREV-abc/" -b "$optin" "$site/transactions/detail/PHID-XACT-DREV-abc/"
check "raw comments go to Rust with the opt-in" "rust /transactions/raw/PHID-XACT-DREV-abc/" -b "$optin" "$site/transactions/raw/PHID-XACT-DREV-abc/"
check "comment edits go to Rust with the opt-in" "rust /transactions/edit/PHID-XACT-DREV-abc/" -b "$optin" "$site/transactions/edit/PHID-XACT-DREV-abc/"
check "comment edits Rust leaves are replayed with their body" "php /transactions/edit/PHID-XACT-DREV-abc/ text=fallback" -X POST -d 'text=fallback' -b "$optin" "$site/transactions/edit/PHID-XACT-DREV-abc/"
check "comment quotes go to PHP" "php /transactions/quote/PHID-XACT-DREV-abc/ " -b "$optin" "$site/transactions/quote/PHID-XACT-DREV-abc/"
check "the Herald test console goes to Rust with the opt-in" "rust /herald/test/" -b "$optin" "$site/herald/test/"
check "disabling a rule goes to Rust with the opt-in" "rust /herald/disable/3/disable/" -b "$optin" "$site/herald/disable/3/disable/"
check "webhooks go to Rust with the opt-in" "rust /herald/webhook/" -b "$optin" "$site/herald/webhook/"
check "webhook pages go to PHP" "php /herald/webhook/view/1/ " -b "$optin" "$site/herald/webhook/view/1/"
check "rule edits go to PHP" "php /herald/edit/3/ " -b "$optin" "$site/herald/edit/3/"
check "subscriber lists go to Rust with the opt-in" "rust /subscriptions/list/PHID-DREV-abc/" -b "$optin" "$site/subscriptions/list/PHID-DREV-abc/"
check "flags go to Rust with the opt-in" "rust /flag/" -b "$optin" "$site/flag/"
check "Harbormaster commands go to PHP" "php /harbormaster/build/restart/3/ " -b "$optin" "$site/harbormaster/build/restart/3/"
check "the repository list goes to Rust with the opt-in" "rust /diffusion/" -b "$optin" "$site/diffusion/"
check "repository queries go to Rust with the opt-in" "rust /diffusion/query/all/" -b "$optin" "$site/diffusion/query/all/"
check "repository edits go to PHP" "php /diffusion/edit/1/ " -b "$optin" "$site/diffusion/edit/1/"
check "blame goes to Rust with the opt-in" "rust /source/x/blame/master/README" -b "$optin" "$site/source/x/blame/master/README"
check "lastmodified posts go to Rust" "rust" -X POST -d 'paths=1' -b "$optin" "$site/diffusion/GIT/lastmodified/master/"
check "lastmodified posts Rust leaves are replayed with their body" "php /diffusion/GIT/lastmodified/master/ paths=fallback" -X POST -d 'paths=fallback' -b "$optin" "$site/diffusion/GIT/lastmodified/master/"
check "management panels go to Rust with the opt-in" "rust /diffusion/GIT/manage/basics/" -b "$optin" "$site/diffusion/GIT/manage/basics/"
check "repository URI edits go to PHP" "php /diffusion/GIT/uri/edit/ " -b "$optin" "$site/diffusion/GIT/uri/edit/"
check "branches of a commit go to Rust with the opt-in" "rust /diffusion/GIT/commit/abc/branches/" -b "$optin" "$site/diffusion/GIT/commit/abc/branches/"
check "inline diffs of commits go to PHP" "php /diffusion/GIT/diff/ " -b "$optin" "$site/diffusion/GIT/diff/"
check "settings go to Rust with the opt-in" "rust /settings/user/alice/page/datetime/" -b "$optin" "$site/settings/user/alice/page/datetime/"
check "the first settings panel goes to Rust" "rust /settings/user/alice/" -b "$optin" "$site/settings/user/alice/"
check "settings saves go to Rust" "rust" -X POST -d 'x=1' -b "$optin" "$site/settings/user/alice/page/emailpreferences/"
check "settings saves PHP refuses are replayed with their body" "php /settings/user/alice/page/datetime/ x=fallback" -X POST -d 'x=fallback' -b "$optin" "$site/settings/user/alice/page/datetime/"
check "settings panels not ported go to PHP" "php /settings/user/alice/page/ssh/ " -b "$optin" "$site/settings/user/alice/page/ssh/"
check "global settings go to PHP" "php /settings/builtin/global/ " -b "$optin" "$site/settings/builtin/global/"
check "the external editor panel goes to Rust" "rust /settings/user/alice/page/editor/" -b "$optin" "$site/settings/user/alice/page/editor/"
check "address actions go to Rust" "rust /settings/user/alice/page/email/?new=true" -b "$optin" "$site/settings/user/alice/page/email/?new=true"
check "address actions Rust leaves are replayed with their body" "php /settings/user/alice/page/email/ email=fallback" -X POST -d 'email=fallback' -b "$optin" "$site/settings/user/alice/page/email/"
check "token generation goes to Rust" "rust /conduit/token/edit/?objectPHID=PHID-USER-x" -b "$optin" "$site/conduit/token/edit/?objectPHID=PHID-USER-x"
check "token pages go to Rust" "rust /conduit/token/edit/12/" -b "$optin" "$site/conduit/token/edit/12/"
check "token terminations go to Rust" "rust" -X POST -d 'objectPHID=x' -b "$optin" "$site/conduit/token/terminate/12/"
check "token terminations Rust leaves are replayed with their body" "php /conduit/token/terminate/ objectPHID=fallback" -X POST -d 'objectPHID=fallback' -b "$optin" "$site/conduit/token/terminate/"
check "the arc token handshake goes to PHP" "php /conduit/login/ " -b "$optin" "$site/conduit/login/"
check "session terminations go to Rust" "rust /auth/session/terminate/all/" -b "$optin" "$site/auth/session/terminate/all/"
check "leaving high security goes to Rust" "rust" -X POST -d '__form__=1' -b "$optin" "$site/auth/session/downgrade/"
check "temporary token revocations go to PHP" "php /auth/token/revoke/all/ " -b "$optin" "$site/auth/token/revoke/all/"
check "the application list goes to Rust" "rust /applications/" -b "$optin" "$site/applications/"
check "application pages go to Rust" "rust /applications/view/PhabricatorDifferentialApplication/" -b "$optin" "$site/applications/view/PhabricatorDifferentialApplication/"
check "editing applications goes to PHP" "php /applications/edit/PhabricatorDifferentialApplication/ " -b "$optin" "$site/applications/edit/PhabricatorDifferentialApplication/"
check "the config console goes to Rust" "rust /config/" -b "$optin" "$site/config/"
check "config settings go to Rust" "rust /config/settings/advanced/" -b "$optin" "$site/config/settings/advanced/"
check "config options go to Rust" "rust /config/edit/phabricator.base-uri/" -b "$optin" "$site/config/edit/phabricator.base-uri/"
check "config saves Rust leaves are replayed" "php /config/edit/x.y/ value=fallback" -X POST -d 'value=fallback' -b "$optin" "$site/config/edit/x.y/"
check "setup issues go to PHP" "php /config/issue/ " -b "$optin" "$site/config/issue/"
check "config history goes to PHP" "php /config/settings/history/ " -b "$optin" "$site/config/settings/history/"
check "the daemon console goes to Rust" "rust /daemon/" -b "$optin" "$site/daemon/"
check "tasks go to Rust" "rust /daemon/task/12/" -b "$optin" "$site/daemon/task/12/"
check "bulk jobs go to Rust" "rust /daemon/bulk/query/all/" -b "$optin" "$site/daemon/bulk/query/all/"
check "daemon logs go to PHP" "php /daemon/log/ " -b "$optin" "$site/daemon/log/"
check "the method list goes to Rust" "rust /conduit/" -b "$optin" "$site/conduit/"
check "methods go to Rust" "rust /conduit/method/conduit.ping/" -b "$optin" "$site/conduit/method/conduit.ping/"
check "the Conduit token page goes to PHP" "php /conduit/token/ " -b "$optin" "$site/conduit/token/"
check "application policies go to Rust" "rust /policy/explain/PHID-APPS-PhabricatorDifferentialApplication/view/" -b "$optin" "$site/policy/explain/PHID-APPS-PhabricatorDifferentialApplication/view/"
check "other policies go to PHP" "php /policy/explain/PHID-DREV-abc/view/ " -b "$optin" "$site/policy/explain/PHID-DREV-abc/view/"
check "spaces go to Rust" "rust /S1" -b "$optin" "$site/S1"
check "the space list goes to Rust" "rust /spaces/" -b "$optin" "$site/spaces/"
check "mail goes to Rust" "rust /mail/detail/3/" -b "$optin" "$site/mail/detail/3/"
check "the mail list goes to PHP" "php /mail/ " -b "$optin" "$site/mail/"
check "help goes to Rust" "rust /help/documentation/PhabricatorDifferentialApplication/" -b "$optin" "$site/help/documentation/PhabricatorDifferentialApplication/"
check "search jumps go to Rust" "rust /search/?query=D1" -b "$optin" "$site/search/?query=D1"
check "saved searches go to PHP" "php /search/query/all/ " -b "$optin" "$site/search/query/all/"
check "the switch always goes to Rust" "rust /rust-ui/on?next=/D1" "$site/rust-ui/on?next=/D1"
check "Rust static files go to Rust" "rust /rsrc-rust/abc/phab.css" "$site/rsrc-rust/abc/phab.css"
docker stop routing-rust >/dev/null
docker exec routing-nginx nginx -s reload >/dev/null 2>&1
sleep 1
check "Rust service down goes to PHP" "php /api/conduit.ping x=1" -X POST -d 'x=1' "$url"
check "pages go to PHP while Rust is down" "php / " -b "$optin" "$site/"

exit "$failures"
