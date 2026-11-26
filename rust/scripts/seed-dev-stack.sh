#!/bin/sh
# Resets the development stack's database to docker/phabricator/demo.sql.gz,
# brings it to the current schema, and adds users, projects and revisions
# (scripts/seed-dev-stack.php) to try the Rust interface with. Prints a
# web session cookie per user.
#
# DESTROYS the data of the development database.
#
#   docker compose -f docker-compose.yml -f rust/docker-compose.rust.yml up -d
#   rust/scripts/seed-dev-stack.sh
set -eu

repo=$(cd "$(dirname "$0")/../.." && pwd)
db="docker exec phabricator-phabdb-1"
mysql="mysql -uroot -ppassword"

echo "Loading the demo database..."
$db sh -c "for d in \$($mysql -N -e 'SHOW DATABASES LIKE \"phabricator\\\\_%\"' 2>/dev/null); do $mysql -e \"DROP DATABASE \\\`\$d\\\`\" 2>/dev/null; done"
zcat "$repo/docker/phabricator/demo.sql.gz" | docker exec -i phabricator-phabdb-1 sh -c "$mysql 2>/dev/null"

# The demo predates the applications this fork removed: their PHP data
# migrations fail on missing classes, and are marked as applied.
echo "Upgrading the schema..."
for _ in $(seq 1 40); do
  out=$(docker exec phabricator-phabricator-1 sh -c 'cd /app/phabricator && ./bin/storage upgrade --force 2>&1') || true
  if ! echo "$out" | grep -q EXCEPTION; then
    break
  fi
  if ! echo "$out" | grep -q PhutilMissingSymbolException; then
    echo "$out" | grep EXCEPTION
    exit 1
  fi
  patch=$(echo "$out" | grep '^Applying patch' | tail -1 | sed 's/Applying patch "\([^"]*\)".*/\1/')
  echo "  skipping $patch"
  $db sh -c "$mysql -e \"INSERT INTO phabricator_meta_data.patch_status (patch, applied) VALUES ('$patch', UNIX_TIMESTAMP())\" 2>/dev/null"
done

# php-fpm keeps the HMAC keys of the old database in APCu, and nginx
# resolves php-fpm's address when it starts.
echo "Restarting PHP, nginx and the Rust service..."
docker restart phabricator-phabricator-1 >/dev/null
sleep 8
docker restart phabricator-phabricator.test-1 phabricator-phab-rust-1 >/dev/null

echo "Adding users, projects and revisions..."
docker cp "$repo/rust/scripts/seed-dev-stack.php" phabricator-phabricator-1:/tmp/seed-dev-stack.php
docker exec phabricator-phabricator-1 sh -c 'cd /app/phabricator && php /tmp/seed-dev-stack.php'

echo "Adding a Git repository and its commits..."
docker exec -i phabricator-phabdb-1 sh -c "$mysql 2>/dev/null" < "$repo/rust/scripts/seed-dev-git.sql"
docker exec -i phabricator-phabdb-1 sh -c "$mysql 2>/dev/null" < "$repo/rust/scripts/seed-dev-herald.sql"
docker exec -i phabricator-phabdb-1 sh -c "$mysql 2>/dev/null" < "$repo/rust/scripts/seed-dev-ai-change.sql"
docker exec phabricator-phabricator-1 sh -c 'cd /app/phabricator && \
  ./bin/config set --database merge.conflict.enabled true && \
  ./bin/config set --database merge.conflict.repository-phids "[\"PHID-REPO-paritygit000000\"]"' >/dev/null
