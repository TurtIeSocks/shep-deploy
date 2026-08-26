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
    /// An operation failed with an I/O-shaped error, naming the path most
    /// relevant to the failure.
    ///
    /// Most of the time that operation really is a filesystem call - a
    /// read, write, rename, or symlink - and the path is the one it
    /// touched. A few things that are not filesystem calls at all get
    /// wrapped in here too, rather than inventing a second variant just for
    /// the wrapping: `State::write` reports a TOML serialisation failure
    /// this way, naming the file it was about to serialise into, and the
    /// shared-file module reports a `git` invocation that could not even be
    /// launched, or that answered with bytes that are not valid UTF-8,
    /// against the checkout directory it ran in. What all of these share is
    /// an [`std::io::Error`] as the underlying cause and a single path
    /// worth naming back to whoever reads the message.
    Io {
        /// The path most relevant to the failure - read, written, linked,
        /// removed, or simply the directory an operation was attempted in.
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
    /// Another deploy of the same sheep moved `current` while this one was
    /// preparing its release.
    ///
    /// The swap is refused rather than performed: the other deploy may have
    /// already verified a *newer* release, and swapping on top of it would
    /// silently revert something that is live and healthy.
    Raced {
        /// The sheep whose `current` moved.
        sheep: String,
        /// What `current` pointed at when this deploy started.
        started: String,
        /// What it points at now.
        found: String,
    },
    /// A release had to be rolled back, and the rollback itself failed.
    ///
    /// Carries both halves because either alone leaves an operator unable
    /// to diagnose the machine: `why` is the failure that made a rollback
    /// necessary, and the source is what went wrong performing it.
    RollbackFailed {
        /// Why the rollback was wanted.
        why: String,
        /// What went wrong rolling back.
        source: Box<Error>,
    },
    /// A release was swapped in, something went wrong afterwards, and the
    /// rollback that followed succeeded.
    ///
    /// The deploy still failed, so this is an error and not an
    /// [`crate::deploy::Outcome`] - but the target is healthy on `to`, and
    /// an operator reading only the original failure would have no way to
    /// know a rollback was even attempted. The original stays reachable
    /// through [`core::error::Error::source`].
    RolledBack {
        /// The sha that is live again.
        to: String,
        /// What went wrong before the rollback.
        source: Box<Error>,
    },
    /// A release was swapped in, never came up, and there was no previous
    /// release to fall back to.
    ///
    /// Not a failed rollback: no rollback was attempted, because there was
    /// nothing to attempt one against. This is a target's first deploy, and
    /// it is the failure a new user is most likely to meet, so it says the
    /// whole thing in one sentence rather than being wrapped in another.
    Unverified {
        /// The sheep that did not come up.
        sheep: String,
        /// The release it was left on, because there is no other.
        sha: String,
        /// What went wrong, as a clause that follows the sheep's name.
        why: String,
    },
    /// A release's build command exited without succeeding.
    ///
    /// Deliberately carries only the exit status, not captured output the
    /// way [`Self::Git`] carries `stderr`: a build's own stdout/stderr are
    /// inherited by the child rather than captured (see
    /// [`crate::build::run`]), so an operator watching a deploy sees the
    /// real build log as it happens rather than a blob replayed after the
    /// fact, and there is nothing left here worth capturing a second time.
    Build {
        /// The process's exit status, or `None` if it was killed by a
        /// signal instead of exiting.
        status: Option<i32>,
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
            Self::Raced {
                sheep,
                started,
                found,
            } => write!(
                f,
                "another deploy of {sheep} moved current from {started} to {found} while this \
                 one was building - refusing to swap on top of it, because that release may be \
                 newer than this one"
            ),
            Self::RollbackFailed { why, source } => write!(
                f,
                "rolling back after {why} failed: {source} - current may still point at the \
                 release that was rejected"
            ),
            Self::RolledBack { to, source } => {
                write!(f, "rolled back to {to} after: {source}")
            }
            Self::Unverified { sheep, sha, why } => write!(
                f,
                "{sheep}: {why}, and this is its first deploy, so there is nothing to roll back \
                 to - it is still pointed at {sha}"
            ),
            Self::Build { status } => match status {
                Some(code) => write!(f, "the build exited with status {code}"),
                None => write!(f, "the build was killed by a signal"),
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
            Self::RollbackFailed { source, .. } | Self::RolledBack { source, .. } => {
                Some(&**source)
            }
            Self::Protocol(_)
            | Self::Config(_)
            | Self::Git { .. }
            | Self::Raced { .. }
            | Self::Unverified { .. }
            | Self::Build { .. } => None,
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

    // For `Error::source` on the wrapping variants.
    use core::error::Error as _;

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

    /// fails if a failed rollback reports only one of its two halves. The
    /// failure that made a rollback necessary and the failure of the
    /// rollback itself are both needed: the first says what the machine
    /// was trying to do, the second says what state it is in now.
    #[test]
    fn a_failed_rollback_names_both_failures() {
        let err = Error::RollbackFailed {
            why: "it did not come up".to_owned(),
            source: Box::new(Error::Io {
                path: PathBuf::from("/srv/x/current.tmp"),
                source: std::io::Error::other("file exists"),
            }),
        };
        let shown = err.to_string();
        assert!(shown.contains("it did not come up"), "{shown}");
        assert!(shown.contains("current.tmp"), "{shown}");
    }

    /// fails if a refused swap stops naming what `current` moved between.
    /// The two shas are how an operator tells "another deploy beat me to
    /// it" apart from "something clobbered current".
    #[test]
    fn a_raced_swap_names_both_releases() {
        let err = Error::Raced {
            sheep: "web".to_owned(),
            started: "a1b2c3d".to_owned(),
            found: "e4f5a6b".to_owned(),
        };
        let shown = err.to_string();
        assert!(shown.contains("a1b2c3d"), "{shown}");
        assert!(shown.contains("e4f5a6b"), "{shown}");
    }

    /// fails if a deploy that was rolled back reports only the failure and
    /// not the rollback. An operator reading "unexpected answer from the
    /// shepherd" alone has no way to know a rollback was attempted, let
    /// alone that it worked and what the target is on now.
    #[test]
    fn a_successful_rollback_names_what_is_live_again() {
        let err = Error::RolledBack {
            to: "a1b2c3d".to_owned(),
            source: Box::new(Error::Protocol("a Flock in answer to Describe".to_owned())),
        };
        let shown = err.to_string();
        assert!(shown.contains("rolled back to a1b2c3d"), "{shown}");
        assert!(shown.contains("a Flock in answer to Describe"), "{shown}");
        assert!(
            err.source().is_some(),
            "the original failure stays reachable"
        );
    }

    /// fails if a first deploy that never came up stops naming the sheep,
    /// the release it is stuck on, or what went wrong. This is the failure
    /// a new user is most likely to meet, and the message is all they get.
    #[test]
    fn an_unverified_first_release_names_the_sheep_the_sha_and_the_reason() {
        let err = Error::Unverified {
            sheep: "web".to_owned(),
            sha: "a1b2c3d".to_owned(),
            why: "it did not come up within 90s of the reload".to_owned(),
        };
        let shown = err.to_string();
        assert!(shown.contains("web"), "{shown}");
        assert!(shown.contains("a1b2c3d"), "{shown}");
        assert!(shown.contains("did not come up within 90s"), "{shown}");
    }

    /// fails if the first-deploy failure starts saying the same thing
    /// twice. It used to arrive wrapped in `RollbackFailed`, which framed a
    /// rollback nobody attempted as one that failed, and then said "no
    /// previous release" and "still pointed at" once each in both layers.
    /// One sentence, each fact once.
    #[test]
    fn an_unverified_first_release_says_each_fact_once() {
        let err = Error::Unverified {
            sheep: "web".to_owned(),
            sha: "a1b2c3d".to_owned(),
            why: "it did not come up within 90s of the reload".to_owned(),
        };
        let shown = err.to_string();
        assert_eq!(shown.matches("roll back").count(), 1, "{shown}");
        assert!(!shown.contains("rolling back after"), "{shown}");
    }

    /// fails if a build's exit status stops being named. This is the only
    /// diagnostic `Error::Build` carries at all - its own stdout/stderr are
    /// inherited rather than captured - so losing the status here leaves an
    /// operator with no information whatsoever about why a build failed.
    #[test]
    fn a_build_error_names_a_nonzero_exit_status() {
        let err = Error::Build { status: Some(3) };
        assert!(err.to_string().contains('3'));
    }

    /// fails if the signal-killed branch collapses into the exit-status
    /// wording, or vice versa - the same `Option<i32>` match shape as
    /// `Error::Git`'s, and both arms need their own test for the same
    /// reason `Error::Git`'s two-status shape does.
    #[test]
    fn a_build_error_names_a_signal_kill_distinctly() {
        let err = Error::Build { status: None };
        assert!(err.to_string().contains("signal"));
    }
}
