-- Herald rules reading the revision fields the Rust editor evaluates, for
-- the parity scenarios. Rule 3 matches any of conditions which are false
-- on the dev revisions, so every edit's transcript holds every field's
-- value; rule 4 tests "always"; rule 5 fires on accepted revisions.
-- scripts/seed-dev-stack.sh loads it.
INSERT IGNORE INTO phabricator_herald.herald_rule (id, name, authorPHID,
  contentType, mustMatchAll, configVersion, dateCreated, dateModified,
  repetitionPolicy, ruleType, phid, isDisabled, triggerObjectPHID)
SELECT 3, 'Parity: revision fields', phid, 'differential', 0, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-parityfields0003', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin'
UNION ALL
SELECT 4, 'Parity: always', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-parityalways0004', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin'
UNION ALL
SELECT 5, 'Parity: accepted revisions', phid, 'differential', 1, 38,
  1794600000, 1794600000, 'every', 'global', 'PHID-HRUL-parityaccept0005', 0,
  NULL FROM phabricator_user.user WHERE userName = 'admin';

INSERT IGNORE INTO phabricator_herald.herald_condition (id, ruleID, fieldName,
  fieldCondition, value) VALUES
(101, 3, 'differential.revision.title', 'contains', '"zzherald"'),
(102, 3, 'differential.revision.summary', 'contains', '"zzherald"'),
(103, 3, 'differential.revision.test-plan', 'contains', '"zzherald"'),
(104, 3, 'differential.revision.author', 'isany',
  (SELECT CONCAT('["', phid, '"]') FROM phabricator_user.user WHERE userName = 'admin')),
(105, 3, 'differential.revision.author.projects', 'any',
  '["PHID-PROJ-3kt2nmhl44vlb2qkxazd"]'),
(106, 3, 'differential.revision.reviewers', 'all',
  (SELECT CONCAT('["', a.phid, '","', c.phid, '"]') FROM phabricator_user.user a,
    phabricator_user.user c WHERE a.userName = 'admin' AND c.userName = 'carol')),
(107, 3, 'differential.revision.repository', 'isany',
  (SELECT CONCAT('["', phid, '"]') FROM phabricator_repository.repository WHERE id = 1)),
(108, 3, 'differential.revision.repository.projects', 'any',
  '["PHID-PROJ-fw4ppopw3mj73juscyek"]'),
(109, 3, 'revision.status', 'isany', '["published"]'),
(110, 3, 'cc', 'any',
  (SELECT CONCAT('["', phid, '"]') FROM phabricator_user.user WHERE userName = 'phab-bot')),
(111, 3, 'contentsource', 'is', '"web"'),
(112, 3, 'herald.acting-user', 'isany',
  (SELECT CONCAT('["', phid, '"]') FROM phabricator_user.user WHERE userName = 'dave')),
(114, 4, 'always', 'unconditionally', 'null'),
(115, 4, 'differential.revision.title', 'is', '"zzherald exactly"'),
(116, 5, 'revision.status', 'isany', '["accepted"]'),
(117, 5, 'differential.revision.title', '!contains', '"zzherald"');

INSERT IGNORE INTO phabricator_herald.herald_action (id, ruleID, action,
  target) VALUES
(101, 3, 'email.must-encrypt', 'null'),
(102, 4, 'email.must-encrypt', 'null'),
(103, 5, 'email.must-encrypt', 'null');

-- Regular expressions: rule 6 never fires but evaluates both patterns;
-- rule 7 fires on revisions with an empty test plan.
INSERT IGNORE INTO phabricator_herald.herald_rule (id, name, authorPHID,
  contentType, mustMatchAll, configVersion, dateCreated, dateModified,
  repetitionPolicy, ruleType, phid, isDisabled, triggerObjectPHID)
SELECT 6, 'Parity: patterns', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-paritypatterns06', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin'
UNION ALL
SELECT 7, 'Parity: empty test plans', phid, 'differential', 0, 38,
  1794600000, 1794600000, 'every', 'global', 'PHID-HRUL-parityemptyplan7', 0,
  NULL FROM phabricator_user.user WHERE userName = 'admin';

