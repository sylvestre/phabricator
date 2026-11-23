//! `phab-worker repository <workflow>`: the `bin/repository` workflows of
//! the Git import pipeline ([`phab_app_diffusion::import::update`]), which
//! `PhabricatorRepositoryPullLocalDaemon` runs for each repository.

use phab_app_diffusion::import::ImportError;
use phab_app_diffusion::import::repository::ImportRepository;
use phab_app_diffusion::import::update::{self, Outcome};
use phab_core::Config;
use phab_db::Database;

/// The usage of the subcommand.
pub const USAGE: &str = "usage: phab-worker repository discover [--repair] [--verbose] <repository>
       phab-worker repository refs [--verbose] <repository>
       phab-worker repository update [--no-discovery] [--verbose] <repository>

Runs bin/repository's workflow on one Git repository (an ID, a monogram, a
PHID, a callsign or a short name). Exits 0 when it ran, 3 when it is left
to PHP (nothing was written), 1 on an error.";

/// Exit status when the workflow is left to PHP.
pub const EXIT_DECLINED: u8 = 3;

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// `discover`, `refs` or `update`.
    pub workflow: String,
    /// The repository.
    pub repository: String,
    /// `--repair`.
    pub repair: bool,
    /// `--no-discovery`.
    pub no_discovery: bool,
    /// `--verbose`.
    pub verbose: bool,
}

/// Parses the arguments after `repository`.
pub fn parse(arguments: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut workflow = None;
    let mut repository = None;
    let (mut repair, mut no_discovery, mut verbose) = (false, false, false);
    for argument in arguments {
        match argument.as_str() {
            "--repair" => repair = true,
            "--no-discovery" => no_discovery = true,
            "--verbose" => verbose = true,
            "--help" | "-h" => return Err(String::new()),
            flag if flag.starts_with("--") => return Err(format!("unknown argument: {flag}")),
            _ if workflow.is_none() => workflow = Some(argument),
            _ if repository.is_none() => repository = Some(argument),
            _ => return Err("specify exactly one repository".to_owned()),
        }
    }
    let workflow = workflow.ok_or("specify a workflow")?;
    let allowed: &[&str] = match workflow.as_str() {
        "discover" => &["repair"],
        "refs" => &[],
        "update" => &["no-discovery"],
        other => return Err(format!("unknown workflow: {other}")),
    };
    if (repair && !allowed.contains(&"repair"))
        || (no_discovery && !allowed.contains(&"no-discovery"))
    {
        return Err(format!("invalid flag for {workflow}"));
    }
    Ok(Command {
        workflow,
        repository: repository.ok_or("specify a repository")?,
        repair,
        no_discovery,
        verbose,
    })
}

/// Runs `command`; returns the exit status.
pub async fn run(command: &Command, database: &Database, config: &Config) -> u8 {
    let mut repository = match ImportRepository::find(database, &command.repository).await {
        Ok(Some(repository)) => repository,
        Ok(None) => {
            eprintln!("No repository \"{}\" exists.", command.repository);
            return 1;
        }
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let result: Result<Outcome, ImportError> = match command.workflow.as_str() {
        "discover" => update::discover(database, config, &mut repository, command.repair).await,
        "refs" => update::refs(database, config, &repository).await,
        _ => update::update(database, config, &mut repository, command.no_discovery).await,
    };
    match result {
        Ok(outcome) => {
            if command.verbose {
                for line in &outcome.log {
                    println!("{line}");
                }
            }
            for line in &outcome.output {
                println!("{line}");
            }
            0
        }
        Err(ImportError::Unsupported(reason)) => {
            println!("left to PHP: {reason}");
            EXIT_DECLINED
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(text: &str) -> impl Iterator<Item = String> {
        text.split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parses_the_workflows() {
        assert_eq!(
            parse(arguments("discover --repair FAB")),
            Ok(Command {
                workflow: "discover".to_owned(),
                repository: "FAB".to_owned(),
                repair: true,
                no_discovery: false,
                verbose: false,
            })
        );
        assert!(parse(arguments("update --no-discovery --verbose R3")).is_ok());
        assert!(parse(arguments("refs --repair R3")).is_err());
        assert!(parse(arguments("update")).is_err());
        assert!(parse(arguments("pull R3")).is_err());
        assert!(parse(arguments("refs R3 R4")).is_err());
    }
}
