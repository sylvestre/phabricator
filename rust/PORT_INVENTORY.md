# Port inventory

What PHP serves on Mozilla's install and where each piece stands in the Rust port, to plan the rest of the migration. Generated on 2026-11-23 from the code: the Conduit methods are every `getAPIMethodName()` of `src/` and `moz-extensions/src/`; the web routes are the `getRoutes()` and `getResourceRoutes()` of every installed application, flattened (as `AphrontRoutingMap` does), with the controller each one reaches. Rust's side comes from `rust/nginx/rust-routes.conf`, `rust/nginx/rust-web.conf`, `PHAB_SHADOW_METHODS` in `rust/docker-compose.rust.yml`, the methods the server registers and the `phab-web` router.

Installed applications: every application in `src/applications/` and `moz-extensions/src/` (no `phabricator.uninstalled-applications` in `docker/tools/local.json`, `conf/` or the dev stack's config), minus Almanac, Calendar and Passphrase, which the port drops (their 15 methods and 50 routes are not listed). Phame, Ponder, Phriction, Maniphest, Owners/Packages, Paste, Phurl, Badges, Tokens, Countdown, Slowvote, Fund, Nuance and Workboards are not in this tree at all.

## Legend

| Status | Meaning |
|---|---|
| **Rust** | nginx sends it to Rust, which answers it (Conduit: not in `PHAB_SHADOW_METHODS`; web: for users with the `phab_ui=rust` cookie) |
| **shadowed** | Conduit only: routed to Rust and in `PHAB_SHADOW_METHODS`, so PHP answers and Rust compares |
| **partial** | routed to Rust, which answers some variants (queries, objects, VCS) and hands the rest to PHP with 421 |
| **ported, not routed** | Rust implements it, nginx doesn't send it there yet |
| **not ported** | PHP only; the note says what blocks it or what it is |

## Totals

| | Rust | shadowed | partial | ported, not routed | not ported | total |
|---|---|---|---|---|---|---|
| Conduit methods | 17 | 70 | – | 18 | 15 | 120 |
| Web routes | 44 | – | 54 | 7 | 273 | 378 |

Of the 15 methods not ported, 14 write; the read-only one left is `diffusion.findsymbols`.

Web routes count each PHP route once; a Diffusion route stands for its three spellings (`/source/<name>/`, `/diffusion/<CALLSIGN>/`, `/diffusion/<id>/`), written `/{repo}`. Routes PHP registers twice (Celerity, `/status/`) count once.

## Conduit methods

### Audit

| Method | PHP status | Port | Note |
|---|---|---|---|
| `audit.query` | frozen | shadowed |  |

### Auth

| Method | PHP status | Port | Note |
|---|---|---|---|
| `auth.logout` | stable | ported, not routed | write (terminates sessions); registered only with `PHAB_RUST_LOGIN=1`. |
| `auth.querypublickeys` | stable | ported, not routed | read-only; keys of users (whom anyone sees). Left to PHP: pages with keys of other objects (Almanac devices), empty lists. Registered without `PHAB_RUST_LOGIN` (`register_queries()`). |

### Conduit

| Method | PHP status | Port | Note |
|---|---|---|---|
| `conduit.connect` | stable | not ported | legacy certificate handshake: writes a Conduit session. |
| `conduit.getcapabilities` | stable | shadowed |  |
| `conduit.getcertificate` | stable | not ported | legacy `arc install-certificate` token exchange (writes). |
| `conduit.ping` | stable | Rust |  |
| `conduit.query` | stable | ported, not routed | from the method catalog (`crates/phab-web/catalog/conduit.json`, regenerate it when PHP's methods change), filtered by installed applications the viewer can see. |

### Dashboards

| Method | PHP status | Port | Note |
|---|---|---|---|
| `dashboard.panel.edit` | stable | ported, not routed | write: `name` of existing panels. Left to PHP: creations, policies, the fields of panel types (`text`, query, tabs), edits without effect. |
| `portal.edit` | stable | ported, not routed | write: `name` of existing portals. Left to PHP: creations, policies, projects, edits without effect. |
| `portal.search` | stable | shadowed |  |

### Differential

| Method | PHP status | Port | Note |
|---|---|---|---|
| `differential.changeset.search` | stable | shadowed |  |
| `differential.close` | frozen | not ported | write (frozen; `differential.revision.edit` replaces it). |
| `differential.createcomment` | frozen | not ported | write (frozen; `differential.revision.edit` replaces it). |
| `differential.creatediff` | stable | Rust |  |
| `differential.createinline` | stable | not ported | write: draft inline comments (Lando, moz-phab). |
| `differential.createrawdiff` | stable | ported, not routed | write; waits for a write-parity run. |
| `differential.createrevision` | frozen | not ported | write (frozen; `differential.revision.edit` replaces it). |
| `differential.diff.search` | stable | shadowed |  |
| `differential.getalldiffs` | deprecated | shadowed |  |
| `differential.getcommitmessage` | stable | shadowed |  |
| `differential.getcommitpaths` | stable | shadowed |  |
| `differential.getdiff` | deprecated | shadowed |  |
| `differential.getrawdiff` | stable | shadowed |  |
| `differential.getrevision` | deprecated | shadowed |  |
| `differential.getrevisioncomments` | deprecated | shadowed |  |
| `differential.parsecommitmessage` | stable | shadowed |  |
| `differential.query` | frozen | shadowed |  |
| `differential.querydiffs` | frozen | shadowed |  |
| `differential.revision.edit` | stable | ported, not routed | write; ported for the actions `parity/writes/` covers, waits for the user's go-ahead (moz-phab, Lando). |
| `differential.revision.search` | stable | shadowed |  |
| `differential.setdiffproperty` | stable | Rust |  |
| `differential.updaterevision` | frozen | not ported | write (frozen; `differential.revision.edit` replaces it). |

### Diffusion

| Method | PHP status | Port | Note |
|---|---|---|---|
| `diffusion.blame` | stable | shadowed |  |
| `diffusion.branchquery` | stable | shadowed |  |
| `diffusion.browsequery` | stable | shadowed |  |
| `diffusion.commit.edit` | stable | not ported | write (EditEngine): auditors, audit actions, comments, projects and subscribers through `PhabricatorAuditEditor`, which sends mail, publishes feed stories and runs Herald on commits: not ported. |
| `diffusion.commit.search` | stable | shadowed |  |
| `diffusion.commitparentsquery` | stable | shadowed |  |
| `diffusion.diffquery` | stable | ported, not routed | read-only for Git (the diff is read inline, not stored); with `ArcanistDiffParser`'s dictionaries. Left to PHP: diffs which are not UTF-8 or the Rust parser leaves to PHP. |
| `diffusion.existsquery` | stable | shadowed |  |
| `diffusion.filecontentquery` | stable | ported, not routed | write: stores the content as a temporary file attached to the repository (Git). Left to PHP: Git failures, timeouts (`tooSlow`), chunked output, the files `file.upload` leaves to PHP, shadow calls (they would write). |
| `diffusion.findsymbols` | stable | not ported | read-only; `DiffusionSymbolQuery` over `repository_symbol`, which Mozilla doesn't fill (no symbol import). Easy to port. |
| `diffusion.getlintmessages` | unstable | shadowed |  |
| `diffusion.getrecentcommitsbypath` | deprecated | shadowed |  |
| `diffusion.historyquery` | stable | shadowed |  |
| `diffusion.internal.ancestors` | stable | ported, not routed | new: read-only, Git. |
| `diffusion.internal.gitrawdiffquery` | stable | shadowed |  |
| `diffusion.lastmodifiedquery` | stable | shadowed | with `PhabricatorRepositoryGraphCache` (read from `cache_general` or rebuilt, never written). |
| `diffusion.looksoon` | unstable | ported, not routed | write: a `needs-update` status message per visible repository. Left to PHP: identifiers which are not lists of strings, shadow calls. |
| `diffusion.mergedcommitsquery` | stable | shadowed |  |
| `diffusion.querycommits` | frozen | shadowed |  |
| `diffusion.querypaths` | stable | shadowed |  |
| `diffusion.rawdiffquery` | stable | ported, not routed | write: stores the diff as a temporary file attached to the repository (Git). Left to PHP: as `diffusion.filecontentquery`. |
| `diffusion.refsquery` | stable | shadowed |  |
| `diffusion.repository.edit` | stable | ported, not routed | write: `name`, `callsign`, `shortName`, `description` (without mentions, references or files), `status` and `publish` of existing repositories; clears the `needs-update` message. Left to PHP: creations, the other types (policies, projects, refs, limits, symbols, encoding...), cluster repositories, empty names, edits without effect. |
| `diffusion.repository.search` | stable | shadowed |  |
| `diffusion.resolverefs` | stable | shadowed |  |
| `diffusion.searchquery` | stable | shadowed |  |
| `diffusion.tagsquery` | stable | shadowed |  |
| `diffusion.updatecoverage` | unstable | not ported | write: stores coverage. |
| `diffusion.uri.edit` | stable | not ported | write (EditEngine). Every edit ends with `applyFinalEffects()`, which saves the repository from its Observe URI and rebuilds `repository_uriindex` from the effective and normalized URIs of all its URIs (builtin ones from `diffusion.ssh-*` and the base URI): not ported. |
| `internal.commit.search` | stable | shadowed |  |

### Feed

| Method | PHP status | Port | Note |
|---|---|---|---|
| `feed.query` | unstable | ported, not routed | new: read-only, the `data` view (rendered views go to PHP). |

### Files

| Method | PHP status | Port | Note |
|---|---|---|---|
| `file.allocate` | stable | Rust |  |
| `file.download` | stable | Rust |  |
| `file.info` | frozen | shadowed |  |
| `file.querychunks` | stable | Rust |  |
| `file.search` | stable | shadowed |  |
| `file.upload` | stable | Rust |  |
| `file.uploadchunk` | stable | Rust |  |
| `file.uploadhash` | deprecated | Rust |  |

### Flags

| Method | PHP status | Port | Note |
|---|---|---|---|
| `flag.delete` | stable | Rust |  |
| `flag.edit` | stable | Rust |  |
| `flag.query` | stable | shadowed |  |

### Harbormaster

| Method | PHP status | Port | Note |
|---|---|---|---|
| `harbormaster.artifact.search` | stable | shadowed |  |
| `harbormaster.build.edit` | stable | not ported | write (EditEngine). |
| `harbormaster.build.search` | stable | shadowed |  |
| `harbormaster.buildable.edit` | stable | not ported | write (EditEngine). |
| `harbormaster.buildable.search` | stable | shadowed |  |
| `harbormaster.buildplan.edit` | stable | ported, not routed | write: `name` and `behavior.*` of existing plans the actor can edit (no mail, feed or Herald). Left to PHP: creations, policies, projects, subscribers, empty names, edits without effect. |
| `harbormaster.buildplan.search` | stable | shadowed |  |
| `harbormaster.createartifact` | stable | ported, not routed | write; waits for a CI or Lando contract run. |
| `harbormaster.log.search` | stable | shadowed |  |
| `harbormaster.queryautotargets` | stable | ported, not routed | write (creates the autotargets it returns); waits for a CI contract run. |
| `harbormaster.querybuildables` | stable | shadowed |  |
| `harbormaster.querybuilds` | deprecated | shadowed |  |
| `harbormaster.sendmessage` | stable | ported, not routed | write; waits for a Lando or CI contract run. |
| `harbormaster.step.edit` | stable | not ported | write (EditEngine). The engine has no fields of its own: its types are the custom fields of the step implementations (`core:customfield` into `details`, credentials), not ported. |
| `harbormaster.step.search` | stable | shadowed |  |
| `harbormaster.target.search` | stable | shadowed |  |

### moz-extensions: conduit

| Method | PHP status | Port | Note |
|---|---|---|---|
| `bmoexternalaccount.search` | stable | shadowed |  |
| `bugzilla.account.search` | stable | shadowed |  |
| `feed.query_id` | unstable | shadowed |  |
| `policy.create` | stable | Rust |  |
| `policy.query` | stable | shadowed |  |

### moz-extensions: differential

| Method | PHP status | Port | Note |
|---|---|---|---|
| `differential.custom.revision.visibility` | unstable | shadowed |  |

### moz-extensions: email

| Method | PHP status | Port | Note |
|---|---|---|---|
| `feed.for_email.query` | unstable | shadowed |  |
| `feed.for_email.query_id` | unstable | shadowed |  |
| `feed.for_email.status` | unstable | shadowed |  |
| `feed.for_email.status_id` | unstable | shadowed |  |

### moz-extensions: reviewhelper

| Method | PHP status | Port | Note |
|---|---|---|---|
| `reviewhelper.request` | unstable | not ported | calls Mozilla's review helper service over HTTP and records the request (write). |

### People

| Method | PHP status | Port | Note |
|---|---|---|---|
| `user.disable` | deprecated | Rust |  |
| `user.edit` | stable | Rust |  |
| `user.enable` | deprecated | Rust |  |
| `user.find` | deprecated | shadowed |  |
| `user.query` | frozen | shadowed |  |
| `user.search` | stable | shadowed |  |
| `user.whoami` | stable | shadowed |  |

### PHID

| Method | PHP status | Port | Note |
|---|---|---|---|
| `phid.info` | deprecated | shadowed |  |
| `phid.lookup` | stable | shadowed |  |
| `phid.query` | stable | shadowed |  |

### Projects

| Method | PHP status | Port | Note |
|---|---|---|---|
| `project.create` | frozen | Rust |  |
| `project.edit` | stable | Rust |  |
| `project.query` | frozen | shadowed |  |
| `project.search` | stable | shadowed |  |

### Remarkup

| Method | PHP status | Port | Note |
|---|---|---|---|
| `remarkup.process` | unstable | shadowed |  |

### Repositories (Diffusion)

| Method | PHP status | Port | Note |
|---|---|---|---|
| `repository.query` | frozen | shadowed |  |

### Edges (infrastructure)

| Method | PHP status | Port | Note |
|---|---|---|---|
| `edge.search` | stable | shadowed |  |

### Transactions

| Method | PHP status | Port | Note |
|---|---|---|---|
| `transaction.search` | stable | shadowed |  |

## Web routes

`{name}` is a variable segment, `[...]` an optional part, `{a|b}` alternatives. `/D1/action` and `/rust-ui/*` are Rust-only routes PHP doesn't have.

### Applications (`PhabricatorApplicationsApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/applications/[query/{queryKey}/]` | `PhabricatorApplicationsListController` | partial | builtin queries (Launcher, All); saved queries go to PHP |
| `/applications/view/{application}/` | `PhabricatorApplicationDetailViewController` | partial | details and policies; the storage panels of Files and Search and applications with policy edits (their timeline) go to PHP |
| `/applications/edit/{application}/` | `PhabricatorApplicationEditController` | not ported | write (form or action) |
| `/applications/{application}/{install\|uninstall}/` | `PhabricatorApplicationUninstallController` | not ported | write (form or action) |
| `/applications/panel/{application}/{panel}/{path...}` | `PhabricatorApplicationPanelController` | not ported | admin: application panels |

### Auth (`PhabricatorAuthApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/auth/` | `PhabricatorAuthListController` | not ported | admin: auth providers |
| `/auth/config/new/` | `PhabricatorAuthNewController` | not ported | write (form or action) |
| `/auth/config/edit/[{id}/]` | `PhabricatorAuthEditController` | not ported | write (form or action) |
| `/auth/config/{enable\|disable}/{id}/` | `PhabricatorAuthDisableController` | not ported | write (form or action) |
| `/auth/config/view/{id}/` | `PhabricatorAuthProviderViewController` | not ported | admin: auth provider |
| `/auth/login/{pkey}/[{extra}/]` | `PhabricatorAuthLoginController` | ported, not routed | W7 web login, off by default (`PHAB_RUST_LOGIN=1`); also links and refreshes BMO accounts of logged-in users; security review pending |
| `/auth/loggedout/` | `PhabricatorAuthStartController` | ported, not routed | W7 web login, off by default (`PHAB_RUST_LOGIN=1`); security review pending |
| `/auth/invite/{code}/` | `PhabricatorAuthInviteController` | not ported | invites (unused with BMO login) |
| `/auth/register/[{akey}/]` | `PhabricatorAuthRegisterController` | ported, not routed | W7, `PHAB_RUST_LOGIN=1`: BMO accounts with an account key, approval mail included; PHP keeps addresses to verify by mail, `auth.email-domains`, invites, event listeners, `phabricator.production-uri`, taken addresses and `/auth/register/` without a key; security review pending |
| `/auth/start/` | `PhabricatorAuthStartController` | ported, not routed | W7 web login, off by default (`PHAB_RUST_LOGIN=1`); security review pending |
| `/auth/validate/` | `PhabricatorAuthValidateController` | ported, not routed | W7 web login, off by default (`PHAB_RUST_LOGIN=1`); security review pending |
| `/auth/finish/` | `PhabricatorAuthFinishController` | ported, not routed | W7 web login, off by default (`PHAB_RUST_LOGIN=1`); TOTP prompt (`auth_challenge`, `auth.factor` rate limit, `jump_to_hisec`); other factor types stay PHP; security review pending |
| `/auth/unlink/{id}/` | `PhabricatorAuthUnlinkController` | not ported | write (form or action) |
| `/auth/{link\|refresh}/{id}/` | `PhabricatorAuthLinkController` | ported, not routed | W7, `PHAB_RUST_LOGIN=1`: BMO only; security review pending |
| `/auth/confirmlink/{akey}/` | `PhabricatorAuthConfirmLinkController` | ported, not routed | W7, `PHAB_RUST_LOGIN=1`: BMO only, with the TOTP prompt of `requireHighSecurityToken()`; security review pending |
| `/auth/session/terminate/{id}/` | `PhabricatorAuthTerminateSessionController` | Rust | deletes one or all of the viewer's other sessions (PHP logs nothing); sessions of external accounts and workflow dialogs 421 |
| `/auth/token/revoke/{id}/` | `PhabricatorAuthRevokeTokenController` | not ported | write (form or action) |
| `/auth/session/downgrade/` | `PhabricatorAuthDowngradeSessionController` | Rust | leaves high security (`hisec-exit` log row); workflow dialogs 421 |
| `/auth/enroll/[{pageKey}/]` | `PhabricatorAuthNeedsMultiFactorController` | not ported | MFA enrollment (writes a factor); Rust pages send the user there |
| `/auth/sshkey/for/{forPHID}/[query/{queryKey}/[{queryAction}/]]` | `PhabricatorAuthSSHKeyListController` | not ported | SSH keys (unused without hosting) |
| `/auth/sshkey/generate/` | `PhabricatorAuthSSHKeyGenerateController` | not ported | write (form or action) |
| `/auth/sshkey/upload/` | `PhabricatorAuthSSHKeyEditController` | not ported | write (form or action) |
| `/auth/sshkey/edit/{id}/` | `PhabricatorAuthSSHKeyEditController` | not ported | write (form or action) |
| `/auth/sshkey/revoke/{id}/` | `PhabricatorAuthSSHKeyRevokeController` | not ported | write (form or action) |
| `/auth/sshkey/view/{id}/` | `PhabricatorAuthSSHKeyViewController` | not ported | SSH key (unused without hosting) |
| `/auth/password/` | `PhabricatorAuthSetPasswordController` | not ported | write (form or action) |
| `/auth/external/` | `PhabricatorAuthSetExternalController` | not ported | write (form or action) |
| `/auth/mfa/[query/{queryKey}/[{queryAction}/]]` | `PhabricatorAuthFactorProviderListController` | not ported | admin: MFA providers |
| `/auth/mfa/edit/[{id}/][{editAction}/]` | `PhabricatorAuthFactorProviderEditController` | not ported | write (form or action) |
| `/auth/mfa/{id}/` | `PhabricatorAuthFactorProviderViewController` | not ported | admin: MFA provider |
| `/auth/mfa/message/{id}/` | `PhabricatorAuthFactorProviderMessageController` | not ported | admin: MFA provider message |
| `/auth/mfa/challenge/status/{id}/` | `PhabricatorAuthChallengeStatusController` | not ported | MFA challenge polling of PHP forms |
| `/auth/message/[query/{queryKey}/[{queryAction}/]]` | `PhabricatorAuthMessageListController` | not ported | admin: auth messages |
| `/auth/message/edit/[{id}/][{editAction}/]` | `PhabricatorAuthMessageEditController` | not ported | write (form or action) |
| `/auth/message/{id}/` | `PhabricatorAuthMessageViewController` | not ported | admin: auth message |
| `/login/` | `PhabricatorAuthStartController` | ported, not routed | W7 web login, off by default (`PHAB_RUST_LOGIN=1`); security review pending |
| `/login/email/` | `PhabricatorEmailLoginController` | not ported | write: sends a login link |
| `/login/once/{type}/{id}/{key}/[{emailID}/]` | `PhabricatorAuthOneTimeLoginController` | not ported | write: one-time login links |
| `/login/refresh/` | `PhabricatorRefreshCSRFController` | not ported | CSRF refresh of PHP pages |
| `/login/mustverify/` | `PhabricatorMustVerifyEmailController` | not ported | roadblock page (sends mail) |
| `/emailverify/{code}/` | `PhabricatorEmailVerificationController` | not ported | write: verifies an address |
| `/logout/` | `PhabricatorLogoutController` | ported, not routed | W7 web login, off by default (`PHAB_RUST_LOGIN=1`); security review pending |

### Celerity (`PhabricatorCelerityApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/res/[{mtime}T/][{postprocessor}X/]{library}/{hash}/{path...}` | `CelerityPhabricatorResourceController` | not ported | Celerity static resources; Rust pages use `/rsrc-rust/`. Keep in PHP until PHP pages are gone |

### Conduit (`PhabricatorConduitApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/conduit/[query/{queryKey}/[{queryAction}/]]` | `PhabricatorConduitListController` | partial | builtin queries (Modern, All), from the method catalog (`scripts/generate-catalogs.php`); saved queries go to PHP |
| `/conduit/method/{method}/` | `PhabricatorConduitConsoleController` | partial | properties, description and parameters; methods with documentation pages (search and edit engines) go to PHP, and the "Call Method" form stays PHP's |
| `/conduit/log/[query/{queryKey}/[{queryAction}/]]` | `PhabricatorConduitLogController` | not ported | admin: call log (search engine) |
| `/conduit/log/view/{view}/` | `PhabricatorConduitLogController` | not ported | admin: call log (search engine) |
| `/conduit/token/` | `PhabricatorConduitTokenController` | not ported | shows (and on first use creates) the CLI token |
| `/conduit/token/edit/[{id}/]` | `PhabricatorConduitTokenEditController` | Rust | generates and shows the viewer's tokens (and bots' for administrators), behind the TOTP high security prompt; other users' tokens, non-TOTP factors and workflow dialogs 421 |
| `/conduit/token/terminate/[{id}/]` | `PhabricatorConduitTokenTerminateController` | Rust | expires one or all of the viewer's tokens (and bots' for administrators), behind the TOTP high security prompt; other users' tokens, non-TOTP factors and workflow dialogs 421 |
| `/conduit/login/` | `PhabricatorConduitTokenHandshakeController` | not ported | `arc install-certificate` (writes a token) |
| `/api/{method}` | `PhabricatorConduitAPIController` | partial | per method: see "Conduit methods" |