INSERT IGNORE INTO phabricator_herald.herald_condition (id, ruleID, fieldName,
  fieldCondition, value) VALUES
(118, 6, 'differential.revision.title', 'regexp', '"@^Bug \\\\d+ - @"'),
(119, 6, 'differential.revision.summary', 'regexp', '"/zzherald/i"'),
(120, 7, 'differential.revision.test-plan', '!regexp', '"/./"');

INSERT IGNORE INTO phabricator_herald.herald_action (id, ruleID, action,
  target) VALUES
(104, 6, 'email.must-encrypt', 'null'),
(105, 7, 'email.must-encrypt', 'null');

-- Diff fields: rule 8 fires on revisions touching dom/base (D1, D2) and
-- reads every content field of the others.
INSERT IGNORE INTO phabricator_herald.herald_rule (id, name, authorPHID,
  contentType, mustMatchAll, configVersion, dateCreated, dateModified,
  repetitionPolicy, ruleType, phid, isDisabled, triggerObjectPHID)
SELECT 8, 'Parity: diffs', phid, 'differential', 0, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-paritydiffs00008', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin';

INSERT IGNORE INTO phabricator_herald.herald_condition (id, ruleID, fieldName,
  fieldCondition, value) VALUES
(121, 8, 'differential.revision.diff.content', 'contains', '"zzherald"'),
(122, 8, 'differential.revision.diff.new', 'regexp', '"/zzherald/"'),
(123, 8, 'differential.revision.diff.old', 'contains', '"zzherald"'),
(124, 8, 'differential.revision.diff.affected', 'regexp', '"@^/dom/base/@"'),
(125, 8, 'differential.revision.diff.content', 'regexp-pair',
  '"[\\"@^/dom/@\\",\\"/zzherald/\\"]"');

INSERT IGNORE INTO phabricator_herald.herald_action (id, ruleID, action,
  target) VALUES
(106, 8, 'email.must-encrypt', 'null');

-- Actions: rule 9 subscribes dave and rule 10 unsubscribes bob when a
-- title says so (parity/writes/herald-actions.json).
INSERT IGNORE INTO phabricator_herald.herald_rule (id, name, authorPHID,
  contentType, mustMatchAll, configVersion, dateCreated, dateModified,
  repetitionPolicy, ruleType, phid, isDisabled, triggerObjectPHID)
SELECT 9, 'Act: subscribe dave', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-actsubscribe0009', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin'
UNION ALL
SELECT 10, 'Act: unsubscribe bob', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-actunsubscrib010', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin';

INSERT IGNORE INTO phabricator_herald.herald_condition (id, ruleID, fieldName,
  fieldCondition, value) VALUES
(201, 9, 'differential.revision.title', 'contains', '"act-subscribe"'),
(202, 10, 'differential.revision.title', 'contains', '"act-unsubscribe"');

INSERT IGNORE INTO phabricator_herald.herald_action (id, ruleID, action,
  target) VALUES
(201, 9, 'subscribers.add',
  (SELECT CONCAT('["', phid, '"]') FROM phabricator_user.user WHERE userName = 'dave')),
(202, 10, 'subscribers.remove',
  (SELECT CONCAT('["', phid, '"]') FROM phabricator_user.user WHERE userName = 'bob'));

-- Rule 11 adds dave as reviewer, rule 12 as blocking reviewer.
INSERT IGNORE INTO phabricator_herald.herald_rule (id, name, authorPHID,
  contentType, mustMatchAll, configVersion, dateCreated, dateModified,
  repetitionPolicy, ruleType, phid, isDisabled, triggerObjectPHID)
SELECT 11, 'Act: review dave', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-actreviewer00011', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin'
UNION ALL
SELECT 12, 'Act: block dave', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-actblocking00012', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin';

INSERT IGNORE INTO phabricator_herald.herald_condition (id, ruleID, fieldName,
  fieldCondition, value) VALUES
(203, 11, 'differential.revision.title', 'contains', '"act-review"'),
(204, 12, 'differential.revision.title', 'contains', '"act-block"');

INSERT IGNORE INTO phabricator_herald.herald_action (id, ruleID, action,
  target) VALUES
