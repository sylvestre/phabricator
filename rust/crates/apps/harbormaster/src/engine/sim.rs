//! `HarbormasterBuildEngine` over the rows of one build or buildable, in
//! memory: each step reads the state the previous ones left, as PHP's
//! queries would read the rows it saved, and records what PHP writes as
//! [`Op`]s.

use std::collections::{BTreeSet, HashMap};

use phab_conduit::Context;
use phab_core::{Phid, PhidType, random};
use phab_db::Database;
use serde_json::{Map, Value, json};

use super::lock::GlobalLock;
use super::rows::{
    self, ArtifactRow, BuildRow, BuildableRow, DiffRow, MessageRow, StepRow, TargetRow,
};
use super::write::{self, Op, TaskData};
use super::{EngineError, HARBORMASTER_APP, Mode, TARGET_WORKER, unsupported};
use crate::storage::Plan;

/// `HarbormasterBuildStatus` constants the engine sets.
pub(super) mod status {
    pub(crate) const PENDING: &str = "pending";
    pub(crate) const BUILDING: &str = "building";
    pub(crate) const PASSED: &str = "passed";
    pub(crate) const FAILED: &str = "failed";
    pub(crate) const ABORTED: &str = "aborted";
    pub(crate) const PAUSED: &str = "paused";
    pub(crate) const DEADLOCKED: &str = "deadlocked";
}

/// `HarbormasterBuildTarget` statuses.
pub(super) mod target_status {
    pub(crate) const PENDING: &str = "target/pending";
    pub(crate) const BUILDING: &str = "target/building";
    pub(crate) const WAITING: &str = "target/waiting";
    pub(crate) const PASSED: &str = "target/passed";
    pub(crate) const FAILED: &str = "target/failed";
    pub(crate) const ABORTED: &str = "target/aborted";
}

/// `HarbormasterBuildableStatus` constants.
mod buildable_status {
    pub(crate) const PREPARING: &str = "preparing";
    pub(crate) const BUILDING: &str = "building";
    pub(crate) const PASSED: &str = "passed";
    pub(crate) const FAILED: &str = "failed";
}

/// The content source of the engine's edits (`PhabricatorDaemonContentSource`).
const DAEMON_SOURCE: &str = r#"{"source":"daemon","params":[]}"#;

/// `PhabricatorApplicationTransactionPublishWorker`.
const PUBLISH_WORKER: &str = "PhabricatorApplicationTransactionPublishWorker";

/// `PhabricatorWorker::PRIORITY_ALERTS` and `PRIORITY_DEFAULT`.
const PRIORITY_ALERTS: i64 = 1000;
pub(super) const PRIORITY_DEFAULT: i64 = 2000;

/// The step implementations installed (`getImplementations()`).
const IMPLEMENTATIONS: &[&str] = &[
    "HarbormasterAbortOlderBuildsBuildStepImplementation",
    "HarbormasterArcLintBuildStepImplementation",
    "HarbormasterArcUnitBuildStepImplementation",
    "HarbormasterHTTPRequestBuildStepImplementation",
    "HarbormasterWaitForPreviousBuildStepImplementation",
];

/// `HarbormasterBuildStatus` `isBuilding`.
pub(super) fn is_building(status: &str) -> bool {
    matches!(status, status::PENDING | status::BUILDING)
}

/// `HarbormasterBuildStatus` `isComplete`.
fn is_complete(status: &str) -> bool {
    matches!(
        status,
        status::PASSED | status::FAILED | status::ABORTED | "error" | status::DEADLOCKED
    )
}

/// `HarbormasterBuildTarget::isComplete()`.
pub(super) fn target_is_complete(status: &str) -> bool {
    matches!(
        status,
        target_status::PASSED | target_status::FAILED | target_status::ABORTED
    )
}

/// `HarbormasterBuildTarget::isFailed()`.
fn target_is_failed(status: &str) -> bool {
    matches!(status, target_status::FAILED | target_status::ABORTED)
}

/// `HarbormasterBuildTarget::isUnderway()`.
fn target_is_underway(status: &str) -> bool {
    matches!(status, target_status::PENDING | target_status::BUILDING)
}

/// The option of a plan behavior.
fn behavior(plan: &Plan, key: &str) -> Result<&'static str, EngineError> {
    crate::plan::behavior(plan, key).map_err(|_| unsupported("unusual plan behaviors"))
}

