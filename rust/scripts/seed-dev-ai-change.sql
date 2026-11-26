-- AI changes (`mozilla.ai-change`, Bug 2091563) for the parity scenarios
-- of parity/ai-change.json and parity/writes/ai-change.json: the dev stack
-- has none. scripts/seed-dev-stack.sh loads it; it can be loaded again.
--
-- D2 gets an older diff, 960, to request a comparison against; D3 has a
-- pending merge conflict resolution requested by bob, D4 one which lapsed
-- (its change is diff 2, not D4's), D5 an approved edit and D6 (abandoned)
-- a pending one.
INSERT IGNORE INTO phabricator_differential.differential_diff (id, phid,
  revisionID, authorPHID, repositoryPHID, sourceMachine, sourcePath,
  sourceControlSystem, sourceControlBaseRevision, sourceControlPath,
  lintStatus, unitStatus, lineCount, branch, bookmark, creationMethod,
  dateCreated, dateModified, description, repositoryUUID, viewPolicy,
  commitPHID)
SELECT 960, 'PHID-DIFF-parityaibase0960', revisionID, authorPHID,
  repositoryPHID, sourceMachine, sourcePath, sourceControlSystem,
  sourceControlBaseRevision, sourceControlPath, lintStatus, unitStatus,
  lineCount, branch, bookmark, creationMethod, dateCreated - 60,
  dateModified - 60, description, repositoryUUID, viewPolicy, commitPHID
  FROM phabricator_differential.differential_diff WHERE id = 2;

UPDATE phabricator_differential.differential_revision r
  JOIN phabricator_user.user u ON u.userName = 'bob'
  SET r.properties = JSON_SET(
    IF(JSON_TYPE(r.properties) = 'OBJECT', r.properties, JSON_OBJECT()),
    '$."mozilla.ai-change"', JSON_OBJECT(
      'kind', 'merge-conflict',
      'changeDiffID', 3,
      'changeDiffPHID', r.activeDiffPHID,
      'baseDiffID', NULL,
      'summary', 'Kept the beta API.',
      'confidence', 'high',
      'details', 'Moved the overflow handling into **updateOverflow**.',
      'requesterPHID', u.phid,
      'requestEpoch', 1794600000,
      'approverPHID', NULL,
      'approvalEpoch', NULL))
  WHERE r.id = 3;

UPDATE phabricator_differential.differential_revision r
  JOIN phabricator_user.user u ON u.userName = 'carol'
  SET r.properties = JSON_SET(
    IF(JSON_TYPE(r.properties) = 'OBJECT', r.properties, JSON_OBJECT()),
    '$."mozilla.ai-change"', JSON_OBJECT(
      'kind', 'merge-conflict',
      'changeDiffID', 2,
      'changeDiffPHID', 'PHID-DIFF-jwtb7gq2etgin2prmz4u',
      'baseDiffID', NULL,
      'summary', NULL,
      'confidence', NULL,
      'details', NULL,
      'requesterPHID', u.phid,
      'requestEpoch', 1794600000,
      'approverPHID', NULL,
      'approvalEpoch', NULL))
  WHERE r.id = 4;

UPDATE phabricator_differential.differential_revision r
  JOIN phabricator_user.user u ON u.userName = 'carol'
  JOIN phabricator_user.user a ON a.userName = 'alice'
  SET r.properties = JSON_SET(
    IF(JSON_TYPE(r.properties) = 'OBJECT', r.properties, JSON_OBJECT()),
    '$."mozilla.ai-change"', JSON_OBJECT(
      'kind', 'requested-edit',
      'changeDiffID', 5,
      'changeDiffPHID', r.activeDiffPHID,
      'baseDiffID', NULL,
      'summary', 'Renamed the helper.',
      'confidence', 'medium',
      'details', NULL,
      'requesterPHID', u.phid,
      'requestEpoch', 1794600000,
      'approverPHID', a.phid,
      'approvalEpoch', 1794600060))
  WHERE r.id = 5;

UPDATE phabricator_differential.differential_revision r
  JOIN phabricator_user.user u ON u.userName = 'dave'
  SET r.properties = JSON_SET(
    IF(JSON_TYPE(r.properties) = 'OBJECT', r.properties, JSON_OBJECT()),
    '$."mozilla.ai-change"', JSON_OBJECT(
      'kind', 'merge-conflict',
      'changeDiffID', 6,
      'changeDiffPHID', r.activeDiffPHID,
      'baseDiffID', NULL,
      'summary', NULL,
      'confidence', 'low',
      'details', NULL,
      'requesterPHID', u.phid,
      'requestEpoch', 1794600000,
      'approverPHID', NULL,
      'approvalEpoch', NULL))
  WHERE r.id = 6;
