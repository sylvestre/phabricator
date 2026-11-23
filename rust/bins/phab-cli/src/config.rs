//! `config get|set|delete|list`: `PhabricatorConfigManagement*Workflow`.

use std::io::Read as _;

use phab_app_config::local::LocalSource;
use phab_app_config::{Error, entry, json, value};
use phab_db::Database;
use serde_json::{Value, json};

use crate::Context;

/// The command PHP runs these workflows with.
const PHP_COMMAND: &str = "bin/config";

/// The parsed arguments of a workflow: `--database`, `--stdin` and the
/// rest.
struct Arguments {
    database: bool,
    stdin: bool,
    args: Vec<String>,
}

fn parse(arguments: &[String], flags: &[&str]) -> Result<Arguments, String> {
    let mut parsed = Arguments {
        database: false,
        stdin: false,
        args: Vec::new(),
    };
    for argument in arguments {
        match argument.as_str() {
            "--database" if flags.contains(&"database") => parsed.database = true,
            "--stdin" if flags.contains(&"stdin") => parsed.stdin = true,
            flag if flag.starts_with("--") => {
                return Err(format!("Argument \"{flag}\" is unrecognized."));
            }
            _ => parsed.args.push(argument.clone()),
        }
    }
    Ok(parsed)
}

/// Runs `config <workflow> ...`.
pub async fn run(context: &mut Context<'_>, arguments: &[String]) -> u8 {
    let Some((workflow, rest)) = arguments.split_first() else {
        return context.usage_error("Choose a workflow: get, set, delete or list.");
    };
    let flags: &[&str] = match workflow.as_str() {
        "get" | "list" => &[],
        "set" => &["database", "stdin"],
        "delete" => &["database"],
        "migrate" | "done" => {
            return context.left_to_php(&format!("config {workflow}"), PHP_COMMAND);
        }
        other => return context.usage_error(&format!("Unknown workflow \"{other}\".")),
    };
    let parsed = match parse(rest, flags) {
        Ok(parsed) => parsed,
        Err(message) => return context.usage_error(&message),
    };
    match workflow.as_str() {
        "get" => get(context, &parsed).await,
        "set" => set(context, &parsed).await,
        "delete" => delete(context, &parsed).await,
        _ => list(context),
    }
}

/// `PhabricatorConfigManagementListWorkflow`.
fn list(context: &mut Context<'_>) -> u8 {
    for option in phab_app_config::options() {
        let _ = writeln!(context.out, "{}", option.key);
    }
    0
}

/// The database of the configuration `local.json` gives.
async fn connect(context: &Context<'_>) -> Result<Database, String> {
    let config = crate::local_config(context)?;
    Database::connect(&config)
        .await
        .map_err(|error| error.to_string())
}

/// One source of `config get`'s answer.
fn source_entry(key: &str, source: &str, found: Result<Option<Value>, ()>) -> Value {
    let (value, status, error_info) = match found {
        Ok(Some(value)) => (value, "set", Value::Null),
        Ok(None) => (Value::Null, "unset", Value::Null),
        Err(()) => (
            Value::Null,
            "error",
            Value::String("Database source is not configured properly".to_owned()),
        ),
    };
    json!({
        "key": key,
        "source": source,
        "value": value,
        "status": status,
        "errorInfo": error_info,
    })
}

/// `PhabricatorConfigManagementGetWorkflow`.
async fn get(context: &mut Context<'_>, arguments: &Arguments) -> u8 {
    let key = match arguments.args.as_slice() {
        [] => return context.usage_error("Specify a configuration key to get."),
        [key] => key.clone(),
        _ => return context.usage_error("Too many arguments: expected one key."),
    };
    if phab_app_config::option(&key).is_none() {
        return context.usage_error(&format!(
            "No such configuration key '{key}'! Use `config list` to list all keys."
        ));
    }
    let local = match LocalSource::load(&context.local_path()) {
        Ok(local) => local,
        Err(error) => return context.error(&error.to_string()),
    };
    let database = match connect(context).await {
        Ok(database) => entry::value(&database, &key).await.map_err(|_| ()),
        Err(_) => Err(()),
    };
    let result = json!({
        "config": [
            source_entry(&key, "local", Ok(local.get(&key).cloned())),
            source_entry(&key, "database", database),
        ],
    });
    let Value::Object(result) = result else {
        unreachable!("json! of an object");
    };
    let _ = context
        .out
        .write_all(json::encode_formatted(&result).as_bytes());
    0
}