/// `getUnprocessedMessageState()`: the pending status the commands give
/// the build, and the one command applied (the last of the strongest).
fn message_state(messages: &[MessageRow]) -> (Option<&'static str>, Option<&MessageRow>) {
    let (mut restarting, mut aborting, mut pausing, mut resuming) = (false, false, false, false);
    let mut apply = None;
    for message in messages {
        match message.kind.as_str() {
            "restart" => {
                restarting = true;
                aborting = false;
                apply = Some(message);
            }
            "abort" => {
                aborting = true;
                restarting = false;
                apply = Some(message);
            }
            "pause" => {
                pausing = true;
                resuming = false;
                apply = Some(message);
            }
            "resume" => {
                resuming = true;
                pausing = false;
                apply = Some(message);
            }
            _ => {}
        }
    }
    let pending = if restarting {
        Some("x-restarting")
    } else if aborting {
        Some("x-aborting")
    } else if pausing {
        Some("x-pausing")
    } else if resuming {
        Some("x-resuming")
    } else {
        None
    };
    (pending, apply)
}

/// The state of one run of the engine.
#[derive(Debug, Clone)]
pub(super) struct Sim {
    pub(super) now: i64,
    pub(super) buildable: BuildableRow,
    /// The container the buildable had when its build was loaded.
    loaded_container: Option<String>,
    pub(super) diff: DiffRow,
    /// Every build of the buildable, newest first, as the engine left it.
    pub(super) builds: Vec<BuildRow>,
    plans: HashMap<String, Plan>,
    /// The unconsumed messages to the builds, targets and buildable, oldest
    /// first, with those the engine consumed marked.
    messages: Vec<MessageRow>,
    /// The build the engine continues, in `builds`.
    current: Option<usize>,
    steps: Vec<StepRow>,
    /// The targets of the current build, every generation, newest first;
    /// those the engine creates are added with ID 0.
    pub(super) targets: Vec<TargetRow>,
    artifacts: Vec<ArtifactRow>,
    release_queue: Vec<usize>,
    /// The new targets: their PHID, and which [`Op::NewTarget`] they are.
    new_targets: Vec<(String, usize)>,
    /// `DifferentialDiff::getHarbormasterContainerPHID()`.
    diff_container: Option<String>,
    saved_header: Option<String>,
    pub(super) ops: Vec<Op>,
}

/// The rows of a buildable whose object loads, and of its builds.
async fn load_buildable(
    database: &Database,
    buildable: BuildableRow,
    now: i64,
) -> Result<Option<Sim>, EngineError> {
    let Some(diff) = rows::buildable_object(database, &buildable.object_phid).await? else {
        return Ok(None);
    };
    let builds = rows::builds_of(database, &buildable.phid).await?;
    let mut plan_phids: Vec<String> = builds.iter().map(|build| build.plan_phid.clone()).collect();
    plan_phids.sort();
    plan_phids.dedup();
    let plans = rows::plans(database, &plan_phids)
        .await?
        .into_iter()
        .map(|plan| (plan.phid.as_str().to_owned(), plan))
        .collect();
    let messages =
        rows::unconsumed_messages(database, std::slice::from_ref(&buildable.phid)).await?;
    let diff_container = rows::diff_container(database, &buildable.object_phid).await?;
    Ok(Some(Sim {
        now,
        loaded_container: buildable.container_phid.clone(),
        buildable,
        diff,
        builds,
        plans,
        messages,
        current: None,
        steps: Vec::new(),
        targets: Vec::new(),
        artifacts: Vec::new(),
        release_queue: Vec::new(),
        new_targets: Vec::new(),
        diff_container,
        saved_header: None,
        ops: Vec::new(),
    }))
}

impl Sim {
    /// The rows of the build `build`, as `HarbormasterBuildQuery` loads it
    /// for the omnipotent viewer: `None` when it doesn't load (its
    /// buildable or the buildable's object is gone).
    pub(super) async fn load_build(
        database: &Database,
        build: &BuildRow,
        now: i64,
    ) -> Result<Option<Self>, EngineError> {
        let Some(buildable) = rows::buildable_by_phid(database, &build.buildable_phid).await?
        else {
            return Ok(None);
        };
        let Some(mut sim) = load_buildable(database, buildable, now).await? else {
            return Ok(None);
        };
        let current = sim
            .builds
            .iter()
            .position(|row| row.id == build.id)
            .ok_or_else(|| EngineError::Failed("a build left its buildable".to_owned()))?;
        sim.current = Some(current);
        if sim.plans.contains_key(&build.plan_phid) {
            sim.steps = rows::steps_of(database, &build.plan_phid).await?;
        }
        sim.targets = rows::targets_of(database, &build.phid).await?;
        let target_phids: Vec<String> = sim
            .targets
            .iter()
            .map(|target| target.phid.clone())
            .collect();
        sim.artifacts = rows::artifacts_of(database, &target_phids).await?;
        let mut receivers = target_phids;
        receivers.push(build.phid.clone());
        let mut messages = rows::unconsumed_messages(database, &receivers).await?;
        messages.append(&mut sim.messages);
        messages.sort_by_key(|message| message.id);
        sim.messages = messages;
        sim.saved_header = rows::saved_header(database, &build.phid).await?;
        if !rows::json_is_stable(&build.parameters) {
            return Err(unsupported(
                "builds whose parameters Lisk would encode differently",
            ));
        }
        Ok(Some(sim))
    }

