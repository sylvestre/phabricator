//! `phab-cli`: the `bin/config`, `bin/repository`, `bin/worker` and
//! `bin/storage` workflows the port keeps (see the library).

use std::io::IsTerminal as _;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let ansi = out.is_terminal();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
    let mut context = phab_cli::Context {
        config_dir: phab_worker::config_dir(),
        out: &mut out,
        err: &mut err,
        ansi,
        now,
    };
    std::process::ExitCode::from(phab_cli::run(&mut context, &arguments).await)
}