/// `PhabricatorConfigManagementSetWorkflow`.
async fn set(context: &mut Context<'_>, arguments: &Arguments) -> u8 {
    let Some(key) = arguments.args.first().cloned() else {
        return context.usage_error("Specify the configuration key you want to set.");
    };
    let raw = if arguments.stdin {
        if arguments.args.len() > 1 {
            return context.usage_error(
                "Too many arguments: expected only a configuration key when using \"--stdin\".",
            );
        }
        let _ = writeln!(context.err, "Reading value from stdin...");
        let mut raw = String::new();
        if let Err(error) = std::io::stdin().read_to_string(&mut raw) {
            return context.error(&error.to_string());
        }
        raw
    } else {
        match arguments.args.len() {
            1 => {
                return context.usage_error(&format!(
                    "Specify a value to set the configuration key \"{key}\" to, or use \
                     \"--stdin\" to read a value from stdin."
                ));
            }
            2 => arguments.args[1].clone(),
            _ => return context.usage_error("Too many arguments: expected one key and one value."),
        }
    };
    set_value(context, &key, &raw, arguments.database).await
}

/// The checks and the write of `config set`, for a value read already.
pub async fn set_value(context: &mut Context<'_>, key: &str, raw: &str, use_database: bool) -> u8 {
    let Some(option) = phab_app_config::option(key) else {
        return context.usage_error(&format!(
            "Configuration key \"{key}\" is unknown. Use \"bin/config list\" to list all known \
             keys."
        ));
    };
    let parsed = value::from_command_line(option, raw);
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(error) => return report(context, &error),
    };
    if use_database {
        let config = match crate::local_config(context) {
            Ok(config) => config,
            Err(error) => return context.error(&error),
        };
        // PHP reads `config.lock` from the whole stack, the database too.
        let config = match connect(context).await {
            Ok(database) => match database.load_config_entries().await {
                Ok(entries) => {
                    let mut config = config;
                    config.push_layer("Database", entries);
                    config
                }
                Err(error) => return context.error(&error.to_string()),
            },
            Err(error) => return context.error(&error),
        };
        if option.is_locked(&config) {
            return context.usage_error(&format!(
                "Config key \"{key}\" is locked and can only be set in local configuration. To \
                 learn more, see \"Configuration Guide: Locked and Hidden Configuration\" in the \
                 documentation."
            ));
        }
    }
    if let Err(error) = value::validate_option(option, &parsed) {
        return report(context, &error);
    }
    let message = if use_database {
        let database = match connect(context).await {
            Ok(database) => database,
            Err(error) => return context.error(&error),
        };
        if let Err(error) = entry::set(&database, key, &parsed, context.now).await {
            return context.error(&error.to_string());
        }
        format!("Wrote configuration key \"{key}\" to database storage.")
    } else {
        let mut local = match LocalSource::load(&context.local_path()) {
            Ok(local) => local,
            Err(error) => return context.error(&error.to_string()),
        };
        if let Err(error) = local.set(key, parsed) {
            return report(context, &error);
        }
        format!(
            "Wrote configuration key \"{key}\" to local storage (in file \"{}\").",
            local.path().display()
        )
    };
    context.done("DONE", &message);
    0
}

/// An error of the configuration code, as the workflow reports it.
fn report(context: &mut Context<'_>, error: &Error) -> u8 {
    match error {
        Error::Usage(message) | Error::Local(message) => context.usage_error(message),
        Error::Unsupported(reason) => context.left_to_php(reason, PHP_COMMAND),
        Error::Db(error) => context.error(&error.to_string()),
    }
}

/// `PhabricatorConfigManagementDeleteWorkflow`.
async fn delete(context: &mut Context<'_>, arguments: &Arguments) -> u8 {
    let key = match arguments.args.as_slice() {
        [] => return context.usage_error("Specify a configuration key to delete."),
        [key] => key.clone(),
        _ => return context.usage_error("Too many arguments: expected one key."),
    };
    let source = if arguments.database {
        "database"
    } else {
        "local"
    };
    let deleted = if arguments.database {
        let database = match connect(context).await {
            Ok(database) => database,
            Err(error) => return context.error(&error),
        };
        match entry::delete(&database, &key, context.now).await {
            Ok(deleted) => deleted,
            Err(error) => return context.error(&error.to_string()),
        }
    } else {
        let mut local = match LocalSource::load(&context.local_path()) {
            Ok(local) => local,
            Err(error) => return context.error(&error.to_string()),
        };
        if local.get(&key).is_some() {
            if let Err(error) = local.delete(&key) {
                return report(context, &error);
            }
            true
        } else {
            false
        }
    };
    if !deleted {
        return context.usage_error(&format!(
            "Configuration key '{key}' is not set in {source} configuration!"
        ));
    }
    let _ = writeln!(context.out, "Deleted '{key}' from {source} configuration.");
    0
}