    fn index(&self) -> Result<usize, EngineError> {
        self.current
            .ok_or_else(|| EngineError::Failed("no build to continue".to_owned()))
    }

    /// The build being continued.
    pub(super) fn build(&self) -> &BuildRow {
        &self.builds[self.current.unwrap_or_default()]
    }

    fn build_mut(&mut self) -> &mut BuildRow {
        let index = self.current.unwrap_or_default();
        &mut self.builds[index]
    }

    /// `$build->save()`.
    fn save_build(&mut self) {
        let build = self.build();
        self.ops.push(Op::Build {
            id: build.id,
            status: build.status.clone(),
            generation: build.generation,
        });
    }

    /// `$target->save()` of an existing target.
    pub(super) fn save_target(&mut self, index: usize) -> Result<(), EngineError> {
        let target = &self.targets[index];
        if !rows::json_is_stable(&target.details) || !rows::json_is_stable(&target.variables) {
            return Err(unsupported(
                "targets whose JSON Lisk would encode differently",
            ));
        }
        self.ops.push(Op::Target {
            id: target.id,
            status: target.status.clone(),
            started: target.started,
            completed: target.completed,
        });
        Ok(())
    }

    /// `$message->setIsConsumed(true)->save()`.
    fn consume(&mut self, id: i64) {
        if let Some(message) = self.messages.iter_mut().find(|message| message.id == id) {
            message.consumed = true;
        }
        self.ops.push(Op::Message { id });
    }

    /// `HarbormasterBuildArtifact::releaseArtifact()`: no installed
    /// artifact type does anything more than mark it released.
    fn release(&mut self, index: usize) -> Result<(), EngineError> {
        let artifact = &mut self.artifacts[index];
        if artifact.released {
            return Ok(());
        }
        if !rows::json_is_stable(&artifact.data) {
            return Err(unsupported(
                "artifacts whose data Lisk would encode differently",
            ));
        }
        artifact.released = true;
        let id = artifact.id;
        self.ops.push(Op::Artifact { id });
        Ok(())
    }

    /// The targets of the current build's generation, newest first.
    fn current_targets(&self) -> Vec<usize> {
        let generation = self.build().generation;
        (0..self.targets.len())
            .filter(|index| self.targets[*index].generation == generation)
            .collect()
    }

    /// The unreleased artifacts of these targets, newest first.
    fn unreleased_artifacts(&self, targets: &[usize]) -> Vec<usize> {
        let phids: BTreeSet<&str> = targets
            .iter()
            .map(|index| self.targets[*index].phid.as_str())
            .collect();
        (0..self.artifacts.len())
            .filter(|index| {
                let artifact = &self.artifacts[*index];
                !artifact.released && phids.contains(artifact.target_phid.as_str())
            })
            .collect()
    }

    /// `HarbormasterBuild::releaseAllArtifacts()`.
    fn release_all(&mut self) -> Result<(), EngineError> {
        let targets = self.current_targets();
        for index in self.unreleased_artifacts(&targets) {
            self.release(index)?;
        }
        Ok(())
    }

    /// `HarbormasterBuild::restartBuild()`.
    fn restart_build(&mut self) -> Result<(), EngineError> {
        self.release_all()?;
        self.build_mut().generation += 1;
        Ok(())
    }

