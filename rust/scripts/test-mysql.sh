#!/bin/sh
# Starts the MySQL server the integration tests use and prints the variables
# to export. Same image and settings as the `phabdb` Compose service.
#
#   eval "$(rust/scripts/test-mysql.sh)"
#   cargo test --manifest-path rust/Cargo.toml
set -eu

name=phab-rust-test-mysql
port=${PHAB_TEST_MYSQL_PORT:-13306}
repo=$(cd "$(dirname "$0")/../.." && pwd)

if [ -z "$(docker ps -q --filter "name=^${name}$")" ]; then
    docker rm -f "$name" >/dev/null 2>&1 || true
    docker run -d --name "$name" \
        -e MYSQL_ROOT_PASSWORD=password \
        --ulimit nofile=262144:262144 \
        -p "127.0.0.1:${port}:3306" \
        -v "$repo/docker/phabricator/mysql:/etc/mysql/conf.d:ro" \
        mysql:5.7 >/dev/null
fi

until docker exec "$name" mysqladmin ping -h 127.0.0.1 -ppassword --silent >/dev/null 2>&1; do
    sleep 1
done

echo "export PHAB_TEST_MYSQL_URL=mysql://root:password@127.0.0.1:${port}"
