use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use phab_conduit::Registry;
use phab_server::{AppState, build_context, default_config_dir, hostname, router};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

fn init_logging() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .with(sentry::integrations::tracing::layer())
        .init();
}

fn methods() -> Registry {
    Registry::default()
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // Reads SENTRY_DSN; reporting is disabled without it.
    let _sentry = sentry::init(sentry::ClientOptions {
        release: sentry::release_name!(),
        ..sentry::ClientOptions::default()
    });
    init_logging();

    let config_dir =
        std::env::var_os("PHAB_CONFIG_DIR").map_or_else(default_config_dir, PathBuf::from);
    let listen: SocketAddr = match std::env::var("PHAB_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_owned())
        .parse()
    {
        Ok(address) => address,
        Err(error) => {
            tracing::error!(%error, "invalid PHAB_LISTEN");
            return std::process::ExitCode::FAILURE;
        }
    };

    let context = match build_context(&config_dir, hostname()).await {
        Ok(context) => context,
        Err(error) => {
            tracing::error!(%error, "startup failed");
            return std::process::ExitCode::FAILURE;
        }
    };
    let state = Arc::new(AppState {
        context,
        registry: methods(),
    });

    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, %listen, "cannot listen");
            return std::process::ExitCode::FAILURE;
        }
    };
    tracing::info!(%listen, methods = ?state.registry.names(), "listening");
    if let Err(error) = axum::serve(listener, router(state)).await {
        tracing::error!(%error, "server stopped");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
