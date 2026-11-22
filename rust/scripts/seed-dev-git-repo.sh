#!/bin/sh
# Builds the working copy of the dev stack's Git repository R2 (see
# seed-dev-git.sql), which the diffusion.* query methods read and
# parity/diffusion-git.json replays against:
#
#   rust/scripts/seed-dev-git-repo.sh [directory]   # default /var/repo/2
#
# Authors, committers and dates are fixed, so the commit hashes are the
# same everywhere and the scenarios can name them. The directory must not
# exist yet. Both the PHP and the Rust containers must see it at R2's
# localPath (/var/repo/2/), owned by the user Git runs as.
#
# History (newest first):
#
#   *   Merge branch 'feature'           (master; merges "Add b.txt")
#   |\
#   | * Add b.txt                        (feature)
#   * | Delete a.txt
#   |/
#   * Add a.txt and a link               (tag v2, annotated)
#   * Add the README                     (tag v1, lightweight)
set -eu

target=${1:-/var/repo/2}
if [ -e "$target" ]; then
  echo "$target already exists" >&2
  exit 1
fi
mkdir -p "$target"
cd "$target"

export GIT_AUTHOR_NAME=alice GIT_AUTHOR_EMAIL=alice@example.com
export GIT_COMMITTER_NAME=alice GIT_COMMITTER_EMAIL=alice@example.com
export GIT_CONFIG_NOSYSTEM=1 HOME=/nonexistent
export LANG=C

commit() {
  GIT_AUTHOR_DATE="$1 +0000" GIT_COMMITTER_DATE="$1 +0000" \
    git commit --quiet --no-gpg-sign --allow-empty-message -m "$2"
}

git init --quiet --initial-branch=master .
printf 'Hello\n' > README
git add README
commit 1794600000 "Add the README"
GIT_COMMITTER_DATE="1794600000 +0000" git tag v1

mkdir dir
printf 'one\ntwo\nthree\n' > dir/a.txt
printf 'Hello\nWorld\n' > README
ln -s README link
git add README dir/a.txt link
commit 1794600100 "Add a.txt and a link"
GIT_COMMITTER_DATE="1794600100 +0000" git tag --annotate -m "Version 2" v2

git checkout --quiet -b feature
printf 'bee\n' > dir/b.txt
git add dir/b.txt
commit 1794600200 "Add b.txt"

git checkout --quiet master
git rm --quiet dir/a.txt
commit 1794600300 "Delete a.txt"
GIT_AUTHOR_DATE="1794600400 +0000" GIT_COMMITTER_DATE="1794600400 +0000" \
  git merge --quiet --no-ff --no-gpg-sign -m "Merge branch 'feature'" feature

git log --format='%H %s' --all