### Config (`PhabricatorConfigApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/config/` | `PhabricatorConfigConsoleController` | Rust | admin: the console menu; the library versions (read from Git) are only in PHP's |
| `/config/edit/{key}/` | `PhabricatorConfigEditController` | partial | admin, read-only: description, examples and the value of each source; saves (POST), unknown options, custom types and entries with a history go to PHP (or `phab-cli config set`) |
| `/config/database/[{ref}/[{database}/[{table}/[col/{column}\|key/{key}/]]]]` | `PhabricatorConfigDatabaseStatusController` | not ported | admin: config UI, to be replaced by a CLI (§9.3) |
| `/config/dbissue/` | `PhabricatorConfigDatabaseIssueController` | not ported | admin: config UI, to be replaced by a CLI (§9.3) |
| `/config/{ignore\|unignore}/{key}/` | `PhabricatorConfigIgnoreController` | not ported | write (form or action) |
| `/config/issue/` | `PhabricatorConfigIssueListController` | not ported | admin: the results of PHP's setup checks, which examine PHP's own environment; stays PHP |
| `/config/issue/panel/` | `PhabricatorConfigIssuePanelController` | not ported | admin: the results of PHP's setup checks; stays PHP |
| `/config/issue/{key}/` | `PhabricatorConfigIssueViewController` | not ported | admin: the results of PHP's setup checks; stays PHP |
| `/config/cache/` | `PhabricatorConfigCacheController` | not ported | admin: config UI, to be replaced by a CLI (§9.3) |
| `/config/cache/purge/` | `PhabricatorConfigPurgeCacheController` | not ported | write (form or action) |
| `/config/module/[{module}/]` | `PhabricatorConfigModuleController` | not ported | admin: config UI, to be replaced by a CLI (§9.3) |
| `/config/cluster/databases/` | `PhabricatorConfigClusterDatabasesController` | not ported | admin: config UI, to be replaced by a CLI (§9.3) |
| `/config/cluster/repositories/` | `PhabricatorConfigClusterRepositoriesController` | not ported | admin: config UI, to be replaced by a CLI (§9.3) |
| `/config/cluster/search/` | `PhabricatorConfigClusterSearchController` | not ported | admin: config UI, to be replaced by a CLI (§9.3) |
| `/config/settings/` | `PhabricatorConfigSettingsListController` | Rust | admin: core settings, read-only |
| `/config/settings/{advanced\|all}/` | `PhabricatorConfigSettingsListController` | Rust | admin: settings lists, read-only |
| `/config/settings/history/` | `PhabricatorConfigSettingsHistoryController` | not ported | admin: config UI, to be replaced by a CLI (§9.3) |