    /// `HarbormasterBuildEngine::continueBuild()`, once the build is
    /// locked and reloaded.
    pub(super) fn continue_build(&mut self) -> Result<(), EngineError> {
        self.index()?;
        let old_status = self.build().status.clone();
        self.update_build()?;
        // Queued once the lock is released.
        for (phid, position) in std::mem::take(&mut self.new_targets) {
            self.ops.push(Op::Task {
                class: TARGET_WORKER,
                data: TaskData::Target(position),
                priority: PRIORITY_DEFAULT,
                object_phid: Some(phid),
            });
        }
        if self.build().status != old_status {
            self.update_buildable()?;
        }
        for index in std::mem::take(&mut self.release_queue) {
            self.release(index)?;
        }
        if !is_building(&self.build().status) {
            self.release_all()?;
        }
        Ok(())
    }

    /// `updateBuild()`.
    fn update_build(&mut self) -> Result<(), EngineError> {
        let build_phid = self.build().phid.clone();
        // The messages the build query attached.
        let unprocessed: Vec<MessageRow> = self
            .messages
            .iter()
            .filter(|message| message.receiver == build_phid && !message.consumed)
            .cloned()
            .collect();
        let (pending, apply) = message_state(&unprocessed);
        match apply.cloned() {
            None => {
                if self.build().status == status::PENDING {
                    self.restart_build()?;
                    status::BUILDING.clone_into(&mut self.build_mut().status);
                    self.save_build();
                }
            }
            Some(message) => {
                self.apply_command(&message, pending)?;
                for message in &unprocessed {
                    self.consume(message.id);
                }
            }
        }
        if self.build().status == status::BUILDING {
            self.update_build_steps()?;
        }
        Ok(())
    }

    /// `canApplyMessage()` of the command's transaction type
    /// (`getTransactionHasEffect()`).
    fn can_apply(&self, kind: &str, pending: Option<&str>) -> Result<bool, EngineError> {
        let build = self.build();
        if build.plan_auto_key.is_some() {
            return Ok(false);
        }
        Ok(match kind {
            "abort" => !is_complete(&build.status),
            "pause" => build.status != status::PAUSED && !is_complete(&build.status),
            "restart" => {
                let Some(plan) = self.plans.get(&build.plan_phid) else {
                    return Ok(false);
                };
                match behavior(plan, "restartable")? {
                    "never" => false,
                    // `$this->isFailed()` on the transaction type: an Error.
                    "failed" => {
                        return Err(unsupported("restarts of plans restartable only if failed"));
                    }
                    _ => true,
                }
            }
            "resume" => build.status == status::PAUSED || pending == Some("x-pausing"),
            _ => false,
        })
    }

    /// `HarbormasterBuildTransactionEditor::applyTransactions()` with the
    /// `message/<kind>` transaction of `message`, as the omnipotent viewer
    /// acting as Harbormaster.
    fn apply_command(
        &mut self,
        message: &MessageRow,
        pending: Option<&str>,
    ) -> Result<(), EngineError> {
        let kind = message.kind.as_str();
        let effect = self.can_apply(kind, pending)?;
        if effect {
            match kind {
                "abort" => status::ABORTED.clone_into(&mut self.build_mut().status),
                "pause" => status::PAUSED.clone_into(&mut self.build_mut().status),
                "restart" => {
                    self.restart_build()?;
                    status::BUILDING.clone_into(&mut self.build_mut().status);
                }
                _ => status::BUILDING.clone_into(&mut self.build_mut().status),
            }
        }
        // Saved even when the command has no effect.
        self.save_build();
        let mut xaction_phids = Vec::new();
        if effect {
            let phid =
                Phid::generate(PhidType::TRANSACTION, Some(PhidType::new(*b"HMBD"))).into_string();
            let author = message
                .author
                .clone()
                .filter(|phid| phab_core::php::is_truthy_str(phid))
                .unwrap_or_else(|| HARBORMASTER_APP.to_owned());
            self.ops.push(Op::Transaction {
                phid: phid.clone(),
                author,
                object: self.build().phid.clone(),
                kind: format!("message/{kind}"),
                new_value: phab_core::php::json_encode(&json!(kind)),
                metadata: phab_core::php::json_encode(
                    &json!({"core.groupID": random::random_characters(32)}),
                ),
                content_source: DAEMON_SOURCE,
            });
            xaction_phids.push(phid);
            if kind == "abort" {
                self.release_all()?;
            }
        }
        let build_phid = self.build().phid.clone();
        let data = json!({
            "objectPHID": build_phid,
            "actorPHID": HARBORMASTER_APP,
            "xactionPHIDs": xaction_phids,
            "state": {
                "parentMessageID": null,
                "isNewObject": false,
                "heraldEmailPHIDs": [],
                "heraldForcedEmailPHIDs": [],
                "heraldHeader": self.saved_header,
                "mailToPHIDs": [],
                "mailCCPHIDs": [],
                "feedNotifyPHIDs": [],
                "feedRelatedPHIDs": [],
                "feedShouldPublish": false,
                "mailShouldSend": false,
                "mustEncrypt": [],
                "mailStamps": [],
                "mailUnexpandablePHIDs": [],
                "mailMutedPHIDs": [],
                "webhookMap": [],
                "silent": null,
                "sendHistory": false,
                "excludeMailRecipientPHIDs": [],
                "custom": [],
                "custom.encoding": [],
            },
        });
        self.ops.push(Op::Task {
            class: PUBLISH_WORKER,
            data: TaskData::Json(phab_core::php::json_encode(&data)),
            priority: PRIORITY_ALERTS,
            object_phid: Some(build_phid),
        });
        Ok(())
    }

