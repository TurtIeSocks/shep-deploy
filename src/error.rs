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
    /// A rollback put `current` and `deploy.toml` back, and then could not
    /// confirm a reload onto them.
    ///
    /// "Could not confirm" rather than "was refused", because both reach
    /// here and they are not the same: the shepherd can decline the reload
    /// outright, or it can accept one whose reply never comes back. Either
    /// way this crate does not know which release the running instances are
    /// on, which is the whole of what the message has to convey.
    ///
    /// The one state this crate cannot repair on its own, and it is named
    /// rather than glossed: the filesystem and the record agree with each
    /// other, and the process may still be running the release that was
    /// rejected. The shepherd refuses a reload while another is in flight,
    /// so the ordinary cause is a reload that outlived verification, and the
    /// ordinary fix is one command once it settles.
    Split {
        /// The sheep left in this state.
        sheep: String,
        /// The release `current` and `deploy.toml` both name now.
        on: String,
        /// The release one or more instances may still be executing.
        running: String,
        /// Why a rollback was wanted in the first place.
        why: String,
        /// Why the reload could not be issued.
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
    /// A release was swapped in, never came up, and there was no earlier
    /// release to fall back to.
    ///
    /// Not a failed rollback: no rollback was attempted, because there was
    /// nothing to attempt one against. Usually that is a target's first
    /// deploy, which is the failure a new user is most likely to meet - but
    /// not always, so the message says what was looked at rather than
    /// asserting which case it is. A retry after an interrupted deploy
    /// reaches this too, when `current` already names the release being
    /// attempted and `deploy.toml`'s own release has been swept up by
    /// retention. It says the whole thing in one sentence rather than being
    /// wrapped in another.
    Unverified {
        /// The sheep that did not come up.
        sheep: String,
        /// The release it was left on, because there is no other.
        sha: String,
        /// What went wrong, as a clause that follows the sheep's name.
        why: String,
    },
    /// A sheep's first cutover did not come up, and the sheep it was
    /// replacing is still serving.
    ///
    /// Its own variant rather than a reuse of [`Self::Unverified`] because
    /// the situation is different in the way that matters to an operator:
    /// nothing was swapped and nothing was rolled back. What has to be said
    /// instead is the likeliest cause, which is specific to this one moment
    /// in a target's life, and whether the shepherd's persisted record
    /// could be put back.
    ///
    /// `repaired` is not a detail. An accepted `Start` records its config
    /// against the sheep's NAME, and deleting the instance it spawned does
    /// not undo that while the original keeps the name alive, so an
    /// unrepaired roll means a reboot silently brings the sheep back on the
    /// release that was just rejected. That is invisible in `shep flock`,
    /// which is why it is named here rather than left to be discovered.
    CutOver {
        /// The sheep whose cutover was abandoned.
        sheep: String,
        /// Why the newcomer was rejected.
        why: String,
        /// Whether the shepherd's persisted roll was put back.
        repaired: bool,
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
                "rolling back after {why} failed: {source} - the rollback can stop at any of its \
                 three steps, so check `current`, deploy.toml and what the shepherd is actually \
                 running against each other before retrying"
            ),
            Self::Split {
                sheep,
                on,
                running,
                why,
                source,
            } => write!(
                f,
                "{sheep}: {why}, so it was rolled back to {on} - but no reload onto it could be \
                 confirmed ({source}). current and deploy.toml both name {on}; one or more \
                 instances may still be running {running}. The reload that blocked this one has \
                 to finish first: a `stopping` row in `shep flock` means it is still draining, \
                 but the earlier part of a swap shows no row of its own, so wait for the pids in \
                 `shep describe {sheep}` to hold still. Then `shep reload {sheep}` finishes the \
                 rollback."
            ),
            Self::RolledBack { to, source } => {
                write!(f, "rolled back to {to} after: {source}")
            }
            Self::Unverified { sheep, sha, why } => write!(
                f,
                "{sheep}: {why}, and there is no earlier release to roll back to - neither \
                 current nor deploy.toml names one that is still on disk - so it is still \
                 pointed at {sha}"
            ),
            Self::CutOver {
                sheep,
                why,
                repaired,
            } => {
                write!(
                    f,
                    "{sheep}'s first cutover did not come up ({why}), so it was removed and the \
                     original is still running. The first cutover is the one deploy that runs two \
                     instances at once, so an app that does not bind with SO_REUSEPORT cannot take \
                     its own port while the original still holds it. Every deploy after the first \
                     replaces the instance rather than joining it and does not meet this. The \
                     deploy tree is left in place with {sheep}'s first release already built, so \
                     nothing has to be rebuilt to try again."
                )?;
                if !repaired {
                    // The half an operator cannot see. `shep flock` shows a
                    // healthy sheep either way; only the persisted roll is
                    // wrong, and only a restart reveals it.
                    write!(
                        f,
                        " One thing is NOT back as it was: the shepherd recorded the new release \
                         against {sheep} when it accepted the start, and that record could not be \
                         put back. It is correct in the running process and wrong on disk, so a \
                         daemon restart followed by `shep muster` would bring {sheep} back on the \
                         release that was just rejected. Re-register it from its own Flockfile to \
                         correct the record before restarting the shepherd."
                    )?;
                }
                Ok(())
            }
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
            Self::RollbackFailed { source, .. }
            | Self::RolledBack { source, .. }
            | Self::Split { source, .. } => Some(&**source),
            Self::Protocol(_)
            | Self::Config(_)
            | Self::Git { .. }
            | Self::Raced { .. }
            | Self::Unverified { .. }
            | Self::CutOver { .. }
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

    use shep_client::shep_core::protocol::{RpcError, RpcErrorCode};

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

    /// fails if the failed-rollback message keeps blaming the swap. The
    /// rollback has three steps and any of them can be the one that broke;
    /// the measured case had the swap succeed and the reload refused, where
    /// "current may still point at the release that was rejected" sends an
    /// operator to look at the one part that was fine.
    #[test]
    fn a_failed_rollback_does_not_blame_one_step() {
        let err = Error::RollbackFailed {
            why: "it did not come up".to_owned(),
            source: Box::new(Error::Protocol("web is already being reloaded".to_owned())),
        };
        let shown = err.to_string();
        assert!(!shown.contains("current may still point"), "{shown}");
        assert!(shown.contains("deploy.toml"), "{shown}");
    }

    /// A `Split` with the shape the deploy sequence really builds.
    ///
    /// The source is an `Rpc` carrying `Internal`, which is what
    /// `ReloadInFlight` arrives as and the only shape `crate::deploy`'s own
    /// `is_retryable` will let through to a `Split`. A `Protocol` here
    /// would be a state this crate cannot produce, which is how a fixture
    /// stops testing the thing it names.
    fn split() -> Error {
        Error::Split {
            sheep: "web".to_owned(),
            on: "a1b2c3d".to_owned(),
            running: "e4f5a6b".to_owned(),
            why: "it did not come up and stay up, 32s after the reload".to_owned(),
            source: Box::new(Error::Request(RequestError::Rpc(RpcError {
                code: RpcErrorCode::Internal,
                message: "web is already being reloaded".to_owned(),
            }))),
        }
    }

    /// fails if the one state this crate cannot repair stops naming all
    /// three of the things an operator has to compare, or stops saying what
    /// to run. A message that says "something went wrong" here leaves a
    /// half-deployed sheep and no way to reason about it.
    #[test]
    fn a_split_state_names_all_three_and_the_way_out() {
        let shown = split().to_string();
        assert!(shown.contains("current"), "{shown}");
        assert!(shown.contains("deploy.toml"), "{shown}");
        assert!(shown.contains("a1b2c3d"), "{shown}");
        assert!(shown.contains("e4f5a6b"), "{shown}");
        assert!(shown.contains("shep reload web"), "{shown}");
        // Why the rollback was wanted, which used to live in a wrapper
        // around this and is now carried here.
        assert!(shown.contains("did not come up and stay up"), "{shown}");
    }

    /// fails if `Split` stops naming an observable an operator can act on,
    /// or starts claiming one is sufficient. "Once no reload is in flight"
    /// is not something shep reports at all. A `stopping` row is real but
    /// only covers the drain: a swap waiting on readiness shows no row of
    /// its own, so the message has to send the operator somewhere that
    /// covers both, and pids holding still in `describe` does.
    #[test]
    fn a_split_state_names_something_an_operator_can_see() {
        let shown = split().to_string();
        assert!(shown.contains("stopping"), "{shown}");
        assert!(shown.contains("shep describe"), "{shown}");
        assert!(!shown.contains("no reload in flight"), "{shown}");
    }

    /// fails if `Split` collapses a multi-instance flock into one process.
    /// A reload replaces instances one at a time, so what it interrupts is
    /// a mixture - some instances on the new release, some on the old - and
    /// "the running process" describes a flock of one.
    #[test]
    fn a_split_state_does_not_promise_a_single_process() {
        let shown = split().to_string();
        assert!(shown.contains("one or more instances"), "{shown}");
        assert!(!shown.contains("the running process"), "{shown}");
    }

    /// fails if the reload's own failure stops being reachable through the
    /// error chain. `Split` used to flatten it into a string, which loses
    /// the RPC code for anything walking `source()` - the same code
    /// `crate::deploy`'s retry decision is made on.
    #[test]
    fn a_split_state_keeps_the_reload_failure_in_the_chain() {
        let err = split();
        assert!(err.source().is_some(), "{err:?}");
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

    /// fails if a release with nothing to roll back to stops naming the
    /// sheep, the release it is stuck on, or what went wrong. Usually that
    /// is a first deploy, which is the failure a new user is most likely to
    /// meet - but a retry after an interrupted deploy reaches it too, so
    /// the test is named for the variant rather than for one of its causes.
    /// The message is all an operator gets either way.
    #[test]
    fn an_unverified_release_names_the_sheep_the_sha_and_the_reason() {
        let err = Error::Unverified {
            sheep: "web".to_owned(),
            sha: "a1b2c3d".to_owned(),
            why: "it did not come up and stay up, 16s after the reload".to_owned(),
        };
        let shown = err.to_string();
        assert!(shown.contains("web"), "{shown}");
        assert!(shown.contains("a1b2c3d"), "{shown}");
        assert!(shown.contains("did not come up and stay up"), "{shown}");
    }

    /// fails if this failure starts saying the same thing twice. It used to
    /// arrive wrapped in `RollbackFailed`, which framed a rollback nobody
    /// attempted as one that failed, and then said "no previous release"
    /// and "still pointed at" once each in both layers. One sentence, each
    /// fact once.
    #[test]
    fn an_unverified_release_says_each_fact_once() {
        let err = Error::Unverified {
            sheep: "web".to_owned(),
            sha: "a1b2c3d".to_owned(),
            why: "it did not come up and stay up, 16s after the reload".to_owned(),
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