(203, 11, 'differential.reviewers.add',
  (SELECT CONCAT('["', phid, '"]') FROM phabricator_user.user WHERE userName = 'dave')),
(204, 12, 'differential.reviewers.blocking',
  (SELECT CONCAT('["', phid, '"]') FROM phabricator_user.user WHERE userName = 'dave'));

-- Rule 13 tags with Build Reviewers, 14 mails dave, 15 does nothing, 17
-- removes Frontend Reviewers.
INSERT IGNORE INTO phabricator_herald.herald_rule (id, name, authorPHID,
  contentType, mustMatchAll, configVersion, dateCreated, dateModified,
  repetitionPolicy, ruleType, phid, isDisabled, triggerObjectPHID)
SELECT 13, 'Act: tag', phid, 'differential', 1, 38, 1794600000, 1794600000,
  'every', 'global', 'PHID-HRUL-acttag0000000013', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin'
UNION ALL
SELECT 14, 'Act: email dave', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-actemail0000014', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin'
UNION ALL
SELECT 15, 'Act: nothing', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-actnothing00015', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin'
UNION ALL
SELECT 17, 'Act: untag', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-actuntag0000017', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin';

INSERT IGNORE INTO phabricator_herald.herald_condition (id, ruleID, fieldName,
  fieldCondition, value) VALUES
(205, 13, 'differential.revision.title', 'contains', '"act-tag"'),
(206, 14, 'differential.revision.title', 'contains', '"act-email"'),
(207, 15, 'differential.revision.title', 'contains', '"act-nothing"'),
(209, 17, 'differential.revision.title', 'contains', '"act-untag"');

INSERT IGNORE INTO phabricator_herald.herald_action (id, ruleID, action,
  target) VALUES
(205, 13, 'projects.add', '["PHID-PROJ-fw4ppopw3mj73juscyek"]'),
(206, 14, 'email.other',
  (SELECT CONCAT('["', phid, '"]') FROM phabricator_user.user WHERE userName = 'dave')),
(207, 15, 'nothing', 'null'),
(209, 17, 'projects.remove', '["PHID-PROJ-sv7lxylfx3dmjxbkjkt4"]');

-- Personal rules (H5): dave subscribes himself, alice reviews, carol gets
-- mail, on titles saying act-mine; act-wrong fires a global rule with a
-- self action and a personal rule with a tag, both of the wrong rule type.
INSERT IGNORE INTO phabricator_herald.herald_rule (id, name, authorPHID,
  contentType, mustMatchAll, configVersion, dateCreated, dateModified,
  repetitionPolicy, ruleType, phid, isDisabled, triggerObjectPHID)
SELECT 18, 'Mine: dave subscribes', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'personal', 'PHID-HRUL-minedave0000018', 0, NULL
  FROM phabricator_user.user WHERE userName = 'dave'
UNION ALL
SELECT 19, 'Mine: alice reviews', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'personal', 'PHID-HRUL-minealice000019', 0, NULL
  FROM phabricator_user.user WHERE userName = 'alice'
UNION ALL
SELECT 20, 'Mine: carol gets mail', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'personal', 'PHID-HRUL-minecarol000020', 0, NULL
  FROM phabricator_user.user WHERE userName = 'carol'
UNION ALL
SELECT 21, 'Wrong: global self subscribe', phid, 'differential', 1, 38,
  1794600000, 1794600000, 'every', 'global', 'PHID-HRUL-wrongglobal00021', 0,
  NULL FROM phabricator_user.user WHERE userName = 'admin'
UNION ALL
SELECT 22, 'Wrong: personal tag', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'personal', 'PHID-HRUL-wrongpersonal022', 0, NULL
  FROM phabricator_user.user WHERE userName = 'bob';

INSERT IGNORE INTO phabricator_herald.herald_condition (id, ruleID, fieldName,
  fieldCondition, value) VALUES
(218, 18, 'differential.revision.title', 'contains', '"act-mine"'),
(219, 19, 'differential.revision.title', 'contains', '"act-mine"'),
(220, 20, 'differential.revision.title', 'contains', '"act-mine"'),
(221, 21, 'differential.revision.title', 'contains', '"act-wrong"'),
(222, 22, 'differential.revision.title', 'contains', '"act-wrong"');

