//! Runs the task classes ported to Rust from Phabricator's queue, next to
//! PHP's taskmasters, which keep running every other class.

use std::sync::Arc;

use phab_worker::{
    Executed, LeaseOwner, Worker, config_dir, connect, handlers, repository_command,
};
use tokio::sync::watch;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

const USAGE: &str = "usage: phab-worker [--once [--id <task id>]...]
       phab-worker repository <workflow> ... (see phab-worker repository --help)

Without arguments, runs the ported task classes until SIGTERM. With --once,
leases at most one task (among the --id ones if given), runs it, prints what
happened as JSON and exits: 0 when it ran, 2 when there was no task to lease,
3 when its handler left it to PHP, 1 on an error.";

/// Exit status of `--once` when no task could be leased.
const EXIT_NOTHING: u8 = 2;
/// Exit status of `--once` when the handler left the task to PHP.
const EXIT_DECLINED: u8 = 3;
/// Exit status for bad arguments.
const EXIT_USAGE: u8 = 64;

/// `--once` and its `--id`s: `None` to run as a daemon, no IDs for one
/// task of any ID.
fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Option<Vec<i64>>, String> {
    let mut once = false;
    let mut ids = Vec::new();
    let mut arguments = arguments.peekable();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--once" => once = true,
            "--id" => {
                let value = arguments.next().ok_or("--id needs a task ID")?;
                ids.push(
                    value
                        .parse()
                        .map_err(|_| format!("invalid task ID: {value}"))?,
                );
            }
            "--help" | "-h" => return Err(String::new()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if !once {
        if !ids.is_empty() {
            return Err("--id needs --once".to_owned());
        }
        return Ok(None);
    }
    Ok(Some(ids))
}

/// `--once`: leases and runs at most one task, and says what happened.
async fn work_once(worker: &Worker, ids: Option<&[i64]>) -> std::process::ExitCode {
    let ran = match worker.work_on(ids).await {
        Ok(ran) => ran,
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"outcome": "error", "message": error.to_string()})
            );
            return std::process::ExitCode::FAILURE;
        }
    };
    let Some(ran) = ran else {
        let declined = ids
            .unwrap_or_default()
            .iter()
            .find_map(|id| Some((*id, worker.declined.reason(*id)?)));
        if let Some((id, reason)) = declined {
            println!(
                "{}",
                serde_json::json!({"id": id, "outcome": "declined", "message": reason})
            );
            return std::process::ExitCode::from(EXIT_DECLINED);
        }
        println!("{}", serde_json::json!({"outcome": "nothing"}));
        return std::process::ExitCode::from(EXIT_NOTHING);
    };
    let (outcome, message, status) = match &ran.outcome {
        Ok(Executed::Succeeded(_)) => ("succeeded", String::new(), 0),
        Ok(Executed::FailedPermanently { message, .. }) => {
            ("failed-permanently", message.clone(), 0)
        }
        Ok(Executed::Yielded(_)) => ("yielded", String::new(), 0),
        Ok(Executed::Failed { message, .. }) => ("failed", message.clone(), 0),
        Ok(Executed::Declined { reason, .. }) => ("declined", reason.clone(), EXIT_DECLINED),
        Err(error) => ("error", error.to_string(), 1),
    };
    println!(
        "{}",
        serde_json::json!({
            "id": ran.id,
            "class": ran.class,
            "outcome": outcome,
            "message": message,
        })
    );
    std::process::ExitCode::from(status)
}

/// Resolves on SIGTERM or Ctrl-C.
async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("shutting down after the current tasks");
}

/// `phab-worker repository ...`, logging to stderr.
async fn repository(arguments: impl Iterator<Item = String>) -> std::process::ExitCode {
    let command = match repository_command::parse(arguments) {
        Ok(command) => command,
        Err(error) if error.is_empty() => {
            println!("{}", repository_command::USAGE);
            return std::process::ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("{error}\n\n{}", repository_command::USAGE);
            return std::process::ExitCode::from(EXIT_USAGE);
        }
    };
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
    let config_dir = config_dir();
    let (config, database) = match connect(&config_dir).await {
        Ok(context) => context,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    std::process::ExitCode::from(repository_command::run(&command, &database, &config).await)
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let mut arguments = std::env::args().skip(1).peekable();
    if arguments.peek().map(String::as_str) == Some("repository") {
        arguments.next();
        return repository(arguments).await;
    }
    let once = match parse_arguments(std::env::args().skip(1)) {
        Ok(once) => once,
        Err(error) if error.is_empty() => {
            println!("{USAGE}");
            return std::process::ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("{error}\n\n{USAGE}");
            return std::process::ExitCode::from(EXIT_USAGE);
        }
    };
    // With `--once`, stdout is for the outcome only.
    let log = tracing_subscriber::fmt::layer().json();
    let log = if once.is_some() {
        log.with_writer(std::io::stderr).boxed()
    } else {
        log.boxed()
    };
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(log)
        .init();
    let config_dir = config_dir();
    let mut registry = handlers::registry();
    if let Ok(allowed) = std::env::var("PHAB_WORKER_CLASSES") {
        let allowed: Vec<&str> = allowed
            .split(',')
            .map(str::trim)
            .filter(|class| !class.is_empty())
            .collect();
        let unknown = registry.retain(&allowed);
        if !unknown.is_empty() {
            tracing::error!(?unknown, "PHAB_WORKER_CLASSES names classes not ported");
            return std::process::ExitCode::FAILURE;
        }
    }
    if registry.classes().is_empty() {
        tracing::error!("no task class to run");
        return std::process::ExitCode::FAILURE;
    }
    let taskmasters: usize = match std::env::var("PHAB_WORKER_TASKMASTERS")
        .unwrap_or_else(|_| "1".to_owned())
        .parse()
    {
        Ok(count) if count > 0 => count,
        _ => {
            tracing::error!("invalid PHAB_WORKER_TASKMASTERS");
            return std::process::ExitCode::FAILURE;
        }
    };

    let (config, database) = match connect(&config_dir).await {
        Ok(context) => context,
        Err(error) => {
            tracing::error!(%error, "startup failed");
            return std::process::ExitCode::FAILURE;
        }
    };
    let host = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    let worker = Arc::new(Worker::new(
        database,
        config,
        registry,
        LeaseOwner::new(&host),
    ));
    if let Some(ids) = once {
        return work_once(&worker, (!ids.is_empty()).then_some(&ids[..])).await;
    }
    tracing::info!(
        classes = ?worker.registry.classes(),
        taskmasters,
        "working"
    );

    let (stop, stopped) = watch::channel(false);
    let mut taskmasters: Vec<_> = (0..taskmasters)
        .map(|_| {
            let worker = Arc::clone(&worker);
            let stopped = stopped.clone();
            tokio::spawn(async move { worker.run(stopped).await })
        })
        .collect();
    shutdown_signal().await;
    let _ = stop.send(true);
    for taskmaster in taskmasters.drain(..) {
        if let Err(error) = taskmaster.await {
            tracing::error!(%error, "taskmaster panicked");
        }
    }
    std::process::ExitCode::SUCCESS
}
