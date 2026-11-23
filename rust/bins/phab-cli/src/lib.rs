//! `phab-cli`: the `bin/` workflows of PHP which the port keeps, each
//! writing what PHP's writes.
//!
//! - `config get|set|delete|list` (`bin/config`): `local.json`, or the
//!   `config_entry` table with `--database`.
//! - `repository discover|refs|update` (`bin/repository`), through
//!   `phab-worker`'s import code.
//! - `worker execute --id N` (`bin/worker`), through `phab-worker`'s
//!   handlers.
//! - `storage status` (`bin/storage`), read-only.
//!
//! What a workflow doesn't port exits with [`EXIT_LEFT_TO_PHP`] before
//! writing anything, saying which `bin/` command to run instead.

pub mod config;
pub mod storage;
pub mod worker;

use std::io::Write;
use std::path::PathBuf;

/// Exit status of a workflow left to PHP.
pub const EXIT_LEFT_TO_PHP: u8 = 3;
/// Exit status of `PhutilArgumentUsageException`.
pub const EXIT_USAGE: u8 = 77;

/// The usage of the command.
pub const USAGE: &str = "usage: phab-cli config get <key>
       phab-cli config set [--database] <key> <value>
       phab-cli config set [--database] --stdin <key> < value.json
       phab-cli config delete [--database] <key>
       phab-cli config list
       phab-cli repository discover [--repair] [--verbose] <repository>
       phab-cli repository refs [--verbose] <repository>
       phab-cli repository update [--no-discovery] [--verbose] <repository>
       phab-cli worker execute --id <task id> [--id <task id>...]
       phab-cli storage status

The workflows of bin/config, bin/repository, bin/worker and bin/storage.
local.json is read from PHAB_CONFIG_DIR (/app/phabricator/conf/local by
default). Exits 0 on success, 3 when the work is left to the PHP command
(nothing was written), 77 on a usage error, 1 on other errors.";

/// Where a workflow reads its configuration and writes its output.
pub struct Context<'a> {
    /// The directory of `local.json`.
    pub config_dir: PathBuf,
    /// The standard output.
    pub out: &'a mut dyn Write,
    /// The standard error.
    pub err: &'a mut dyn Write,
    /// Whether the output is a terminal, for PHP's colors.
    pub ansi: bool,
    /// The current Unix time.
    pub now: i64,
}

impl Context<'_> {
    /// The path of `local.json`.
    pub fn local_path(&self) -> PathBuf {
        self.config_dir.join("local.json")
    }

    /// The Phabricator root (`conf/local/..` up twice), for the paths PHP
    /// prints.
    pub fn root(&self) -> PathBuf {
        self.config_dir
            .parent()
            .and_then(std::path::Path::parent)
            .map_or_else(
                || PathBuf::from("/app/phabricator"),
                std::path::Path::to_path_buf,
            )
    }

    /// `PhutilArgumentUsageException`: its message on stderr, and 77.
    pub fn usage_error(&mut self, message: &str) -> u8 {
        let _ = writeln!(self.err, "Usage Exception: {message}");
        EXIT_USAGE
    }

    /// A workflow or a value Rust doesn't port.
    pub fn left_to_php(&mut self, reason: &str, command: &str) -> u8 {
        let _ = writeln!(
            self.err,
            "left to PHP: {reason}. Nothing was written; run {command} instead."
        );
        EXIT_LEFT_TO_PHP
    }

    /// A failure.
    pub fn error(&mut self, message: &str) -> u8 {
        let _ = writeln!(self.err, "{message}");
        1
    }

    /// `tsprintf("<bg:green>** %s **</bg> %s\n", ...)`: a green label in a
    /// terminal, the plain text elsewhere.
    pub fn done(&mut self, label: &str, message: &str) {
        if self.ansi {
            let _ = writeln!(self.out, "\x1b[42m\x1b[1m {label} \x1b[m\x1b[49m {message}");
        } else {
            let _ = writeln!(self.out, " {label}  {message}");
        }
    }
}

/// Runs the command line `arguments` (without the program name).
pub async fn run(context: &mut Context<'_>, arguments: &[String]) -> u8 {
    let Some((command, rest)) = arguments.split_first() else {
        let _ = writeln!(context.err, "{USAGE}");
        return EXIT_USAGE;
    };
    match command.as_str() {
        "config" => config::run(context, rest).await,
        "storage" => storage::run(context, rest).await,
        "worker" => worker::run(context, rest).await,
        "repository" => repository(context, rest).await,
        "--help" | "-h" | "help" => {
            let _ = writeln!(context.out, "{USAGE}");
            0
        }
        other => context.usage_error(&format!("unknown workflow \"{other}\"")),
    }
}

/// `repository <workflow>`: `phab-worker repository`'s code.
async fn repository(context: &mut Context<'_>, arguments: &[String]) -> u8 {
    use phab_worker::repository_command;
    let command = match repository_command::parse(arguments.iter().cloned()) {
        Ok(command) => command,
        Err(error) if error.is_empty() => {
            let _ = writeln!(context.out, "{USAGE}");
            return 0;
        }
        Err(error) => return context.usage_error(&error),
    };
    let (config, database) = match phab_worker::connect(&context.config_dir).await {
        Ok(connected) => connected,
        Err(error) => return context.error(&error),
    };
    repository_command::run(&command, &database, &config).await
}

/// `PhabricatorEnv`'s configuration without the database: the defaults
/// and `local.json`.
pub fn local_config(context: &Context<'_>) -> Result<phab_core::Config, String> {
    let mut config = phab_core::Config::default();
    config
        .push_local_file(&context.local_path())
        .map_err(|error| error.to_string())?;
    Ok(config)
}
