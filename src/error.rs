//! One error type for the whole binary.
//!
//! A deploy is a single linear pass, fetch through verify, so a single enum
//! is the simplest thing that works. Splitting it per module would buy
//! nothing that the variant names do not already say, and every later task
//! adds variants here rather than introducing a second error type.

use core::fmt;
use std::path::PathBuf;

use shep_client::shep_core::protocol::{RpcErrorCode, SmitError};
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
    /// Some operator-supplied input failed validation, and the message says
    /// which and why.
    ///
    /// Deliberately broad, and named for the kind of mistake rather than for
    /// one file. It is built at 35 sites across ten modules: a `[dog.<name>]`
    /// section, a `deploy.toml` record, a release's own Flockfile, an
    /// unresolvable `user`, an artifact path that would escape its release,
    /// and a bad `--watch` argument, which is not TOML at all.
    ///
    /// It said "a `[dog.deploy]` (or per-sheep override) section could not be
    /// understood" until 2026-08-28, which was true of one site and misleading
    /// about the other 34. That matters more here than elsewhere: this
    /// module's own doc says it exists to be read on its own.
    Config(String),
    /// A smit's own text could not become one at all - too long, empty, or
    /// carrying a control character.
    ///
    /// Reached only if [`crate::smit::publish`]'s own truncation somehow
    /// still leaves something the daemon's [`Smit`](shep_client::shep_core::protocol::Smit)
    /// refuses: a branch name is what this crate builds a smit from, and
    /// git already refuses a control character in a ref name, so this is a
    /// defensive path rather than one either of those inputs can reach in
    /// the ordinary case.
    Smit(SmitError),
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
    /// nothing to attempt one against. That means both candidates are
    /// unusable - `current` already names the release being attempted, and
    /// `deploy.toml`'s own release is no longer on disk, usually because
    /// retention reclaimed it. A retry after an interrupted deploy is the
    /// ordinary way to arrive.
    ///
    /// It used to say "usually a target's first deploy". That is no longer
    /// reachable: `crate::deploy::deploy` refuses a tree whose cutover
    /// never landed, and a cutover that DID land wrote a sha into the
    /// record, so by the time any deploy runs there is always a previous
    /// release named. The message still says what it looked at rather than
    /// asserting which case it is.
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
        /// Why the newcomer was rejected, as one or more whole sentences.
        ///
        /// Whole sentences because only the cutover knows which causes are
        /// plausible: a `Start` the shepherd accepted that produced no row
        /// at all cannot be a port collision, and a message that blamed one
        /// anyway would send an operator after the wrong thing.
        why: String,
        /// Whether every instance the cutover added was removed again.
        removed: bool,
        /// Whether the shepherd's persisted roll was put back.
        repaired: bool,
        /// The deploy tree that was built, resolved.
        ///
        /// Carried rather than spelled `$SHEP_HOME/deploy/<sheep>` in the
        /// message. That variable is usually unset in an operator's own
        /// shell - `ShepPaths::resolve` falls back to `~/.shep` - so the
        /// remedy this error exists to give would expand to `/deploy/web`
        /// and silently do nothing.
        tree: PathBuf,
        /// The shepherd's own failure, when that is what ended the cutover.
        ///
        /// `None` for the ordinary case, a release that did not come up
        /// with a healthy shepherd. `Some` when a request failed mid-watch,
        /// which is the arm most likely to leave a poisoned roll AND least
        /// able to repair it, since [`crate::optin`]'s `undo_start` opens
        /// with a `describe` of its own. That combination is exactly why it
        /// is reported here rather than returned bare: a socket error on
        /// its own says nothing about the record left behind.
        source: Option<Box<Error>>,
    },
    /// A cutover landed, and one or more of the instances it replaced
    /// could not be removed.
    ///
    /// The new release IS serving; only the cleanup failed. That is still an
    /// error rather than a warning, because what it leaves behind does not
    /// stay still. A deploy reloads EVERY instance of the sheep's name and
    /// replaces each from its own spec, so a leftover is respawned from the
    /// PRE-ADOPTION config on every deploy from now on, serving the
    /// operator's checkout code while being actively kept alive by the
    /// supervisor. Stale and forgotten would be tolerable; stale and
    /// restarted on every deploy is a deploy that silently half applies,
    /// and it is the consequence the "leave the original running" design
    /// was reversed over.
    Stranded {
        /// The sheep that was cut over.
        sheep: String,
        /// The release it is now serving.
        sha: String,
        /// The instances that could not be removed.
        ids: Vec<u32>,
    },
    /// A deploy tree the cutover never landed on: nothing has ever been
    /// served from it.
    ///
    /// Its own variant rather than a [`Self::Config`], and the poll loop is
    /// the whole reason. This refusal cannot clear on its own - no fetch,
    /// no commit and no retry changes it, only an operator removing the
    /// tree and running `setup` again - so a loop that could not tell it
    /// from an ordinary config problem would either print the same line
    /// twice a minute forever or would have to recognise it by matching on
    /// its own message text.
    ///
    /// `crate::deploy::set_watch`'s refusal of `auto` on the same tree
    /// stays an ordinary [`Self::Config`]: nothing retries a setting, so
    /// there is nothing there for a caller to recognise.
    NotCutOver {
        /// The sheep whose tree it is.
        sheep: String,
        /// The tree to remove before running `setup` again.
        tree: PathBuf,
    },
    /// A sha an earlier attempt failed on, left alone until the branch
    /// moves.
    ///
    /// Not a deploy that failed - it is a deploy that was not attempted,
    /// and the failure it is about already had its own error, one tick or
    /// one week ago. It is an error rather than an [`crate::deploy::Outcome`]
    /// because nothing was deployed and because the poll loop already mutes
    /// a repeated line: an operator sees it once when the hold starts, and
    /// again if anything about the target changes.
    ///
    /// Only [`crate::deploy::unattended`] returns it. An operator asking
    /// for a deploy by name is asking for exactly the attempt the loop is
    /// declining to make on its own, so that path retries.
    Held {
        /// The sheep being held.
        sheep: String,
        /// The sha it is held at.
        sha: String,
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

impl Error {
    /// Whether a failed request is worth asking again.
    ///
    /// The refusal this whole retry exists for is `ReloadInFlight`, which shep
    /// maps to [`RpcErrorCode::Internal`] under protest: it carries no code of
    /// its own, so `Internal` is as close as this crate can get to naming it,
    /// and everything else that arrives as `Internal` is at least plausibly
    /// transient too. [`RequestError::Timeout`] and `DeadlineExceeded` are the
    /// same shape of answer from the other two layers.
    ///
    /// Everything else is refused at once, which is the half that was missing.
    /// `NotFound` means the selector matched nothing, and asking a second time
    /// cannot make a sheep exist: retrying it burned the entire budget and then
    /// reported a split state claiming a running process there was none of, with
    /// a suggested `shep reload` that fails identically. `Closed` cannot clear
    /// either - this client's connection is gone and nothing here reconnects.
    ///
    /// An unrecognised code is NOT retried. [`RpcErrorCode`] is
    /// `#[non_exhaustive]`, so this arm is the one a future variant lands in,
    /// and failing fast on an unknown code is the mistake that costs a bounded
    /// delay rather than the one that costs an operator a wrong diagnosis.
    ///
    /// Moved here from `crate::deploy` on 2026-08-28: it is a property of the
    /// error, not of deploying, and `crate::verify` needed the same judgement
    /// for the same RPC over the same socket. A second copy there would have
    /// been the third place this line gets drawn.
    #[must_use]
    pub(crate) fn is_retryable(&self) -> bool {
        match self {
            Self::Request(RequestError::Timeout { .. }) => true,
            Self::Request(RequestError::Rpc(rpc)) => matches!(
                rpc.code,
                RpcErrorCode::Internal | RpcErrorCode::DeadlineExceeded
            ),
            _ => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(err) => write!(f, "cannot reach the shepherd: {err}"),
            Self::Request(err) => write!(f, "the shepherd refused a request: {err}"),
            Self::Protocol(what) => write!(f, "unexpected answer from the shepherd: {what}"),
            Self::Config(what) => write!(f, "bad deploy configuration: {what}"),
            Self::Smit(err) => write!(f, "could not publish a smit: {err}"),
            Self::Held { sheep, sha } => write!(
                f,
                "{sheep} is held at {sha}: the last attempt at that commit did not land, so it \
                 is left alone until the branch moves rather than rebuilt and rolled back every \
                 interval. Push a fix, or run `shep deploy {sheep}` to try the same commit again"
            ),
            Self::NotCutOver { sheep, tree } => write!(
                f,
                "{sheep} has a deploy tree but was never cut over to it, so there is nothing to \
                 deploy: its record names no released sha, and nothing has ever been served from \
                 that tree. Deploying now would reload {sheep} at its own checkout and report \
                 success for a release nothing ran. Finish the cutover instead - remove {} and \
                 run `shep-deploy setup {sheep}`.",
                tree.display()
            ),
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
                removed,
                repaired,
                tree,
                source,
            } => {
                write!(f, "{sheep}'s first cutover did not come up. {why}")?;
                if let Some(source) = source {
                    write!(f, " ({source})")?;
                }
                if *removed {
                    // Not "the instance it added has been removed": a `why`
                    // of "no new instance appeared" means none was ever
                    // added, and the two sentences then contradicted each
                    // other one after the other. This is true either way.
                    write!(
                        f,
                        " Nothing this cutover started is left registered, and the original is \
                         still running on the release it was already serving."
                    )?;
                } else {
                    // `undo_start` swallows each delete so the operator gets
                    // the reason they are here rather than a second error
                    // about the cleanup, which means this is the only place
                    // a failed one is ever mentioned.
                    //
                    // No cwd hint. Two different instances can fail to be
                    // deleted here and only one of them is under the deploy
                    // tree: the repair `Start` re-registers the ORIGINAL
                    // config, so the instance it spawns has the operator's
                    // own checkout as its cwd, identical to the survivor's.
                    // A hint that is wrong half the time sends an operator
                    // looking for something that is not there.
                    write!(
                        f,
                        " An instance this cutover started could NOT be removed, so {sheep} may \
                         now be running more instances than it was: `shep describe {sheep}` \
                         lists them with their ids, and `shep delete <id>` removes one. Compare \
                         the count against what {sheep} is configured to run."
                    )?;
                }
                // The half that turns a failed cutover into a false green.
                // `deploy` does not short-circuit on a record naming no
                // release, so it would build, swap, reload the sheep at its
                // own checkout, see a real pid turnover and print success.
                write!(
                    f,
                    " {sheep} is NOT a deploy target: its record names no deployed release and \
                     nothing has ever been served from its tree. Do NOT run `shep deploy \
                     {sheep}` against it - that would reload the sheep at its own checkout and \
                     report success for a release nothing served. Fix what this message names, \
                     remove {}, and run `shep-deploy setup {sheep}` again.",
                    tree.display()
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
            Self::Stranded { sheep, sha, ids } => {
                let removes: Vec<String> =
                    ids.iter().map(|id| format!("`shep delete {id}`")).collect();
                write!(
                    f,
                    "{sheep} was cut over to {sha} and the new release is serving, but {} of the \
                     instances it replaced could not be removed and are still registered. They \
                     are still running the code from {sheep}'s own checkout, and they will not \
                     go away on their own: every deploy from now on reloads EVERY instance of \
                     the name and respawns each from its own spec, so these come back on the \
                     pre-adoption config each time. Remove them: {}.",
                    ids.len(),
                    removes.join(", ")
                )
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
            Self::Smit(err) => Some(err),
            Self::Io { source, .. } => Some(source),
            Self::RollbackFailed { source, .. }
            | Self::RolledBack { source, .. }
            | Self::Split { source, .. } => Some(&**source),
            Self::CutOver { source, .. } => source
                .as_deref()
                .map(|err| err as &(dyn core::error::Error + 'static)),
            Self::Protocol(_)
            | Self::Config(_)
            | Self::NotCutOver { .. }
            | Self::Held { .. }
            | Self::Git { .. }
            | Self::Raced { .. }
            | Self::Unverified { .. }
            | Self::Stranded { .. }
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

    /// fails if a request that can never succeed is retried anyway.
    ///
    /// The retry exists for `ReloadInFlight`, which shep can only report as
    /// `Internal`; a `NotFound` means the selector matched nothing, and asking
    /// again cannot make a sheep exist. Retrying it burned the whole budget
    /// and then reported a split state claiming a running process there was
    /// none of.
    ///
    /// Moved here from `crate::deploy` with the function it tests.
    #[test]
    fn only_a_failure_that_could_clear_is_retried() {
        let rpc = |code| {
            Error::Request(RequestError::Rpc(RpcError {
                code,
                message: "web is already being reloaded".to_owned(),
            }))
        };

        assert!(rpc(RpcErrorCode::Internal).is_retryable());
        assert!(rpc(RpcErrorCode::DeadlineExceeded).is_retryable());
        assert!(
            Error::Request(RequestError::Timeout {
                after: core::time::Duration::from_secs(1)
            })
            .is_retryable()
        );

        assert!(!rpc(RpcErrorCode::NotFound).is_retryable());
        assert!(!rpc(RpcErrorCode::InvalidConfig).is_retryable());
        assert!(!Error::Request(RequestError::Closed).is_retryable());
        assert!(!Error::Protocol("nonsense".to_owned()).is_retryable());
    }
}
