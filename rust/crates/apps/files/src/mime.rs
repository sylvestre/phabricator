//! `Filesystem::getMimeType()`: the MIME type of uploaded data.
//!
//! PHP asks libmagic (`finfo_file()` with `FILEINFO_MIME`), which answers
//! like `file --brief --mime`, e.g. `text/plain; charset=us-ascii`. Its
//! rules are too many to reimplement, so Rust runs a command which answers
//! the same way; without one, uploads which need a type detected go to PHP.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

/// How long the command may run.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A command printing the MIME type of the file whose path it is given as
/// its last argument, like `file --brief --mime`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MimeCommand {
    program: String,
    arguments: Vec<String>,
}

impl MimeCommand {
    /// A command from a space-separated string such as
    /// `file --brief --mime`; `None` for an empty string.
    pub fn parse(command: &str) -> Option<Self> {
        let mut words = command.split_whitespace();
        let program = words.next()?;
        Some(Self {
            program: program.to_owned(),
            arguments: words.map(str::to_owned).collect(),
        })
    }

    /// The command in `PHAB_MIME_COMMAND`, if set. It must run the same
    /// libmagic rules as PHP's `fileinfo` extension.
    pub fn from_env() -> Option<Self> {
        Self::parse(&std::env::var("PHAB_MIME_COMMAND").ok()?)
    }

    /// The MIME type of `data`, trimmed as PHP trims `file`'s output and
    /// without the encoding PHP strips off (`text/plain`, not
    /// `text/plain; charset=us-ascii`); `None` when the command fails.
    pub fn detect(&self, data: &[u8]) -> Option<String> {
        let mut file = tempfile::NamedTempFile::new().ok()?;
        file.write_all(data).ok()?;
        file.flush().ok()?;
        let output = tempfile::tempfile().ok()?;
        let mut child = Command::new(&self.program)
            .args(&self.arguments)
            .arg(file.path())
            .stdin(Stdio::null())
            .stdout(output.try_clone().ok()?)
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let Some(status) = child.wait_timeout(TIMEOUT).ok()? else {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        };
        if !status.success() {
            return None;
        }
        let mut output = output;
        let mut text = String::new();
        std::io::Seek::rewind(&mut output).ok()?;
        std::io::Read::read_to_string(&mut output, &mut text).ok()?;
        let mime_type = text.trim();
        if mime_type.is_empty() {
            return None;
        }
        let mime_type = mime_type
            .split_once(';')
            .map_or(mime_type, |(mime_type, _)| mime_type);
        Some(mime_type.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(MimeCommand::parse("  "), None);
        assert_eq!(
            MimeCommand::parse("file --brief --mime"),
            Some(MimeCommand {
                program: "file".to_owned(),
                arguments: vec!["--brief".to_owned(), "--mime".to_owned()],
            })
        );
    }

    #[test]
    fn fails_without_the_program() {
        let command = MimeCommand::parse("/nonexistent/file --mime").unwrap();
        assert_eq!(command.detect(b"text"), None);
    }
}