### Daemons (`PhabricatorDaemonsApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/daemon/` | `PhabricatorDaemonConsoleController` | Rust | admin: completed, queued, leased and upcoming tasks, live daemons (their status computed, not saved as PHP does) and triggers |
| `/daemon/task/{id}/` | `PhabricatorWorkerTaskDetailController` | partial | admin: a task and its retries; the tasks of Harbormaster and of the commit parsers, whose data PHP renders, go to PHP |
| `/daemon/log/` | `PhabricatorDaemonLogListController` | not ported | admin: daemon logs |
| `/daemon/log/{id}/` | `PhabricatorDaemonLogViewController` | not ported | admin: daemon log |
| `/daemon/bulk/[query/{queryKey}/]` | `PhabricatorDaemonBulkJobListController` | partial | builtin queries (Authored, All); saved queries go to PHP |
| `/daemon/bulk/monitor/{id}/` | `PhabricatorDaemonBulkJobMonitorController` | not ported | admin: bulk job monitor |
| `/daemon/bulk/view/{id}/` | `PhabricatorDaemonBulkJobViewController` | not ported | admin: bulk job |

### Dashboard (`PhabricatorDashboardApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/W{id}` | `PhabricatorDashboardPanelViewController` | partial | panel page with its usage and timeline; query panels, panels used by other objects and timelines with text, tab or query changes 421 |
| `/dashboard/[query/{queryKey}/]` | `PhabricatorDashboardListController` | partial | the builtin queries (`authored`, the default, `open`, `all`); saved queries and search parameters 421 |
| `/dashboard/view/{id}/` | `PhabricatorDashboardViewController` | Rust | dashboard with its panels |
| `/dashboard/archive/{id}/` | `PhabricatorDashboardArchiveController` | not ported | write (form or action) |
| `/dashboard/edit/[{id}/][{editAction}/]` | `PhabricatorDashboardEditController` | not ported | write (form or action) |
| `/dashboard/install/{id}/[{workflowKey}/[{modeKey}/]]` | `PhabricatorDashboardInstallController` | not ported | write (form or action) |
| `/dashboard/console/` | `PhabricatorDashboardConsoleController` | not ported | console; read-only, static |
| `/dashboard/adjust/{remove\|add\|move}/` | `PhabricatorDashboardAdjustController` | not ported | write (form or action) |
| `/dashboard/panel/install/{engineKey}/[{queryKey}/]` | `PhabricatorDashboardQueryPanelInstallController` | not ported | write (form or action) |
| `/dashboard/panel/[query/{queryKey}/]` | `PhabricatorDashboardPanelListController` | partial | the builtin queries (`authored`, the default, `active`, `all`); saved queries and search parameters 421 |
| `/dashboard/panel/edit/[{id}/][{editAction}/]` | `PhabricatorDashboardPanelEditController` | not ported | write (form or action) |
| `/dashboard/panel/render/{id}/` | `PhabricatorDashboardPanelRenderController` | not ported | async panel rendering of PHP pages |
| `/dashboard/panel/archive/{id}/` | `PhabricatorDashboardPanelArchiveController` | not ported | write (form or action) |
| `/dashboard/panel/tabs/{id}/{add\|move\|remove\|rename}/` | `PhabricatorDashboardPanelTabsController` | not ported | write (form or action) |
| `/portal/[query/{queryKey}/[{queryAction}/]]` | `PhabricatorDashboardPortalListController` | not ported | portal search engine page; read-only |
| `/portal/edit/[{id}/][{editAction}/]` | `PhabricatorDashboardPortalEditController` | not ported | write (form or action) |
| `/portal/view/{portalID}/` | `PhabricatorDashboardPortalViewController` | Rust | portal with its menu and default dashboard |
| `/portal/view/{portalID}/view/{itemID}/` | `PhabricatorDashboardPortalViewController` | not ported | portal menu items (configure/edit write) |
| `/portal/view/{portalID}/hide/{itemID}/` | `PhabricatorDashboardPortalViewController` | not ported | portal menu items (configure/edit write) |
| `/portal/view/{portalID}/default/{itemID}/` | `PhabricatorDashboardPortalViewController` | not ported | portal menu items (configure/edit write) |
| `/portal/view/{portalID}/configure/` | `PhabricatorDashboardPortalViewController` | not ported | portal menu items (configure/edit write) |
| `/portal/view/{portalID}/configure/{global\|custom}/` | `PhabricatorDashboardPortalViewController` | not ported | portal menu items (configure/edit write) |
| `/portal/view/{portalID}/reorder/{global\|custom}/` | `PhabricatorDashboardPortalViewController` | not ported | portal menu items (configure/edit write) |
| `/portal/view/{portalID}/edit/[{id}/][{editAction}/]` | `PhabricatorDashboardPortalViewController` | not ported | portal menu items (configure/edit write) |
| `/portal/view/{portalID}/new/{global\|custom}/{itemKey}/[{id}/][{editAction}/]` | `PhabricatorDashboardPortalViewController` | not ported | portal menu items (configure/edit write) |
| `/portal/view/{portalID}/builtin/{itemID}/[{id}/][{editAction}/]` | `PhabricatorDashboardPortalViewController` | not ported | portal menu items (configure/edit write) |

