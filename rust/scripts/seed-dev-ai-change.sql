-- AI changes (`mozilla.ai-change`, Bug 2091563) for the parity scenarios
-- of parity/ai-change.json and parity/writes/ai-change.json: the dev stack
-- has none. scripts/seed-dev-stack.sh loads it; it can be loaded again.
--
-- D52 gets an older diff, 940, to request a comparison against (its ID
-- is below the active diff's: PHP takes the highest ID as active); D3 has a
-- pending merge conflict resolution requested by bob, D4 one which lapsed
-- (its change is diff 2, not D4's), D5 an approved edit and D6 (abandoned)
-- a pending one. PHIDs are binary columns: they are converted, or JSON_SET
-- would store them as "base64:type15:...".
-- Earlier versions of this file put the older diff, 960, on D2.
DELETE FROM phabricator_differential.differential_diff
  WHERE id = 960 AND phid = 'PHID-DIFF-parityaibase0960';
INSERT IGNORE INTO phabricator_differential.differential_diff (id, phid,
  revisionID, authorPHID, repositoryPHID, sourceMachine, sourcePath,
  sourceControlSystem, sourceControlBaseRevision, sourceControlPath,
  lintStatus, unitStatus, lineCount, branch, bookmark, creationMethod,
  dateCreated, dateModified, description, repositoryUUID, viewPolicy,
  commitPHID)
SELECT 940, 'PHID-DIFF-parityaibase0940', revisionID, authorPHID,
  repositoryPHID, sourceMachine, sourcePath, sourceControlSystem,
  sourceControlBaseRevision, sourceControlPath, lintStatus, unitStatus,
  lineCount, branch, bookmark, creationMethod, dateCreated - 60,
  dateModified - 60, description, repositoryUUID, viewPolicy, commitPHID
  FROM phabricator_differential.differential_diff WHERE id = 952;

UPDATE phabricator_differential.differential_revision r
  JOIN phabricator_user.user u ON u.userName = 'bob'
  SET r.properties = JSON_SET(
    IF(JSON_TYPE(r.properties) = 'OBJECT', r.properties, JSON_OBJECT()),
    '$."mozilla.ai-change"', JSON_OBJECT(
      'kind', 'merge-conflict',
      'changeDiffID', 3,
      'changeDiffPHID', CONVERT(r.activeDiffPHID USING utf8mb4),
      'baseDiffID', NULL,
      'summary', 'Kept the beta API.',
      'confidence', 'high',
      'details', 'Moved the overflow handling into **updateOverflow**.',
      'requesterPHID', CONVERT(u.phid USING utf8mb4),
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
      'requesterPHID', CONVERT(u.phid USING utf8mb4),
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
      'changeDiffPHID', CONVERT(r.activeDiffPHID USING utf8mb4),
      'baseDiffID', NULL,
      'summary', 'Renamed the helper.',
      'confidence', 'medium',
      'details', NULL,
      'requesterPHID', CONVERT(u.phid USING utf8mb4),
      'requestEpoch', 1794600000,
      'approverPHID', CONVERT(a.phid USING utf8mb4),
      'approvalEpoch', 1794600060))
  WHERE r.id = 5;

UPDATE phabricator_differential.differential_revision r
  JOIN phabricator_user.user u ON u.userName = 'dave'
  SET r.properties = JSON_SET(
    IF(JSON_TYPE(r.properties) = 'OBJECT', r.properties, JSON_OBJECT()),
    '$."mozilla.ai-change"', JSON_OBJECT(
      'kind', 'merge-conflict',
      'changeDiffID', 6,
      'changeDiffPHID', CONVERT(r.activeDiffPHID USING utf8mb4),
      'baseDiffID', NULL,
      'summary', NULL,
      'confidence', 'low',
      'details', NULL,
      'requesterPHID', CONVERT(u.phid USING utf8mb4),
      'requestEpoch', 1794600000,
      'approverPHID', NULL,
      'approvalEpoch', NULL))
  WHERE r.id = 6;
