//! Git commands on the working copy of a repository, as
//! `PhabricatorRepository::execLocalCommand()` runs them
//! (`DiffusionGitCommandEngine`): `git <argv>` in the repository's local
//! path, in English, with an empty `HOME`.
//!
//! PHP runs them through `sudo` as `phd.user` when it is set; Rust runs
//! them as itself, so a working copy Git refuses to read for this user
//! ("dubious ownership") is left to PHP, as is one this host doesn't have.
//! A command which fails where PHP raises a `CommandException`, whose
//! message quotes the command line and Git's output, is left to PHP too:
//! see [`WorkingCopy::execx()`].

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use phab_conduit::{MethodError, pager};
use wait_timeout::ChildExt as _;

/// How long a command may run before PHP is left to answer.
const TIMEOUT: Duration = Duration::from_secs(60);

/// What Git says when the working copy belongs to another user.
const DUBIOUS_OWNERSHIP: &[u8] = b"detected dubious ownership";

/// The result of a command (`ExecFuture::resolve()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The exit code; `None` when a signal ended the command.
    pub status: Option<i32>,
    /// The standard output.
    pub stdout: Vec<u8>,
    /// The standard error.
    pub stderr: Vec<u8>,
}

impl Output {
    /// Whether the command exited with 0.
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }
}

/// How [`WorkingCopy::lines()`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinesEnd {
    /// The consumer stopped reading; the command was killed.
    Stopped,
    /// The command ran to its end with this exit code, after every line
    /// (`None` when a signal ended it).
    Exited(Option<i32>),
}

/// A directory Git can use as an empty `HOME`, as
/// `PhabricatorEnv::getEmptyCWD()` gives it to PHP's commands.
fn empty_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = std::env::temp_dir().join("phab-git-empty-home");
        let _ = std::fs::create_dir_all(&home);
        home
    })
}

/// The `-` check of `gitsprintf('%s', ...)`: Git would read a selector
/// starting with `-` as a flag, so PHP refuses it.
pub fn ref_selector(selector: &str) -> Result<String, MethodError> {
    if selector.starts_with('-') {
        return Err(pager::core_error(format!(
            "Git ref selector \"{selector}\" is not a valid selector and can not be passed to \
             the Git CLI safely in the general case."
        )));
    }
    Ok(selector.to_owned())
}

/// The working copy of a Git repository (`getLocalPath()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingCopy {
    path: PathBuf,
}

impl WorkingCopy {
    /// The working copy at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `assertLocalExists()`, then the command.
    fn command(&self, argv: &[String]) -> Result<Command, MethodError> {
        if !self.path.is_dir() {
            return Err(MethodError::Unsupported(
                "a working copy this host doesn't have",
            ));
        }
        let mut command = Command::new("git");
        command
            .args(argv)
            .current_dir(&self.path)
            .env_clear()
            .env("LANG", "en_US.UTF-8")
            .env("HOME", empty_home());
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        Ok(command)
    }

    /// `execLocalCommand()`: runs `git <argv>`, writing `stdin` to it.
    pub async fn exec(
        &self,
        argv: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> Result<Output, MethodError> {
        self.exec_with_timeout(argv, stdin, TIMEOUT).await
    }

    /// [`Self::exec()`] which gives up after `timeout`.
    pub async fn exec_with_timeout(
        &self,
        argv: Vec<String>,
        stdin: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Output, MethodError> {
        let command = self.command(&argv)?;
        let output = tokio::task::spawn_blocking(move || run(command, stdin, timeout))
            .await
            .map_err(MethodError::internal)??;
        if !output.success() && contains(&output.stderr, DUBIOUS_OWNERSHIP) {
            return Err(MethodError::Unsupported(
                "a working copy Git won't read as this user",
            ));
        }
        Ok(output)
    }

    /// `execxLocalCommand()`: the standard output of a command which must
    /// succeed. PHP raises a `CommandException` otherwise, whose message
    /// depends on the host, so a failure is left to PHP.
    pub async fn execx(
        &self,
        argv: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, MethodError> {
        let output = self.exec(argv, stdin).await?;
        if !output.success() {
            return Err(MethodError::Unsupported("a Git command which fails"));
        }
        Ok(output.stdout)
    }

    /// `LinesOfALargeExecFuture`: gives each `delimiter`-terminated piece
    /// of the output to `consumer` with `state`, until it returns `false`
    /// (the command is then killed). At the end of the output, the exit
    /// code is checked before the last unterminated piece, if any, is
    /// given: [`LinesEnd::Exited`] with a failure means PHP raised a
    /// `CommandException` there.
    pub async fn lines<S, F>(
        &self,
        argv: Vec<String>,
        stdin: Option<Vec<u8>>,
        delimiter: u8,
        state: S,
        consumer: F,
    ) -> Result<(S, LinesEnd), MethodError>
    where
        S: Send + 'static,
        F: FnMut(&mut S, &[u8]) -> Result<bool, MethodError> + Send + 'static,
    {
        let command = self.command(&argv)?;
        tokio::task::spawn_blocking(move || {
            stream(command, stdin, delimiter, state, consumer, TIMEOUT)
        })
        .await
        .map_err(MethodError::internal)?
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn spawn(mut command: Command, stdin: Option<Vec<u8>>) -> Result<Child, MethodError> {
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| MethodError::Unsupported("running Git, which is not installed"))?;
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A thread, so a command which writes before reading its input
        // can't block on a full pipe.
        std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }
    Ok(child)
}

fn read_all(pipe: Option<impl std::io::Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut data = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut data);
        }
        data
    })
}