INSERT IGNORE INTO phabricator_herald.herald_action (id, ruleID, action,
  target) VALUES
(218, 18, 'subscribers.self.add', 'null'),
(219, 19, 'differential.reviewers.self.add', 'null'),
(220, 20, 'email.self', 'null'),
(221, 21, 'subscribers.self.add', 'null'),
(222, 22, 'projects.add', '["PHID-PROJ-fw4ppopw3mj73juscyek"]');

-- Repetition policies: rule 23 subscribes dave the first time only, rule 24
-- mails dave only when it did not match the last time. Both applied to D7
-- before.
INSERT IGNORE INTO phabricator_herald.herald_rule (id, name, authorPHID,
  contentType, mustMatchAll, configVersion, dateCreated, dateModified,
  repetitionPolicy, ruleType, phid, isDisabled, triggerObjectPHID)
SELECT 23, 'Once: subscribe dave', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'first', 'global', 'PHID-HRUL-oncefirst000023', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin'
UNION ALL
SELECT 24, 'Change: mail dave', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'change', 'global', 'PHID-HRUL-oncechange00024', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin';

INSERT IGNORE INTO phabricator_herald.herald_condition (id, ruleID, fieldName,
  fieldCondition, value) VALUES
(223, 23, 'differential.revision.title', 'contains', '"act-once"'),
(224, 24, 'differential.revision.title', 'contains', '"act-change"');

INSERT IGNORE INTO phabricator_herald.herald_action (id, ruleID, action,
  target) VALUES
(223, 23, 'subscribers.add',
  (SELECT CONCAT('["', phid, '"]') FROM phabricator_user.user WHERE userName = 'dave')),
(224, 24, 'email.other',
  (SELECT CONCAT('["', phid, '"]') FROM phabricator_user.user WHERE userName = 'dave'));

INSERT IGNORE INTO phabricator_herald.herald_ruleapplied (phid, ruleID)
SELECT phid, 23 FROM phabricator_differential.differential_revision WHERE id = 7
UNION ALL
SELECT phid, 24 FROM phabricator_differential.differential_revision WHERE id = 7;

-- Several targets, users and projects mixed: PHP groups them by type in
-- the order types come, each in its query's order (users newest first,
-- projects by name).
INSERT IGNORE INTO phabricator_herald.herald_rule (id, name, authorPHID,
  contentType, mustMatchAll, configVersion, dateCreated, dateModified,
  repetitionPolicy, ruleType, phid, isDisabled, triggerObjectPHID)
SELECT 25, 'Many: subscribe', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-manysubscribe025', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin'
UNION ALL
SELECT 26, 'Many: review', phid, 'differential', 1, 38, 1794600000,
  1794600000, 'every', 'global', 'PHID-HRUL-manyreview000026', 0, NULL
  FROM phabricator_user.user WHERE userName = 'admin';

INSERT IGNORE INTO phabricator_herald.herald_condition (id, ruleID, fieldName,
  fieldCondition, value) VALUES
(225, 25, 'differential.revision.title', 'contains', '"act-many"'),
(226, 26, 'differential.revision.title', 'contains', '"act-many"');

INSERT IGNORE INTO phabricator_herald.herald_action (id, ruleID, action,
  target) VALUES
(225, 25, 'subscribers.add',
  (SELECT CONCAT('["', c.phid, '","PHID-PROJ-fw4ppopw3mj73juscyek","', d.phid, '","', a.phid, '"]')
   FROM phabricator_user.user c, phabricator_user.user d, phabricator_user.user a
   WHERE c.userName = 'carol' AND d.userName = 'dave' AND a.userName = 'admin')),
(226, 26, 'differential.reviewers.add',
  (SELECT CONCAT('["PHID-PROJ-sv7lxylfx3dmjxbkjkt4","', d.phid, '","', b.phid, '"]')
   FROM phabricator_user.user d, phabricator_user.user b
   WHERE d.userName = 'dave' AND b.userName = 'bob'));