### Differential (`PhabricatorDifferentialApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/D{id}` | `DifferentialRevisionViewController` | Rust | revision page; `?id=` shows another of its diffs, `?download=true` its raw diff (Git and Mercurial repositories: the patch stored as a temporary file attached to the revision, as PHP does; unified diffs 421); comparisons (`?vs=`) 421; `/DN/action` (Rust only) posts its forms |
| `/D{id}/new/` | `DifferentialRevisionViewController` | Rust | changes since the viewer's last action, read from the transactions: all changes with PHP's notice when there is no action, or no update since; the comparison with the diff of the last action is PHP's (421) |
| `/differential/[query/{queryKey}/[{queryAction}/]]` | `DifferentialRevisionListController` | partial | `/differential/` only (the queue); saved and builtin queries go to PHP |
| `/differential/diff/{id}/` | `DifferentialDiffViewController` | Rust | a diff without a revision, with the revision page's renderer (diffs of revisions redirect to `/DN?id=`); its "Attach To" form posts to PHP (421) |
| `/differential/diff/{id}/changesets/[query/{queryKey}/[{queryAction}/]]` | `DifferentialChangesetListController` | partial | the builtin query (All Changesets); saved queries, filters and query actions 421 |
| `/differential/diff/create/` | `DifferentialDiffCreateController` | not ported | write: paste or upload a diff |
| `/differential/changeset/` | `DifferentialChangesetViewController` | partial | the standalone changeset (a Rust page); the Ajax requests of PHP's revision and diff pages (each changeset, its "show more" context) answered with PHP's JSON from PHP's parse cache, side by side (`changeset_ajax.rs`, checked against `tests/oracle/changeset.php`); no cache yet, comparisons, view state changes, raw files (`?view=`), the unified renderer, inline comments, lint and coverage, document engines and property changes 421 |
| `/differential/revision/edit/[{id}/][{editAction}/]` | `DifferentialRevisionEditController` | partial | the edit form `/differential/revision/edit/N/` (`revision_edit.rs`: title, summary, test plan, reviewers, tags, subscribers, Bugzilla bug ID; author, repository and policy changes, workflows and validation errors 421) and POST `/differential/revision/edit/N/comment/` (the comment form, `web_edit.rs`; 421 for workflows, drafts and unported actions); new revisions and other edit actions go to PHP |
| `/differential/revision/attach/{diffID}/to/[{id}/][{editAction}/]` | `DifferentialRevisionEditController` | not ported | write (form or action) |
| `/differential/revision/closedetails/{phid}/` | `DifferentialRevisionCloseDetailsController` | not ported | dialog; read-only |
| `/differential/revision/update/{revisionID}/` | `DifferentialDiffCreateController` | not ported | write: paste or upload a diff |
| `/differential/revision/inlines/{id}/` | `DifferentialRevisionInlinesController` | not ported | inline list; read-only, small |
| `/differential/revision/paths/{id}/` | `DifferentialRevisionAffectedPathsController` | not ported | affected paths; read-only, small |
| `/differential/comment/inline/edit/{id}/` | `DifferentialInlineCommentEditController` | not ported | write: draft inline comments (the Rust page posts inlines to PHP's Conduit API) |
| `/differential/preview/` | `PhabricatorMarkupPreviewController` | not ported | Rust pages use `/rust-ui/preview` |

### Diffusion (`PhabricatorDiffusionApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/r{repositoryCallsign}\|R{repositoryID}:{commit}` | `DiffusionCommitController` | Rust | commit page (Git) |
| `/{repo}/` | `DiffusionRepositoryController` | Rust | repository home with "Locate File" (Git; Mercurial and Subversion 421) |
| `/{repo}/repository/{dblob...}` | `DiffusionRepositoryController` | Rust | Git only; Mercurial, Subversion and unported views 421 |
| `/{repo}/change/{dblob...}` | `DiffusionChangeController` | Rust | Git; binary and image changes 421 |
| `/{repo}/clone/` | `DiffusionCloneController` | not ported | clone dialog; read-only, small |
| `/{repo}/history/{dblob...}` | `DiffusionHistoryController` | Rust | Git only, with the commit graph; Mercurial, Subversion and unported views 421 |
| `/{repo}/browse/{dblob...}` | `DiffusionBrowseController` | Rust | Git only, with blame (`?blame=off`); Mercurial, Subversion and unported views 421 |
| `/{repo}/document/{dblob...}` | `DiffusionDocumentController` | not ported | document rendering of a file; read-only |
| `/{repo}/blame/{dblob...}` | `DiffusionBlameController` | Rust | JSON of the browse pages; tooltips are `title`s, no profile pictures |
| `/{repo}/lastmodified/{dblob...}` | `DiffusionLastModifiedController` | Rust | JSON of the browse pages; branches with lint and invalid CSRF tokens 421 |
| `/{repo}/diff/` | `DiffusionDiffController` | not ported | inline diff of the PHP commit page |
| `/{repo}/tags/{dblob...}` | `DiffusionTagListController` | Rust | Git only; Mercurial, Subversion and unported views 421 |
| `/{repo}/branches/{dblob...}` | `DiffusionBranchTableController` | Rust | Git only; Mercurial, Subversion and unported views 421 |
| `/{repo}/refs/{dblob...}` | `DiffusionRefTableController` | not ported | refs table; read-only |
| `/{repo}/lint/{dblob...}` | `DiffusionLintController` | not ported | lint messages; read-only (`repository_lintmessage`) |
| `/{repo}/commit/{commit}/` | `DiffusionCommitController` | Rust | commit page (Git) |
| `/{repo}/commit/{commit}/branches/` | `DiffusionCommitBranchesController` | Rust | JSON of the commit pages |
| `/{repo}/commit/{commit}/tags/` | `DiffusionCommitTagsController` | Rust | JSON of the commit pages |
| `/{repo}/compare/` | `DiffusionCompareController` | Rust | Git, with the commit graph and the "Compare Against" dialog, whose post goes to PHP (421) |
| `/{repo}/manage/[{panel}/]` | `DiffusionRepositoryManagePanelsController` | partial | read-only: Basics, Policies, Storage, Limits, Branches, Staging Area, Symbols; URIs, History, cluster repositories and Spaces 421 |
| `/{repo}/uri/view/{id}/` | `DiffusionRepositoryURIViewController` | not ported | admin: repository URI |
| `/{repo}/uri/disable/{id}/` | `DiffusionRepositoryURIDisableController` | not ported | write (form or action) |
| `/{repo}/uri/edit/[{id}/][{editAction}/]` | `DiffusionRepositoryURIEditController` | not ported | write (form or action) |
| `/{repo}/uri/credential/{id}/{edit\|remove}/` | `DiffusionRepositoryURICredentialController` | not ported | write (form or action) |
| `/{repo}/edit/activate/` | `DiffusionRepositoryEditActivateController` | not ported | write (form or action) |
| `/{repo}/edit/dangerous/` | `DiffusionRepositoryEditDangerousController` | not ported | write (form or action) |
| `/{repo}/edit/enormous/` | `DiffusionRepositoryEditEnormousController` | not ported | write (form or action) |
| `/{repo}/edit/delete/` | `DiffusionRepositoryEditDeleteController` | not ported | write (form or action) |
| `/{repo}/edit/update/` | `DiffusionRepositoryEditUpdateController` | not ported | write (form or action) |
| `/{repo}/edit/publish/` | `DiffusionRepositoryEditPublishingController` | not ported | write (form or action) |
| `/{repo}/pathtree/{dblob...}` | `DiffusionPathTreeController` | Rust | JSON of "Locate File" (repository pages) |
| `/{repo}[/*]` | `DiffusionRepositoryDefaultController` | not ported | catch-all of repository URIs (redirects); Git clients never send the opt-in cookie |
| `/diffusion/[query/{queryKey}/[{queryAction}/]]` | `DiffusionRepositoryListController` | partial | builtin queries (Active, All); saved queries, filters and query actions 421 |
| `/diffusion/edit/[{id}/][{editAction}/]` | `DiffusionRepositoryEditController` | not ported | write (form or action) |
| `/diffusion/pushlog/[query/{queryKey}/[{queryAction}/]]` | `DiffusionPushLogListController` | not ported | push logs (hosting only) |
| `/diffusion/pushlog/view/{id}/` | `DiffusionPushEventViewController` | not ported | push event (hosting only) |
| `/diffusion/synclog/[query/{queryKey}/[{queryAction}/]]` | `DiffusionSyncLogListController` | not ported | cluster sync logs (no cluster) |
| `/diffusion/pulllog/[query/{queryKey}/[{queryAction}/]]` | `DiffusionPullLogListController` | not ported | pull logs (hosting only) |
| `/diffusion/identity/[query/{queryKey}/[{queryAction}/]]` | `DiffusionIdentityListController` | not ported | identity search engine page; read-only |
| `/diffusion/identity/edit/[{id}/][{editAction}/]` | `DiffusionIdentityEditController` | not ported | write (form or action) |
| `/diffusion/identity/view/{id}/` | `DiffusionIdentityViewController` | not ported | identity page; read-only |
| `/diffusion/inline/edit/{phid}/` | `DiffusionInlineCommentController` | not ported | write: commit inline comments |
| `/diffusion/services/path/complete/` | `DiffusionPathCompleteController` | not ported | path typeahead of PHP forms |
| `/diffusion/services/path/validate/` | `DiffusionPathValidateController` | not ported | path validation of PHP forms |
| `/diffusion/symbol/{name}/` | `DiffusionSymbolController` | not ported | symbol search (no symbol index at Mozilla) |
| `/diffusion/external/` | `DiffusionExternalController` | not ported | external commit redirect; read-only |
| `/diffusion/lint/` | `DiffusionLintController` | not ported | lint messages; read-only (`repository_lintmessage`) |
| `/diffusion/commit/[query/{queryKey}/[{queryAction}/]]` | `DiffusionCommitListController` | not ported | commit search engine page; read-only |
| `/diffusion/commit/edit/[{id}/][{editAction}/]` | `DiffusionCommitEditController` | not ported | write (form or action) |
| `/diffusion/picture/{id}/` | `DiffusionRepositoryProfilePictureController` | not ported | write: repository picture |

### Favorites (`PhabricatorFavoritesApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/favorites/` | `PhabricatorFavoritesMenuItemController` | partial | the viewer's favorites menu, read-only; items of other types and configured project or repository forms 421 |
| `/favorites/menu/view/{itemID}/` | `PhabricatorFavoritesMenuItemController` | not ported | favorites menu (configure/edit write) |
| `/favorites/menu/hide/{itemID}/` | `PhabricatorFavoritesMenuItemController` | not ported | favorites menu (configure/edit write) |
| `/favorites/menu/default/{itemID}/` | `PhabricatorFavoritesMenuItemController` | not ported | favorites menu (configure/edit write) |
| `/favorites/menu/configure/` | `PhabricatorFavoritesMenuItemController` | not ported | favorites menu (configure/edit write) |
| `/favorites/menu/configure/{global\|custom}/` | `PhabricatorFavoritesMenuItemController` | not ported | favorites menu (configure/edit write) |
| `/favorites/menu/reorder/{global\|custom}/` | `PhabricatorFavoritesMenuItemController` | not ported | favorites menu (configure/edit write) |
| `/favorites/menu/edit/[{id}/][{editAction}/]` | `PhabricatorFavoritesMenuItemController` | not ported | favorites menu (configure/edit write) |
| `/favorites/menu/new/{global\|custom}/{itemKey}/[{id}/][{editAction}/]` | `PhabricatorFavoritesMenuItemController` | not ported | favorites menu (configure/edit write) |
| `/favorites/menu/builtin/{itemID}/[{id}/][{editAction}/]` | `PhabricatorFavoritesMenuItemController` | not ported | favorites menu (configure/edit write) |

### Feed (`PhabricatorFeedApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/feed/{id}/` | `PhabricatorFeedDetailController` | Rust | new: one story; `?text=1` and stories Rust can't render 421 |
| `/feed/[query/{queryKey}/]` | `PhabricatorFeedListController` | partial | `/feed/` (`?userPHIDs=`, paging) and the builtin queries `all` and `projects` (new); saved queries 421 |
| `/feed/transactions/[query/{queryKey}/[{queryAction}/]]` | `PhabricatorFeedTransactionListController` | not ported | transaction search engine page; read-only |

### Files (`PhabricatorFilesApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/F{id}[${lines}]` | `PhabricatorFileViewController` | partial | file page without `$lines`; files with a timeline, files the viewer can't see (PHP's policy page) and storage outside MySQL 421 |
| `/file/[query/{queryKey}/]` | `PhabricatorFileListController` | partial | builtin queries (Authored, All); saved queries and filters 421 |
| `/file/view/{id}/[{engineKey}/][${lines}]` | `PhabricatorFileViewController` | partial | `/file/view/{id}/` as `/F{id}`; engine keys and lines go to PHP |
| `/file/info/{phid}/` | `PhabricatorFileViewController` | Rust | redirect to `/F{id}` |
| `/file/upload/` | `PhabricatorFileUploadController` | not ported | write (form or action) |
| `/file/dropupload/` | `PhabricatorFileDropUploadController` | not ported | write (form or action) |
| `/file/compose/` | `PhabricatorFileComposeController` | not ported | write (form or action) |
| `/file/thread/{phid}/` | `PhabricatorFileLightboxController` | not ported | lightbox comments of PHP pages |
| `/file/delete/{id}/` | `PhabricatorFileDeleteController` | not ported | write (form or action) |
| `/file/edit/[{id}/][{editAction}/]` | `PhabricatorFileEditController` | not ported | write (form or action) |
| `/file/imageproxy/` | `PhabricatorFileImageProxyController` | not ported | fetches external images (writes a file) |
| `/file/transforms/{id}/` | `PhabricatorFileTransformListController` | not ported | admin: transforms of a file |
| `/file/uploaddialog/[{single}]` | `PhabricatorFileUploadDialogController` | not ported | write (form or action) |
| `/file/iconset/{key}/select/` | `PhabricatorFileIconSetSelectController` | not ported | icon picker dialog of PHP forms |
| `/file/document/{engineKey}/{phid}/` | `PhabricatorFileDocumentController` | not ported | document engine render of PHP pages |
| `/file/ui/detach/{objectPHID}/{filePHID}/` | `PhabricatorFileDetachController` | not ported | write (form or action) |
| `/file/ui/curtain/list/{phid}/` | `PhabricatorFileUICurtainListController` | not ported | dialog of PHP pages |
| `/file/ui/curtain/attach/{objectPHID}/{filePHID}/` | `PhabricatorFileUICurtainAttachController` | not ported | write (form or action) |
| `/file/{data\|download}/[@{instance}/]{key}/{phid}/[{token}/]*` | `PhabricatorFileDataController` | partial | data from MySQL with PHP's headers (type, disposition, cache, ranges, compression); only with the opt-in cookie, so not on an alternate file domain. Download dialogs, wrong keys, partial uploads, the redirect to the file domain, PDFs shown inline, odd ranges and other storage 421 |
| `/file/xform/[@{instance}/]{transform}/{phid}/{key}/` | `PhabricatorFileTransformController` | not ported | image thumbnails (writes the transform on first use) |

### Flags (`PhabricatorFlagsApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/flag/[query/{queryKey}/]` | `PhabricatorFlagListController` | partial | the Flagged query; saved queries, filters and more than a page 421 |
| `/flag/view/{view}/` | `PhabricatorFlagListController` | not ported | flag list; read-only (`flag.query` is ported) |
| `/flag/edit/{phid}/` | `PhabricatorFlagEditController` | not ported | write (form or action) |
| `/flag/delete/{id}/` | `PhabricatorFlagDeleteController` | not ported | write (form or action) |

### Harbormaster (`PhabricatorHarbormasterApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/B{id}` | `HarbormasterBuildableViewController` | Rust | buildable |
| `/harbormaster/[query/{queryKey}/]` | `HarbormasterBuildableListController` | partial | the default list only |
| `/harbormaster/step/add/[{id}/]` | `HarbormasterStepAddController` | not ported | write (form or action) |
| `/harbormaster/step/new/{plan}/{class}/` | `HarbormasterStepEditController` | not ported | write (form or action) |
| `/harbormaster/step/view/{id}/` | `HarbormasterStepViewController` | partial | steps with credentials and edits in their timeline 421 |
| `/harbormaster/step/edit/[{id}/]` | `HarbormasterStepEditController` | not ported | write (form or action) |
| `/harbormaster/step/delete/[{id}/]` | `HarbormasterStepDeleteController` | not ported | write (form or action) |
| `/harbormaster/buildable/{id}/{pause\|resume\|restart\|abort}/` | `HarbormasterBuildableActionController` | not ported | write (form or action) |
| `/harbormaster/build/[query/{queryKey}/[{queryAction}/]]` | `HarbormasterBuildListController` | partial | builtin queries; saved queries and filters 421 |
| `/harbormaster/build/{id}/[{generation}/]` | `HarbormasterBuildViewController` | Rust | build and its older runs |
| `/harbormaster/build/{pause\|resume\|restart\|abort}/{id}/[{via}/]` | `HarbormasterBuildActionController` | not ported | write (form or action) |
| `/harbormaster/plan/[query/{queryKey}/[{queryAction}/]]` | `HarbormasterPlanListController` | partial | builtin queries; saved queries and filters 421 |
| `/harbormaster/plan/edit/[{id}/][{editAction}/]` | `HarbormasterPlanEditController` | not ported | write (form or action) |
| `/harbormaster/plan/disable/{id}/` | `HarbormasterPlanDisableController` | not ported | write (form or action) |
| `/harbormaster/plan/behavior/{id}/{behaviorKey}/` | `HarbormasterPlanBehaviorController` | not ported | write (form or action) |
| `/harbormaster/plan/run/{id}/` | `HarbormasterPlanRunController` | not ported | write (form or action) |
| `/harbormaster/plan/{id}/` | `HarbormasterPlanViewController` | Rust | build plan |
| `/harbormaster/unit/{id}/` | `HarbormasterUnitMessageListController` | not ported | unit results of a buildable; read-only |
| `/harbormaster/unit/view/{id}/` | `HarbormasterUnitMessageViewController` | not ported | one unit result; read-only |
| `/harbormaster/lint/{id}/` | `HarbormasterLintMessagesController` | not ported | lint results of a buildable; read-only |
| `/harbormaster/log/view/{id}/[${lines}]` | `HarbormasterBuildLogViewController` | partial | without `$lines`; the whole log, or its last lines (`?l=`) |
| `/harbormaster/log/render/{id}/[${lines}]` | `HarbormasterBuildLogRenderController` | not ported | log rendering of PHP pages |
| `/harbormaster/log/download/{id}/` | `HarbormasterBuildLogDownloadController` | not ported | log download (writes a file) |

### Help (`PhabricatorHelpApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/help/keyboardshortcut/` | `PhabricatorHelpKeyboardShortcutController` | partial | as a page (`?keys=`); the dialogs PHP pages request go to PHP |
| `/help/documentation/{application}/` | `PhabricatorHelpDocumentationController` | Rust | the help articles of an application (from the catalog) |

### Herald (`PhabricatorHeraldApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/H{id}` | `HeraldRuleViewController` | Rust | Herald rule |
| `/herald/[query/{queryKey}/]` | `HeraldRuleListController` | partial | builtin queries; saved queries 421 |
| `/herald/new/` | `HeraldNewController` | not ported | write (form or action) |
| `/herald/create/` | `HeraldNewController` | not ported | write (form or action) |
| `/herald/edit/[{id}/]` | `HeraldRuleController` | not ported | write: the rule editor |
| `/herald/disable/{id}/{action}/` | `HeraldDisableController` | not ported | write (form or action) |
| `/herald/test/` | `HeraldTestConsoleController` | not ported | runs rules against an object (dry run, writes a transcript) |
| `/herald/transcript/` | `HeraldTranscriptListController` | Rust | default list |
| `/herald/transcript/[query/{queryKey}/]` | `HeraldTranscriptListController` | partial | builtin queries |
| `/herald/transcript/{id}/[{view}/]` | `HeraldTranscriptController` | partial | transcripts of revisions, every view; transaction titles not ported 421 |
| `/herald/webhook/[query/{queryKey}/[{queryAction}/]]` | `HeraldWebhookListController` | not ported | webhook list; read-only (admin) |
| `/herald/webhook/view/{id}/[request/{requestID}/]` | `HeraldWebhookViewController` | not ported | webhook page; read-only (admin) |
| `/herald/webhook/edit/[{id}/][{editAction}/]` | `HeraldWebhookEditController` | not ported | write (form or action) |
| `/herald/webhook/test/{id}/` | `HeraldWebhookTestController` | not ported | write (form or action) |
| `/herald/webhook/key/{view\|cycle}/{id}/` | `HeraldWebhookKeyController` | not ported | write (form or action) |

### Home (`PhabricatorHomeApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/` | `PhabricatorHomeMenuItemController` | partial | the home menu and the builtin home page (active revisions, repositories, recent activity); menus with stored items (pinned dashboards, personal items) go to PHP |
| `/home/` | `PhabricatorHomeMenuItemController` | not ported | home menu items (configure/edit write) |
| `/home/menu/view/{itemID}/` | `PhabricatorHomeMenuItemController` | not ported | home menu items (configure/edit write) |
| `/home/menu/hide/{itemID}/` | `PhabricatorHomeMenuItemController` | not ported | home menu items (configure/edit write) |
| `/home/menu/default/{itemID}/` | `PhabricatorHomeMenuItemController` | not ported | home menu items (configure/edit write) |
| `/home/menu/configure/` | `PhabricatorHomeMenuItemController` | not ported | home menu items (configure/edit write) |
| `/home/menu/configure/{global\|custom}/` | `PhabricatorHomeMenuItemController` | not ported | home menu items (configure/edit write) |
| `/home/menu/reorder/{global\|custom}/` | `PhabricatorHomeMenuItemController` | not ported | home menu items (configure/edit write) |
| `/home/menu/edit/[{id}/][{editAction}/]` | `PhabricatorHomeMenuItemController` | not ported | home menu items (configure/edit write) |
| `/home/menu/new/{global\|custom}/{itemKey}/[{id}/][{editAction}/]` | `PhabricatorHomeMenuItemController` | not ported | home menu items (configure/edit write) |
| `/home/menu/builtin/{itemID}/[{id}/][{editAction}/]` | `PhabricatorHomeMenuItemController` | not ported | home menu items (configure/edit write) |

### MetaMTA (`PhabricatorMetaMTAApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/mail/[query/{queryKey}/]` | `PhabricatorMetaMTAMailListController` | not ported | mail search engine page; read-only |
| `/mail/detail/{id}/` | `PhabricatorMetaMTAMailViewController` | partial | message, headers, delivery and metadata for the sender and the recipients; mail to projects or lists, encryption reasons and routing rules go to PHP |

### Notifications (`PhabricatorNotificationsApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/notification/[query/{queryKey}/]` | `PhabricatorNotificationListController` | partial | new: `all` and `unread`; saved queries and `?unread=` 421 |
| `/notification/panel/` | `PhabricatorNotificationPanelController` | not ported | JSON for the notification menu of PHP pages |
| `/notification/clear/` | `PhabricatorNotificationClearController` | not ported | write: mark all read |

### People (`PhabricatorPeopleApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/people/[query/{queryKey}/[{queryAction}/]]` | `PhabricatorPeopleListController` | partial | the builtin queries `active` (the default) and `all`, through `user.search`; the approval queue, saved queries and search parameters 421 |
| `/people/logs/[query/{queryKey}/[{queryAction}/]]` | `PhabricatorPeopleLogsController` | not ported | admin: user activity logs (search engine) |
| `/people/logs/{id}/` | `PhabricatorPeopleLogViewController` | not ported | admin: one log entry |
| `/people/invite/[query/{queryKey}/]` | `PhabricatorPeopleInviteListController` | not ported | admin: invites (unused with BMO login) |
| `/people/invite/send/` | `PhabricatorPeopleInviteSendController` | not ported | write (form or action) |
| `/people/approve/{id}/[via/{via}/]` | `PhabricatorPeopleApproveController` | not ported | write (form or action) |
| `/people/disapprove/{id}/` | `PhabricatorPeopleDisableController` | not ported | write (form or action) |
| `/people/disable/{id}/` | `PhabricatorPeopleDisableController` | not ported | write (form or action) |
| `/people/empower/{id}/` | `PhabricatorPeopleEmpowerController` | not ported | write (form or action) |
| `/people/delete/{id}/` | `PhabricatorPeopleDeleteController` | not ported | write (form or action) |
| `/people/rename/{id}/` | `PhabricatorPeopleRenameController` | not ported | write (form or action) |
| `/people/welcome/{id}/` | `PhabricatorPeopleWelcomeController` | not ported | write (form or action) |
| `/people/create/` | `PhabricatorPeopleCreateController` | not ported | write (form or action) |
| `/people/new/{type}/` | `PhabricatorPeopleNewController` | not ported | write (form or action) |
| `/people/editprofile/{id}/` | `PhabricatorPeopleProfileEditController` | not ported | write (form or action) |
| `/people/commits/{id}/` | `PhabricatorPeopleProfileCommitsController` | not ported | read-only; needs commits by author across repositories with their policies |
| `/people/revisions/{id}/` | `PhabricatorPeopleProfileRevisionsController` | Rust | new: recent revisions of a user |
| `/people/picture/{id}/` | `PhabricatorPeopleProfilePictureController` | not ported | write (form or action) |
| `/people/manage/{id}/` | `PhabricatorPeopleProfileManageController` | partial | details, timeline and the actions, which link to PHP; timelines with custom field changes or comments 421 |
| `/p/{username}/` | `PhabricatorPeopleProfileViewController` | Rust | profile |

### Policy (`PhabricatorPolicyApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/policy/explain/{phid}/{capability}/` | `PhabricatorPolicyExplainController` | partial | applications' policies; other objects (special rules, extended policies, spaces) and custom policies go to PHP |
| `/policy/edit/object/{objectPHID}\|type/{objectType}\|template/{templateType}/[{phid}/]` | `PhabricatorPolicyEditController` | not ported | write (form or action) |

### Project (`PhabricatorProjectApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/project/[query/{queryKey}/]` | `PhabricatorProjectListController` | partial | the builtin queries (`joined`, the default, `watching`, `active`, `all`); saved queries and search parameters 421 |
| `/project/filter/{filter}/` | `PhabricatorProjectListController` | not ported | project search engine page; read-only |
| `/project/archive/{id}/` | `PhabricatorProjectArchiveController` | not ported | write (form or action) |
| `/project/lock/{id}/` | `PhabricatorProjectLockController` | not ported | write (form or action) |
| `/project/members/{id}/` | `PhabricatorProjectMembersViewController` | Rust | new: read-only; the actions link to PHP |
| `/project/members/{id}/add/` | `PhabricatorProjectMembersAddController` | not ported | write (form or action) |
| `/project/{members\|watchers}/{id}/remove/` | `PhabricatorProjectMembersRemoveController` | not ported | write (form or action) |
| `/project/profile/{id}/` | `PhabricatorProjectProfileController` | Rust | project profile |
| `/project/view/{id}/` | `PhabricatorProjectViewController` | Rust | project profile |
| `/project/picture/{id}/` | `PhabricatorProjectEditPictureController` | not ported | write (form or action) |
| `/project/edit/[{id}/][{editAction}/]` | `PhabricatorProjectEditController` | partial | `/project/edit/{id}/`: the form, and its posts which change the name, icon, color or hashtags, written by the Rust editor with a web content source; description and policy changes, milestones, subprojects, archived projects, custom fields, spaces and configured forms 421 before writing; new projects go to PHP |
| `/project/{projectID}/item/view/{itemID}/` | `PhabricatorProjectMenuItemController` | not ported | profile menu items (custom menus; writes for configure/reorder/edit) |
| `/project/{projectID}/item/hide/{itemID}/` | `PhabricatorProjectMenuItemController` | not ported | profile menu items (custom menus; writes for configure/reorder/edit) |
| `/project/{projectID}/item/default/{itemID}/` | `PhabricatorProjectMenuItemController` | not ported | profile menu items (custom menus; writes for configure/reorder/edit) |
| `/project/{projectID}/item/configure/` | `PhabricatorProjectMenuItemController` | not ported | profile menu items (custom menus; writes for configure/reorder/edit) |
| `/project/{projectID}/item/configure/{global\|custom}/` | `PhabricatorProjectMenuItemController` | not ported | profile menu items (custom menus; writes for configure/reorder/edit) |
| `/project/{projectID}/item/reorder/{global\|custom}/` | `PhabricatorProjectMenuItemController` | not ported | profile menu items (custom menus; writes for configure/reorder/edit) |
| `/project/{projectID}/item/edit/[{id}/][{editAction}/]` | `PhabricatorProjectMenuItemController` | not ported | profile menu items (custom menus; writes for configure/reorder/edit) |
| `/project/{projectID}/item/new/{global\|custom}/{itemKey}/[{id}/][{editAction}/]` | `PhabricatorProjectMenuItemController` | not ported | profile menu items (custom menus; writes for configure/reorder/edit) |
| `/project/{projectID}/item/builtin/{itemID}/[{id}/][{editAction}/]` | `PhabricatorProjectMenuItemController` | not ported | profile menu items (custom menus; writes for configure/reorder/edit) |
| `/project/subprojects/{id}/` | `PhabricatorProjectSubprojectsController` | Rust | new: read-only; the actions link to PHP |
| `/project/update/{id}/{action}/` | `PhabricatorProjectUpdateController` | partial | joining and leaving (dialog and post), written by the Rust editor; parents, milestones, subprojects, archived projects, workflow requests and edits without effect 421 |
| `/project/manage/{id}/` | `PhabricatorProjectManageController` | not ported | read-only "Manage" page (history, details) |
| `/project/{watch\|unwatch}/{id}/` | `PhabricatorProjectWatchController` | partial | dialog and post, written by the Rust editor; subprojects, milestones, parents, archived projects, workflow requests and edits without effect 421 |
| `/project/silence/{id}/` | `PhabricatorProjectSilenceController` | not ported | write (form or action) |
| `/project/warning/{id}/` | `PhabricatorProjectSubprojectWarningController` | not ported | dialog before the first subproject |
| `/tag/{slug}/` | `PhabricatorProjectViewController` | Rust | project profile; other hashtags redirect |

### Search (`PhabricatorSearchApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/search/[query/{queryKey}/]` | `PhabricatorSearchController` | partial | the jumps of the search box (`f`, `d`, `r`, `a`, `D123`); fulltext searches go to PHP |
| `/search/hovercard/` | `PhabricatorSearchHovercardController` | not ported | hovercards of PHP pages |
| `/search/handle/{phid}/` | `PhabricatorSearchHandleController` | not ported | handle redirect; read-only, small |
| `/search/edit/key/{queryKey}/` | `PhabricatorSearchEditController` | not ported | write (form or action) |
| `/search/edit/id/{id}/` | `PhabricatorSearchEditController` | not ported | write (form or action) |
| `/search/default/{queryKey}/{engine}/` | `PhabricatorSearchDefaultController` | not ported | write (form or action) |
| `/search/delete/key/{queryKey}/{engine}/` | `PhabricatorSearchDeleteController` | not ported | write (form or action) |
| `/search/delete/id/{id}/` | `PhabricatorSearchDeleteController` | not ported | write (form or action) |
| `/search/order/{engine}/` | `PhabricatorSearchOrderController` | not ported | write (form or action) |
| `/search/rel/{relationshipKey}/{sourcePHID}/` | `PhabricatorSearchRelationshipController` | not ported | write: edit relationships (stack, parents) |
| `/search/source/{relationshipKey}/{sourcePHID}/` | `PhabricatorSearchRelationshipSourceController` | not ported | typeahead source of the relationship dialog |

### Settings (`PhabricatorSettingsApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/settings/[query/{queryKey}/[{queryAction}/]]` | `PhabricatorSettingsListController` | partial | `/settings/` only (redirect, or the list for admins) |
| `/settings/user/{username}/[page/{pageKey}/]` | `PhabricatorSettingsMainController` | partial | panels: datetime, language, diff, display, editor, email, emaildelivery, emailformat, emailpreferences, multifactor, apitokens, sessions; preference saves are written by Rust; the address actions (`?new`, `?delete`, `?verify`, `?primary`) write `PhabricatorUserEditor`'s rows (`user_email`, `user_log`, the `email.add` rate limit, the destruction log, revoked reset links, identity tasks) and the mail of `PhabricatorUserEmail` (`metamta_mail` and its task), behind the TOTP high security prompt where PHP requires one; non-TOTP factors, `auth.email-domains`, refused or taken addresses, `phabricator.production-uri`, unverified accounts making an address primary, removals other objects refer to, workflow dialogs, MFA enrolment, the other panels and administrators editing bots and mailing lists 421 |
| `/settings/builtin/global/[page/{pageKey}/]` | `PhabricatorSettingsMainController` | not ported | admin: the global default settings |
| `/settings/panel/{panel}/` | `PhabricatorSettingsMainController` | Rust | redirects to the user's panel |
| `/settings/adjust/` | `PhabricatorSettingsAdjustController` | not ported | write (form or action) |
| `/settings/timezone/{offset}/` | `PhabricatorSettingsTimezoneController` | not ported | write (form or action) |
| `/settings/issue/` | `PhabricatorSettingsIssueController` | not ported | setup issue dialog of PHP pages |

### Spaces (`PhabricatorSpacesApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/S{id}` | `PhabricatorSpacesViewController` | partial | space page; spaces with a history (their timeline) go to PHP |
| `/spaces/[query/{queryKey}/]` | `PhabricatorSpacesListController` | partial | builtin queries (Active, All); saved queries go to PHP |
| `/spaces/create/` | `PhabricatorSpacesEditController` | not ported | write (form or action) |
| `/spaces/edit/[{id}/]` | `PhabricatorSpacesEditController` | not ported | write (form or action) |
| `/spaces/{activate\|archive}/{id}/` | `PhabricatorSpacesArchiveController` | not ported | write (form or action) |

### Subscriptions (`PhabricatorSubscriptionsApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/subscriptions/{add\|delete}/{phid}/` | `PhabricatorSubscriptionsEditController` | partial | revisions only (`PHID-DREV-`), written by Rust; other objects stay with PHP |
| `/subscriptions/mute/{phid}/` | `PhabricatorSubscriptionsMuteController` | not ported | write: mute notifications |
| `/subscriptions/list/{phid}/` | `PhabricatorSubscriptionsListController` | partial | revisions, as a page; the dialog (Ajax) and other objects 421 |
| `/subscriptions/transaction/{add\|rem}/{phid}/` | `PhabricatorSubscriptionsTransactionController` | not ported | subscriber change dialog; read-only |

### Support (`PhabricatorSupportApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/help/keyboardshortcut/` | `PhabricatorHelpKeyboardShortcutController` | partial | as a page (`?keys=`); the dialogs PHP pages request go to PHP |

### System (`PhabricatorSystemApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/status/` | `PhabricatorStatusController` | not ported | load balancer health check; trivial, but keep it on PHP while PHP serves pages |
| `/debug/` | `PhabricatorDebugController` | not ported | debug page (off in production) |
| `/favicon*ico` | `PhabricatorFaviconController` | not ported | static |
| `/robots*txt` | `PhabricatorRobotsPlatformController` | not ported | static |
| `/services/encoding/` | `PhabricatorSystemSelectEncodingController` | not ported | dialog of PHP pages |
| `/services/highlight/` | `PhabricatorSystemSelectHighlightController` | not ported | dialog of PHP pages |
| `/services/viewas/` | `PhabricatorSystemSelectViewAsController` | not ported | dialog of PHP pages |
| `/readonly/{reason}/` | `PhabricatorSystemReadOnlyController` | not ported | read-only mode explanation; static |
| `/object/{name}/` | `PhabricatorSystemObjectController` | not ported | object name redirect (`/object/D1/`); read-only, small |
| `/robots*txt` | `PhabricatorRobotsResourceController` | not ported | static |

### Transactions (`PhabricatorTransactionsApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/transactions/edit/{phid}/` | `PhabricatorApplicationTransactionCommentEditController` | not ported | write (form or action) |
| `/transactions/remove/{phid}/` | `PhabricatorApplicationTransactionCommentRemoveController` | not ported | write (form or action) |
| `/transactions/history/{phid}/` | `PhabricatorApplicationTransactionCommentHistoryController` | partial | revision comments, as a page; the dialog (Ajax), removed comments and other objects 421 |
| `/transactions/quote/{phid}/` | `PhabricatorApplicationTransactionCommentQuoteController` | not ported | quote text for PHP comment forms |
| `/transactions/raw/{phid}/` | `PhabricatorApplicationTransactionCommentRawController` | not ported | raw comment; read-only |
| `/transactions/detail/{phid}/` | `PhabricatorApplicationTransactionDetailController` | partial | summary and test plan edits of revisions, as a page; the dialog (Ajax) and other transactions 421 |
| `/transactions/showolder/{phid}/` | `PhabricatorApplicationTransactionShowOlderController` | not ported | timeline paging of PHP pages |
| `/transactions/{old\|new}/{phid}/` | `PhabricatorApplicationTransactionValueController` | not ported | old/new values dialog; read-only |
| `/transactions/remarkuppreview/` | `PhabricatorApplicationTransactionRemarkupPreviewController` | not ported | Rust pages use `/rust-ui/preview` |
| `/transactions/editengine/[query/{queryKey}/[{queryAction}/]]` | `PhabricatorEditEngineListController` | not ported | admin: form configurations |
| `/transactions/editengine/{engineKey}/[query/{queryKey}/[{queryAction}/]]` | `PhabricatorEditEngineConfigurationListController` | not ported | admin: form configurations |
| `/transactions/editengine/{engineKey}/edit/[{id}/][{editAction}/]` | `PhabricatorEditEngineConfigurationEditController` | not ported | write (form or action) |
| `/transactions/editengine/{engineKey}/sort/{edit\|create}/` | `PhabricatorEditEngineConfigurationSortController` | not ported | write (form or action) |
| `/transactions/editengine/{engineKey}/view/{key}/` | `PhabricatorEditEngineConfigurationViewController` | not ported | admin: form configurations |
| `/transactions/editengine/{engineKey}/save/{key}/` | `PhabricatorEditEngineConfigurationSaveController` | not ported | write (form or action) |
| `/transactions/editengine/{engineKey}/reorder/{key}/` | `PhabricatorEditEngineConfigurationReorderController` | not ported | write (form or action) |
| `/transactions/editengine/{engineKey}/defaults/{key}/` | `PhabricatorEditEngineConfigurationDefaultsController` | not ported | write (form or action) |
| `/transactions/editengine/{engineKey}/lock/{key}/` | `PhabricatorEditEngineConfigurationLockController` | not ported | write (form or action) |
| `/transactions/editengine/{engineKey}/subtype/{key}/` | `PhabricatorEditEngineConfigurationSubtypeController` | not ported | write (form or action) |
| `/transactions/editengine/{engineKey}/defaultcreate/{key}/` | `PhabricatorEditEngineConfigurationDefaultCreateController` | not ported | write (form or action) |
| `/transactions/editengine/{engineKey}/defaultedit/{key}/` | `PhabricatorEditEngineConfigurationIsEditController` | not ported | write (form or action) |
| `/transactions/editengine/{engineKey}/disable/{key}/` | `PhabricatorEditEngineConfigurationDisableController` | not ported | write (form or action) |

### Typeahead (`PhabricatorTypeaheadApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/typeahead/{browse\|class}/[{class}/]` | `PhabricatorTypeaheadModularDatasourceController` | partial | `class/` of users, projects and (blocking) reviewers, byte for byte; browsing, functions, away users and other datasources 421 |
| `/typeahead/help/{class}/` | `PhabricatorTypeaheadFunctionHelpController` | not ported | help page of typeahead functions |

### ReviewHelper (`PhabricatorReviewHelperApplication`)

| Route | Controller | Port | Note |
|---|---|---|---|
| `/reviewhelper/request/{revisionID}/` | `ReviewHelperRequestController` | not ported | write: asks the review helper service about a revision |
| `/reviewhelper/feedback/` | `ReviewHelperFeedbackController` | not ported | write: review helper feedback |

## Routing what is ported

Read-only methods ported but not routed, to add to the first `location` of `rust/nginx/rust-routes.conf` *and* to `PHAB_SHADOW_METHODS` (AGENTS.md, "Rolling out a Conduit method"): `feed.query`, `diffusion.internal.ancestors`. The other "ported, not routed" methods write, and wait for their parity runs.

Rebuild this file's tables when routes or methods change: the counts come from the code, not from usage. Usage counts (nginx logs, `conduit_methodcalllog`, RUST_PORT_PLAN.md §2) should order what is ported next.
