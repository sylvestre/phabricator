-- A Git repository without a callsign (R2) and its commits, for the parity
-- scenarios of Git-only code paths: the demo's repository (rVCT) is
-- Mercurial. scripts/seed-dev-stack.sh loads it.
INSERT IGNORE INTO phabricator_repository.repository (id, phid, name, callsign,
  versionControlSystem, details, dateCreated, dateModified, uuid, viewPolicy,
  editPolicy, pushPolicy, localPath) VALUES
(2, 'PHID-REPO-paritygit000000', 'Parity Git', NULL, 'git',
  '{"remote-uri":"https://github.com/mozilla/parity.git","staging-uri":"https://stage.example.com/parity.git","tracking-enabled":"inactive","description":"A git mirror","encoding":"ISO-8859-1"}',
  1525784108, 1525973175, 'uuid-parity-git', 'users', 'admin', 'users',
  '/var/repo/2/');

-- Alice's commit and one without an author, both published (1035:
-- message, change, publish and on a permanent ref), and one not on a
-- permanent ref yet (11).
INSERT IGNORE INTO phabricator_repository.repository_commit (id, repositoryID,
  phid, commitIdentifier, epoch, authorPHID, auditStatus, summary,
  importStatus) VALUES
(101, 2, 'PHID-CMIT-paritygit0000001', '9f1c2e3d4b5a69788796a5b4c3d2e1f00f1e2d3c',
  1794601000, (SELECT phid FROM phabricator_user.user WHERE userName = 'alice'),
  'none', 'Bug 1002 - Add the Git README', 1035),
(102, 2, 'PHID-CMIT-paritygit0000002', 'a0b1c2d3e4f5061728394a5b6c7d8e9f00112233',
  1794601100, NULL, 'none', 'Update the Git README', 1035),
(103, 2, 'PHID-CMIT-paritygit0000003', 'b1c2d3e4f5a6071829304a5b6c7d8e9f00112244',
  1794601200, NULL, 'none', 'Draft the Git README', 11);

INSERT IGNORE INTO phabricator_repository.repository_commitdata (commitID,
  authorName, commitMessage, commitDetails) VALUES
(101, 'alice <alice@example.com>', 'Bug 1002 - Add the Git README', '{}'),
(102, 'nobody <nobody@example.com>', 'Update the Git README', '{}'),
(103, 'nobody <nobody@example.com>', 'Draft the Git README', '{}');