    /// `getDependencies()`: the `dependsOn` detail (no installed step has
    /// artifact inputs).
    fn dependencies(step: &StepRow) -> Result<Vec<String>, EngineError> {
        let odd = || unsupported("steps with unusual dependencies");
        let Value::Object(details) = &step.details else {
            if matches!(&step.details, Value::Array(items) if items.is_empty()) {
                return Ok(Vec::new());
            }
            return Err(odd());
        };
        let values: Vec<&Value> = match details.get("dependsOn") {
            None => return Ok(Vec::new()),
            Some(Value::Array(items)) => items.iter().collect(),
            Some(Value::Object(map)) => map.values().collect(),
            Some(_) => return Err(odd()),
        };
        values
            .into_iter()
            .map(|value| value.as_str().map(str::to_owned).ok_or_else(odd))
            .collect()
    }

    /// `updateBuildSteps()`.
    fn update_build_steps(&mut self) -> Result<(), EngineError> {
        let all_targets = self.current_targets();
        self.update_waiting_targets(&all_targets)?;
        if !self.plans.contains_key(&self.build().plan_phid) {
            return Err(unsupported("builds whose plan is gone"));
        }
        let steps = self.steps.clone();
        if let Some(step) = steps
            .iter()
            .find(|step| !IMPLEMENTATIONS.contains(&step.class_name.as_str()))
        {
            return Err(unsupported(format!(
                "steps of the implementation {}",
                step.class_name
            )));
        }
        let mut queued = BTreeSet::new();
        let mut underway = BTreeSet::new();
        let mut waiting = BTreeSet::new();
        let mut complete = BTreeSet::new();
        let mut failed = BTreeSet::new();
        for step in &steps {
            let step_targets: Vec<&TargetRow> = all_targets
                .iter()
                .map(|index| &self.targets[*index])
                .filter(|target| target.step_phid == step.phid)
                .collect();
            let phid = step.phid.clone();
            if step_targets.is_empty() {
                queued.insert(phid);
                continue;
            }
            if step_targets
                .iter()
                .any(|target| target_is_underway(&target.status))
            {
                underway.insert(phid.clone());
            }
            if step_targets
                .iter()
                .any(|target| target.status == target_status::WAITING)
            {
                waiting.insert(phid.clone());
            }
            if step_targets
                .iter()
                .all(|target| target_is_complete(&target.status))
            {
                complete.insert(phid.clone());
            }
            if step_targets
                .iter()
                .any(|target| target_is_failed(&target.status))
            {
                failed.insert(phid);
            }
        }
        if !failed.is_empty() {
            status::FAILED.clone_into(&mut self.build_mut().status);
            self.save_build();
            return Ok(());
        }
        // `count($complete) == count($steps)`: steps are keyed by PHID.
        let step_count = steps
            .iter()
            .map(|step| step.phid.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        if complete.len() == step_count {
            status::PASSED.clone_into(&mut self.build_mut().status);
            self.save_build();
            return Ok(());
        }
        let ongoing = queued.len() + waiting.len() + underway.len();
        if !all_targets.is_empty() && ongoing > 0 {
            // No installed step takes artifacts: every unreleased artifact
            // of the build is released.
            let unreleased = self.unreleased_artifacts(&all_targets);
            self.release_queue.extend(unreleased);
        }
        let mut runnable = Vec::new();
        for step in &steps {
            let dependencies = Self::dependencies(step)?;
            if queued.contains(&step.phid)
                && dependencies
                    .iter()
                    .all(|dependency| complete.contains(dependency))
            {
                runnable.push(step.clone());
            }
        }
        if runnable.is_empty() && waiting.is_empty() && underway.is_empty() {
            status::DEADLOCKED.clone_into(&mut self.build_mut().status);
            self.save_build();
            return Ok(());
        }
        for step in runnable {
            self.new_target(&step)?;
        }
        Ok(())
    }

    /// `updateWaitingTargets()`: the `pass` and `fail` messages sent to
    /// waiting targets, newest first (the oldest wins).
    fn update_waiting_targets(&mut self, targets: &[usize]) -> Result<(), EngineError> {
        let waiting: HashMap<String, usize> = targets
            .iter()
            .filter(|index| self.targets[**index].status == target_status::WAITING)
            .map(|index| (self.targets[*index].phid.clone(), *index))
            .collect();
        if waiting.is_empty() {
            return Ok(());
        }
        let messages: Vec<MessageRow> = self
            .messages
            .iter()
            .rev()
            .filter(|message| !message.consumed && waiting.contains_key(&message.receiver))
            .cloned()
            .collect();
        for message in messages {
            let new_status = match message.kind.as_str() {
                "pass" => target_status::PASSED,
                "fail" => target_status::FAILED,
                _ => continue,
            };
            self.consume(message.id);
            let index = waiting[&message.receiver];
            let now = self.now;
            let target = &mut self.targets[index];
            new_status.clone_into(&mut target.status);
            if target_is_complete(new_status) {
                target.completed = Some(now);
            }
            self.save_target(index)?;
        }
        Ok(())
    }

    /// `HarbormasterBuild::retrieveVariablesFromBuild()`.
    fn variables(&self) -> Result<Map<String, Value>, EngineError> {
        let build = self.build();
        let mut variables = Map::new();
        // `DifferentialDiff::getBuildVariables()`: IDs as Lisk loads them.
        variables.insert("buildable.diff".to_owned(), json!(self.diff.id.to_string()));
        if let Some(revision) = &self.diff.revision {
            variables.insert(
                "buildable.revision".to_owned(),
                json!(revision.id.to_string()),
            );
            if revision.repository_phid.is_some() {
                return Err(unsupported(
                    "targets of revisions with a repository (clone URI variables)",
                ));
            }
        }
        for key in [
            "buildable.diff",
            "buildable.revision",
            "buildable.commit",
            "repository.callsign",
            "repository.phid",
            "repository.vcs",
            "repository.uri",
            "step.timestamp",
            "build.id",
            "initiator.phid",
            "buildable.phid",
            "buildable.object.phid",
            "buildable.container.phid",
            "build.phid",
        ] {
            variables.entry(key.to_owned()).or_insert(Value::Null);
        }
        let parameters: Value = serde_json::from_str(&build.parameters).unwrap_or(Value::Null);
        let parameters: Vec<(String, Value)> = match parameters {
            Value::Object(map) => map.into_iter().collect(),
            Value::Array(items) => items
                .into_iter()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value))
                .collect(),
            _ => return Err(unsupported("builds whose parameters are not a dictionary")),
        };
        for (key, value) in parameters {
            variables.entry(format!("build/{key}")).or_insert(value);
        }
        variables.insert("step.timestamp".to_owned(), json!(self.now));
        variables.insert("build.id".to_owned(), json!(build.id.to_string()));
        variables.insert("initiator.phid".to_owned(), json!(build.initiator));
        variables.insert("buildable.phid".to_owned(), json!(self.buildable.phid));
        variables.insert("buildable.object.phid".to_owned(), json!(self.diff.phid));
        variables.insert(
            "buildable.container.phid".to_owned(),
            json!(self.loaded_container),
        );
        variables.insert("build.phid".to_owned(), json!(build.phid));
        Ok(variables)
    }

    /// `HarbormasterBuildTarget::initializeNewBuildTarget()` and `save()`.
    fn new_target(&mut self, step: &StepRow) -> Result<(), EngineError> {
        let variables = self.variables()?;
        let details = rows::lisk_json(&step.details)
            .ok_or_else(|| unsupported("steps whose details Lisk would encode differently"))?;
        let variables = rows::lisk_json(&Value::Object(variables))
            .ok_or_else(|| unsupported("variables Lisk would encode differently"))?;
        let build = self.build();
        let phid = Phid::generate(PhidType::new(*b"HMBT"), None).into_string();
        let target = TargetRow {
            id: 0,
            phid: phid.clone(),
            build_phid: build.phid.clone(),
            step_phid: step.phid.clone(),
            class_name: step.class_name.clone(),
            details,
            variables,
            status: target_status::PENDING.to_owned(),
            started: None,
            completed: None,
            generation: build.generation,
        };
        let position = self
            .ops
            .iter()
            .filter(|op| matches!(op, Op::NewTarget { .. }))
            .count();
        self.ops.push(Op::NewTarget {
            name: step.name.clone(),
            target: target.clone(),
        });
        // Last rather than first: nothing reads the order of new targets.
        self.targets.push(target);
        self.new_targets.push((phid, position));
        Ok(())
    }

    /// `updateBuildable()`, with the buildable locked.
    pub(super) fn update_buildable(&mut self) -> Result<(), EngineError> {
        let buildable_phid = self.buildable.phid.clone();
        let messages: Vec<MessageRow> = self
            .messages
            .iter()
            .rev()
            .filter(|message| message.receiver == buildable_phid && !message.consumed)
            .cloned()
            .collect();
        let mut done_preparing = false;
        let mut update_container = false;
        for message in &messages {
            match message.kind.as_str() {
                "build" => done_preparing = true,
                "container" => update_container = true,
                _ => {}
            }
            self.consume(message.id);
        }
        if done_preparing && self.buildable.status == buildable_status::PREPARING {
            buildable_status::BUILDING.clone_into(&mut self.buildable.status);
            self.save_buildable();
        }
        if update_container {
            // `getHarbormasterContainerPHID()`, read before planning.
            self.buildable.container_phid = self.diff_container.clone();
            self.save_buildable();
        }
        if self.buildable.status != buildable_status::PREPARING {
            let mut all_pass = true;
            let mut any_fail = false;
            for build in &self.builds {
                let plan = self
                    .plans
                    .get(&build.plan_phid)
                    .ok_or_else(|| unsupported("buildables with builds whose plan is gone"))?;
                let option = behavior(plan, "buildable")?;
                if option == "never" || (option == "building" && is_complete(&build.status)) {
                    continue;
                }
                if build.status != status::PASSED {
                    all_pass = false;
                }
                if is_complete(&build.status) && build.status != status::PASSED {
                    any_fail = true;
                }
            }
            let new_status = if any_fail {
                buildable_status::FAILED
            } else if all_pass {
                buildable_status::PASSED
            } else {
                buildable_status::BUILDING
            };
            if self.buildable.status != new_status {
                new_status.clone_into(&mut self.buildable.status);
                self.save_buildable();
            }
        }
        if self.buildable.status == buildable_status::PREPARING {
            return Ok(());
        }
        self.publish_buildable()
    }

    fn save_buildable(&mut self) {
        self.ops.push(Op::Buildable {
            id: self.buildable.id,
            status: self.buildable.status.clone(),
            container: self.buildable.container_phid.clone(),
        });
    }

    /// `DifferentialRevision::newBuildableStatus()`: the status of the
    /// builds which hold drafts, ignoring autobuilds.
    fn revision_buildable_status(&self) -> Option<&'static str> {
        let mut builds = Vec::new();
        for build in &self.builds {
            if build.plan_auto_key.is_some()
                || !matches!(
                    build.status.as_str(),
                    "inactive"
                        | status::PENDING
                        | status::BUILDING
                        | status::FAILED
                        | status::ABORTED
                        | "error"
                        | status::PAUSED
                        | status::DEADLOCKED
                )
            {
                continue;
            }
            // A plan which doesn't load has the generic options.
            let hold = self
                .plans
                .get(&build.plan_phid)
                .map(|plan| behavior(plan, "hold-drafts").unwrap_or("always"));
            let ignored = match hold {
                Some("never") => true,
                Some("building") => is_complete(&build.status),
                _ => false,
            };
            if !ignored {
                builds.push(build);
            }
        }
        if builds.is_empty() {
            return Some(buildable_status::PASSED);
        }
        if builds.iter().any(|build| is_complete(&build.status)) {
            return Some(buildable_status::FAILED);
        }
        None
    }

    /// `publishBuildable()` with `DifferentialBuildableEngine`: nothing to
    /// do for diffs without a revision, manual buildables, buildables still
    /// building, and when the revision's buildable status stays the same;
    /// otherwise the revision's editor sets it ([`Op::Publish`]).
    fn publish_buildable(&mut self) -> Result<(), EngineError> {
        let Some(revision) = &self.diff.revision else {
            return Ok(());
        };
        if self.buildable.is_manual || self.buildable.status == buildable_status::BUILDING {
            return Ok(());
        }
        let buildables = match &revision.properties {
            Value::Object(properties) => properties.get("buildables"),
            Value::Array(items) if items.is_empty() => None,
            _ => return Err(unsupported("revisions with unusual properties")),
        };
        let old = match buildables {
            Some(Value::Object(map)) => match map.get(&self.buildable.phid) {
                Some(Value::Object(entry)) => entry.get("status"),
                _ => None,
            },
            _ => None,
        };
        let new = self.revision_buildable_status();
        let same = match (old, new) {
            (None | Some(Value::Null), None) => true,
            (Some(Value::String(old)), Some(new)) => old == new,
            _ => false,
        };
        if !same {
            self.ops.push(Op::Publish {
                revision_id: revision.id,
                buildable_phid: self.buildable.phid.clone(),
                status: new,
            });
        }
        Ok(())
    }
}

