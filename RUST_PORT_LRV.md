# lrv: what Fabrique can reuse (2026-11-25)

[lrv](https://github.com/padenot/lrv) (Paul Adenot, MIT or Apache-2.0, Rust + axum
with a JavaScript front end) is a local review UI for unified diffs. It shows a
diff from stdin, a command or a file in the browser, prints the comments people
make as JSON or text, and can show and publish Phabricator review comments.

## 1. A contract client, next to moz-phab and Lando

`src/phabricator.rs` is a real Conduit client. A run of
`lrv --phab-revision D…` against the dev stack checks a real client's read and
publish path through Fabrique:

| Method | What lrv does with it | Fabrique |
|---|---|---|
| `differential.revision.search` | loads the revision | shadowed |
| `differential.diff.search` | the revision's diffs | shadowed |
| `differential.querydiffs` | the active diff and its changesets | shadowed |
| `transaction.search` | inline comments (file, line, side, diff) | shadowed |
| `user.search` | author names | shadowed |
| `phid.query` | object names | shadowed |
| `user.whoami` | the viewer | shadowed |
| `differential.createinline` | one call per drafted inline comment | ported, not routed |
| `differential.createcomment` | publishes the review | ported, not routed |

It is easier to run than Lando: one binary (`cargo install lrv`), with the token
from `PHABRICATOR_API_KEY` or `~/.arcrc`. Its two writes are among the 13 methods
that wait for a contract run before nginx routes them to Rust.

To do: add an lrv run to the contract runs of the plan (W10): read a seeded
revision with inline comments, compare what lrv shows with PHP's answers, then
publish a review through each side and compare the rows with
`rust/scripts/write-parity.py`.

## 2. Ideas for the Rust review page

- Split and stacked diff views, and a commit strip to review a series, which
  fits Phabricator stacks.
- Drafted inline comments, keyboard navigation, and a colour scheme matching
  Phabricator's.
- Playwright end-to-end tests of the review flow (`e2e/tests/`), a model for
  `scripts/browser-check.mjs`.

Fabrique renders diffs on the server, the way PHP does, so the page comparisons
hold. lrv renders them in the browser with Monaco or the diffs.com library
(about 2 MB of JavaScript). Taking its viewer would be a product decision, not a
reuse.

## 3. Not reusable

- Its unified diff parser (`src/diff.rs`): Fabrique uses the port of
  Arcanist's parser, which must match PHP's output exactly.
- Its server, local SQLite store and agent integration: built for a local,
  single-user tool.
