//! How an authentication attempt ended, and whether it may write.

/// Whether authentication may write to the database. PHP extends sessions,
/// upgrades legacy session keys and makes `cli-` tokens permanent while
/// authenticating; shadow traffic and read-only installs must not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writes {
    /// Authentication writes like PHP does.
    Allowed,
    /// Writes are left out; the result is otherwise the same.
    Skipped,
}

/// Result of an authentication attempt which PHP may have handled
/// differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<T> {
    /// Handled as PHP would handle it.
    Done(T),
    /// PHP must handle this request; the string says why.
    Unsupported(&'static str),
}