/// `HarbormasterBuildWorker` for a build: `loadBuild()`, then
/// `continueBuild()` under the build's lock (and the buildable's, which
/// `updateBuildable()` takes).
pub(super) async fn continue_build_task(
    context: &Context,
    (id, display): (i64, String),
    now: i64,
    mode: Mode,
) -> Result<(), EngineError> {
    let database = &context.database;
    let invalid = || EngineError::Permanent(format!("Invalid build ID \"{display}\"."));
    let build = rows::build_by_id(database, id).await?.ok_or_else(invalid)?;
    let mut sim = Sim::load_build(database, &build, now)
        .await?
        .ok_or_else(invalid)?;
    if mode == Mode::Inspect {
        sim.continue_build()?;
        return write::check(context, &sim.ops, now).await;
    }
    let build_lock = GlobalLock::lock(database, &format!("harbormaster.build:{id}")).await?;
    let buildable_lock = match GlobalLock::lock(
        database,
        &format!("harbormaster.buildable:{}", sim.buildable.id),
    )
    .await
    {
        Ok(lock) => lock,
        Err(error) => {
            build_lock.unlock().await?;
            return Err(error);
        }
    };
    let result = async {
        // `$build->reload()`, with what the query attached read again.
        let build = rows::build_by_id(database, id).await?.ok_or_else(invalid)?;
        sim = Sim::load_build(database, &build, now)
            .await?
            .ok_or_else(invalid)?;
        sim.continue_build()?;
        write::apply(context, &sim.ops, now).await
    }
    .await;
    buildable_lock.unlock().await?;
    build_lock.unlock().await?;
    result
}

