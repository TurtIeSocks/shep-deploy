//! One error type for the whole binary.
//!
//! A deploy is a single linear pass, fetch through verify, so a single enum
//! is the simplest thing that works. Splitting it per module would buy
//! nothing that the variant names do not already say, and every later task
//! adds variants here rather than introducing a second error type.

use core::fmt;
use std::path::PathBuf;

use shep_client::{ConnectError, RequestError};

/// Anything that can go wrong in one deploy.
///
/// Deliberately `#[derive(Debug)]` rather than a hand-written impl: nothing
/// in this enum carries a secret to redact. `Git`'s `command` and `stderr`
/// are the closest thing to a risk, and the design is explicit that this dog
/// does no credential handling of its own - it inherits the build user's git
/// auth exactly, so no credential ever passes through a URL or argument this
/// crate constructs. If a later task adds a variant that could carry one,
/// that variant needs its own redacted `Debug` and an exact-string test; this
/// derive is not a blanket exemption.
#[derive(Debug)]
pub enum Error {
    /// The shepherd's socket could not be reached.
    Connect(ConnectError),
    /// A request reached the shepherd and came back an error.
    Request(RequestError),
    /// The shepherd answered with a response this dog cannot use.
    Protocol(String),
    /// A `[dog.deploy]` (or per-sheep override) section could not be
    /// understood.
    Config(String),
    /// A filesystem operation failed, naming the path it failed on.
    Io {
        /// The path being read, written, linked or removed.
        path: PathBuf,
        /// The underlying failure.
        source: std::io::Error,
    },
    /// A `git` invocation exited without doing what it was asked.
    Git {
        /// The command line, for a human reading the failure.
        command: String,
        /// The process's exit status, or `None` if it was killed by a
        /// signal instead of exiting.
        status: Option<i32>,
        /// Whatever `git` wrote to its own stderr. This is usually the only
        /// part of the failure that says anything useful - `fetch`,
        /// `worktree add`, and friends explain themselves there and nowhere
        /// else.
        stderr: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(err) => write!(f, "cannot reach the shepherd: {err}"),
            Self::Request(err) => write!(f, "the shepherd refused a request: {err}"),
            Self::Protocol(what) => write!(f, "unexpected answer from the shepherd: {what}"),
            Self::Config(what) => write!(f, "bad deploy configuration: {what}"),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Git {
                command,
                status,
                stderr,
            } => match status {
                Some(code) => write!(f, "`{command}` exited with status {code}: {stderr}"),
                None => write!(f, "`{command}` was killed by a signal: {stderr}"),
            },
        }
    }
}

impl core::error::Error for Error {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Connect(err) => Some(err),
            Self::Request(err) => Some(err),
            Self::Io { source, .. } => Some(source),
            Self::Protocol(_) | Self::Config(_) | Self::Git { .. } => None,
        }
    }
}

impl From<ConnectError> for Error {
    fn from(err: ConnectError) -> Self {
        Self::Connect(err)
    }
}

impl From<RequestError> for Error {
    fn from(err: RequestError) -> Self {
        Self::Request(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if an Io error stops naming the path it failed on. A deploy
    /// touches many paths and "permission denied" without one is unactionable.
    #[test]
    fn an_io_error_names_the_path() {
        let err = Error::Io {
            path: PathBuf::from("/srv/x/releases/abc/dist"),
            source: std::io::Error::other("permission denied"),
        };
        assert!(err.to_string().contains("/srv/x/releases/abc/dist"));
    }

    /// fails if a git failure hides stderr. git's own message is the only
    /// useful thing when a fetch or worktree add fails.
    #[test]
    fn a_git_error_carries_stderr() {
        let err = Error::Git {
            command: "git fetch origin".to_string(),
            status: Some(128),
            stderr: "fatal: could not read Username".to_string(),
        };
        let shown = err.to_string();
        assert!(shown.contains("git fetch origin"));
        assert!(shown.contains("could not read Username"));
    }
}
