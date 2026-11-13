//! The Rust Phabricator service: answers the Conduit methods it implements
//! and hands the others back to PHP through nginx.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use phab_conduit::Registry;
use phab_server::AppState;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

fn init_logging() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .with(sentry::integrations::tracing::layer())
        .init();
}

fn methods() -> Registry {
    let mut registry = Registry::default();
    phab_app_conduit::register(&mut registry);
    phab_app_people::register(&mut registry);
    phab_app_feed::register(&mut registry);
    registry
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // Reads SENTRY_DSN; reporting is disabled without it.
    let _sentry = sentry::init(sentry::ClientOptions {
        release: sentry::release_name!(),
        ..sentry::ClientOptions::default()
    });
    init_logging();

    let config_dir = std::env::var_os("PHAB_CONFIG_DIR")
        .map_or_else(phab_server::default_config_dir, PathBuf::from);
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

    let context = match phab_server::build_context(&config_dir, phab_server::hostname()).await {
        Ok(context) => context,
        Err(error) => {
            tracing::error!(%error, "startup failed");
            return std::process::ExitCode::FAILURE;
        }
    };
    let remarkup = phab_web::remarkup::new_engine(&context.config);
    let state = Arc::new(AppState {
        remarkup,
        php: phab_web::php::PhpConduit::from_env(),
        context,
        registry: methods(),
        shadow: phab_server::shadow::Shadow::from_env(),
    });

    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, %listen, "cannot listen");
            return std::process::ExitCode::FAILURE;
        }
    };
    tracing::info!(%listen, methods = ?state.registry.names(), "listening");
    if let Err(error) = axum::serve(listener, phab_server::router(state)).await {
        tracing::error!(%error, "server stopped");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