/// `HarbormasterBuildWorker` for a buildable: `loadBuildable()`, then
/// `updateBuildable()` under its lock.
pub(super) async fn update_buildable_task(
    context: &Context,
    phid: &str,
    now: i64,
    mode: Mode,
) -> Result<(), EngineError> {
    let database = &context.database;
    let invalid = || EngineError::Permanent(format!("Invalid buildable PHID \"{phid}\"."));
    let load = || async {
        let buildable = rows::buildable_by_phid(database, phid)
            .await?
            .ok_or_else(invalid)?;
        load_buildable(database, buildable, now)
            .await?
            .ok_or_else(invalid)
    };
    let mut sim = load().await?;
    if mode == Mode::Inspect {
        sim.update_buildable()?;
        return write::check(context, &sim.ops, now).await;
    }
    let lock = GlobalLock::lock(
        database,
        &format!("harbormaster.buildable:{}", sim.buildable.id),
    )
    .await?;
    let result = async {
        sim = load().await?;
        sim.update_buildable()?;
        write::apply(context, &sim.ops, now).await
    }
    .await;
    lock.unlock().await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: i64, kind: &str) -> MessageRow {
        MessageRow {
            id,
            author: None,
            receiver: "PHID-HMBD-1".to_owned(),
            kind: kind.to_owned(),
            consumed: false,
        }
    }

    #[test]
    fn keeps_the_strongest_command() {
        let messages = [message(1, "pause"), message(2, "abort"), message(3, "pass")];
        let (pending, apply) = message_state(&messages);
        assert_eq!(pending, Some("x-aborting"));
        assert_eq!(apply.map(|message| message.id), Some(2));
        let messages = [message(1, "pause"), message(2, "resume")];
        let (pending, apply) = message_state(&messages);
        assert_eq!(pending, Some("x-resuming"));
        assert_eq!(apply.map(|message| message.id), Some(2));
        assert_eq!(message_state(&[message(1, "work")]), (None, None));
    }
}
