//! The libphutil string functions the commit parsers use
//! ([`phab_core::text`]) and
//! `PhabricatorRepositoryCommitData::summarizeCommitMessage()`.

pub use phab_core::text::{split_lines, truncate};

/// `PhabricatorRepositoryCommitData::summarizeCommitMessage()`: the first
/// line, cut to fit the `summary` column (255 bytes) and 80 glyphs.
pub fn summarize_commit_message(message: &str) -> String {
    let first = split_lines(message).first().copied().unwrap_or_default();
    truncate(first, Some(255), Some(80))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_like_php() {
        assert_eq!(summarize_commit_message("Subject\n\nBody"), "Subject");
    }
}