fn exit_code(status: ExitStatus) -> Option<i32> {
    status.code()
}

fn run(command: Command, stdin: Option<Vec<u8>>, timeout: Duration) -> Result<Output, MethodError> {
    let mut child = spawn(command, stdin)?;
    let stdout = read_all(child.stdout.take());
    let stderr = read_all(child.stderr.take());
    let Some(status) = child.wait_timeout(timeout).map_err(MethodError::internal)? else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(MethodError::Unsupported("a Git command which timed out"));
    };
    Ok(Output {
        status: exit_code(status),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// Kills `child` once `deadline` passes, until it exits.
fn watch(child: Arc<Mutex<Child>>, deadline: Instant) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        loop {
            {
                let Ok(mut child) = child.lock() else {
                    return false;
                };
                if !matches!(child.try_wait(), Ok(None)) {
                    return false;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    })
}

fn stream<S, F>(
    command: Command,
    stdin: Option<Vec<u8>>,
    delimiter: u8,
    mut state: S,
    mut consumer: F,
    timeout: Duration,
) -> Result<(S, LinesEnd), MethodError>
where
    F: FnMut(&mut S, &[u8]) -> Result<bool, MethodError>,
{
    let mut child = spawn(command, stdin)?;
    let stdout = child.stdout.take();
    let stderr = read_all(child.stderr.take());
    let child = Arc::new(Mutex::new(child));
    let watchdog = watch(Arc::clone(&child), Instant::now() + timeout);
    let kill = |child: &Arc<Mutex<Child>>| {
        if let Ok(mut child) = child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    };
    let mut reader = BufReader::new(stdout.ok_or_else(|| MethodError::internal("no stdout"))?);
    let last = loop {
        let mut piece = Vec::new();
        reader
            .read_until(delimiter, &mut piece)
            .map_err(MethodError::internal)?;
        if piece.last() != Some(&delimiter) {
            break piece;
        }
        piece.pop();
        match consumer(&mut state, &piece) {
            Ok(true) => {}
            Ok(false) => {
                kill(&child);
                let _ = watchdog.join();
                return Ok((state, LinesEnd::Stopped));
            }
            Err(error) => {
                kill(&child);
                let _ = watchdog.join();
                return Err(error);
            }
        }
    };
    let status = child
        .lock()
        .map_err(|_| MethodError::internal("a poisoned lock"))?
        .wait()
        .map_err(MethodError::internal)?;
    if watchdog.join().unwrap_or(false) {
        return Err(MethodError::Unsupported("a Git command which timed out"));
    }
    let stderr = stderr.join().unwrap_or_default();
    if !status.success() {
        if contains(&stderr, DUBIOUS_OWNERSHIP) {
            return Err(MethodError::Unsupported(
                "a working copy Git won't read as this user",
            ));
        }
        return Ok((state, LinesEnd::Exited(exit_code(status))));
    }
    if !last.is_empty() {
        consumer(&mut state, &last)?;
    }
    Ok((state, LinesEnd::Exited(exit_code(status))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_flags_as_selectors() {
        assert_eq!(ref_selector("HEAD~1").as_deref(), Ok("HEAD~1"));
        assert!(matches!(
            ref_selector("--all"),
            Err(MethodError::Conduit { info: Some(info), .. })
                if info.starts_with("Git ref selector \"--all\" is not a valid selector")
        ));
    }

    #[tokio::test]
    async fn leaves_missing_working_copies_to_php() {
        let copy = WorkingCopy::new("/nonexistent/working/copy");
        assert_eq!(
            copy.exec(vec!["status".to_owned()], None).await,
            Err(MethodError::Unsupported(
                "a working copy this host doesn't have"
            ))
        );
    }
}
