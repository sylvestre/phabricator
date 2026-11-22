//! Runs the task classes ported to Rust from Phabricator's queue, next to
//! PHP's taskmasters, which keep running every other class.

use std::path::PathBuf;
use std::sync::Arc;

use phab_core::Config;
use phab_db::Database;
use phab_worker::{LeaseOwner, Worker, handlers};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

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

/// `PhabricatorEnv`'s stack (defaults, `local.json`, the database), then
/// the connection.
async fn connect(config_dir: &std::path::Path) -> Result<(Config, Database), String> {
    let mut config = Config::default();
    config
        .push_local_file(&config_dir.join("local.json"))
        .map_err(|error| error.to_string())?;
    let database = Database::connect(&config)
        .await
        .map_err(|error| error.to_string())?;
    let entries = database
        .load_config_entries()
        .await
        .map_err(|error| error.to_string())?;
    config.push_layer("Database", entries);
    Ok((config, database))
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    let config_dir = std::env::var_os("PHAB_CONFIG_DIR").map_or_else(
        || PathBuf::from("/app/phabricator/conf/local"),
        PathBuf::from,
    );
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
    let worker = Arc::new(Worker {
        database,
        config,
        registry,
        owner: LeaseOwner::new(&host),
    });
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
