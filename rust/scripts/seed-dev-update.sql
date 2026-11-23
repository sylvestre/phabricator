-- Diffs no revision has yet, for the write-parity scenarios of
-- differential.revision.edit's `update` transaction and of creations
-- (parity/writes/create.json), which can't create diffs themselves: each
-- scenario is a single call. Load it after scripts/seed-dev-stack.sh:
--
--   docker exec -i phabricator-phabdb-1 sh -c 'mysql -uroot -ppassword' \
--     < rust/scripts/seed-dev-update.sql
--
-- 9001: Alice's, in R2, a change and a new file in a new directory, with
-- its local commit (the revision hashes) and Alice's view state of it.
-- 9002: Alice's, without a repository. 9003: Bob's, which Alice can't see.

INSERT IGNORE INTO phabricator_differential.differential_diff (id, phid,
  revisionID, authorPHID, repositoryPHID, sourceMachine, sourcePath,
  sourceControlSystem, sourceControlBaseRevision, sourceControlPath,
  lintStatus, unitStatus, lineCount, branch, bookmark, creationMethod,
  dateCreated, dateModified, description, repositoryUUID, viewPolicy,
  commitPHID)
SELECT 9001, 'PHID-DIFF-parityupdate0001', NULL, phid,
  'PHID-REPO-paritygit000000', 'parity-laptop', '/home/alice/parity', 'git',
  '9f1c2e3d4b5a69788796a5b4c3d2e1f00f1e2d3c', '/', 0, 0, 7, 'HEAD', NULL,
  'moz-phab-git', 1794602000, 1794602000, NULL, 'uuid-parity-git', phid, NULL
  FROM phabricator_user.user WHERE userName = 'alice'
UNION ALL
SELECT 9002, 'PHID-DIFF-parityupdate0002', NULL, phid, NULL, 'parity-laptop',
  '/home/alice/notes', NULL, NULL, NULL, 0, 0, 2, NULL, NULL, 'arc',
  1794602000, 1794602000, NULL, NULL, phid, NULL
  FROM phabricator_user.user WHERE userName = 'alice'
UNION ALL
SELECT 9003, 'PHID-DIFF-parityupdate0003', NULL, phid,
  'PHID-REPO-paritygit000000', 'bob-laptop', '/home/bob/parity', 'git',
  '9f1c2e3d4b5a69788796a5b4c3d2e1f00f1e2d3c', '/', 0, 0, 3, 'HEAD', NULL,
  'moz-phab-git', 1794602000, 1794602000, NULL, 'uuid-parity-git', phid, NULL
  FROM phabricator_user.user WHERE userName = 'bob';

INSERT IGNORE INTO phabricator_differential.differential_changeset (id, diffID,
  oldFile, filename, awayPaths, changeType, fileType, metadata, oldProperties,
  newProperties, addLines, delLines, dateCreated, dateModified, phid) VALUES
(90011, 9001, 'README.md', 'README.md', '[]', 2, 1, '{"line:first":2}', '[]',
  '[]', 1, 1, 1794602000, 1794602000, 'PHID-DCNG-parityupdate0011'),
(90012, 9001, NULL, 'docs/parity/guide.md', '[]', 1, 1, '{"line:first":1}',
  '[]', '{"unix:filemode":"100644"}', 3, 0, 1794602000, 1794602000,
  'PHID-DCNG-parityupdate0012'),
(90021, 9002, 'notes.txt', 'notes.txt', '[]', 2, 1, '{"line:first":1}', '[]',
  '[]', 1, 1, 1794602000, 1794602000, 'PHID-DCNG-parityupdate0021'),
(90031, 9003, 'README.md', 'README.md', '[]', 2, 1, '{"line:first":2}', '[]',
  '[]', 2, 1, 1794602000, 1794602000, 'PHID-DCNG-parityupdate0031');

INSERT IGNORE INTO phabricator_differential.differential_hunk (id, changesetID,
  oldOffset, oldLen, newOffset, newLen, dataType, dataEncoding, dataFormat,
  data, dateCreated, dateModified) VALUES
(90011, 90011, 1, 3, 1, 3, 'text', 'utf8', 'byte',
  ' # Parity\n-old line\n+new line\n context\n', 1794602000, 1794602000),
(90012, 90012, 0, 0, 1, 3, 'text', 'utf8', 'byte',
  '+# Guide\n+\n+Read the README first.\n', 1794602000, 1794602000),
(90021, 90021, 1, 1, 1, 1, 'text', 'utf8', 'byte',
  '-todo\n+done\n', 1794602000, 1794602000),
(90031, 90031, 1, 3, 1, 4, 'text', 'utf8', 'byte',
  ' # Parity\n-old line\n+bob line\n+another\n context\n', 1794602000,
  1794602000);

INSERT IGNORE INTO phabricator_differential.differential_diffproperty (id,
  diffID, name, data, dateCreated, dateModified) VALUES
(90011, 9001, 'local:commits',
  '{"9f1c2e3d4b5a69788796a5b4c3d2e1f00f1e2d3c":{"author":"alice","authorEmail":"alice@example.com","time":"1794601000","message":"Bug 1002 - Add the Git README","commit":"9f1c2e3d4b5a69788796a5b4c3d2e1f00f1e2d3c","tree":"a1b2c3d4e5f60718293a4b5c6d7e8f9001122334","parents":["0011223344556677889900aabbccddeeff001122"],"summary":"Bug 1002 - Add the Git README"}}',
  1794602000, 1794602000);

INSERT IGNORE INTO phabricator_differential.differential_viewstate
  (viewerPHID, objectPHID, viewState, dateCreated, dateModified)
SELECT phid, 'PHID-DIFF-parityupdate0001', '{"engine":"default"}',
  1794602000, 1794602000
  FROM phabricator_user.user WHERE userName = 'alice';
