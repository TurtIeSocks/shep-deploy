//! One deploy, start to finish, and the one setting an operator can change
//! without doing one.
//!
//! [`deploy`] runs the sequence the design spec lays out: fetch, compare,
//! worktree add, link the shared files, build, swap `current`, reload,
//! verify, and on failure swap back and reload again.
//!
//! # Nothing before the swap touches the running app
//!
//! That is the property the whole design is built on, and it is worth
//! saying where it comes from rather than trusting the step order alone: a
//! release is built in `releases/<sha>`, a directory the live release does
//! not share, and the live release is reached through `current`, which
//! nothing moves until the build has already exited zero. A fetch that
//! fails, a worktree that cannot be created, a build that exits three -
//! each of those returns from this module with `current` still pointing
//! exactly where it did, and the app still serving from it. This is what
//! replaces the hardcoded sleep between a build and a restart in the deploy
//! scripts this dog exists to retire.
//!
//! # `Reload`, not `Restart`
//!
//! shep's reload runs `SpawnNew`, `AwaitReady`, `DrainOld`, `ReapOld`. The
//! replacement reaches readiness before the old instance drains, so the old
//! release serves throughout and the new one's startup cost - a vite
//! compile, for the app this was designed against - is paid while the old
//! one is still answering.
//!
//! # Two deploys of one sheep
//!
//! There is no lock. What there is instead is one cheap guard: `current` is
//! read when a deploy starts and read again immediately before the swap,
//! and a deploy whose `current` moved in between refuses rather than
//! swapping on top of whatever moved it. The failure it exists for is
//! specific and silent - a second deploy that finished a *newer* release
//! while this one was still building would otherwise be reverted by this
//! one's swap, verified and healthy, with nothing anywhere reporting a
//! problem. The guard is not a lock and does not pretend to be: two deploys
//! can still interleave inside the swap itself. It removes the case that
//! loses a good release, which is the one worth removing before the poll
//! loop exists to make concurrency ordinary.
//!
//! # A stale `current.tmp` is left alone, deliberately
//!
//! [`crate::swap::point_at`] refuses rather than cleaning up a temporary
//! symlink left by an interrupted swap, and this module does not clean one
//! either. The window between creating that link and renaming it is two
//! syscalls wide, so a `current.tmp` on disk is far more likely to mean
//! another deploy of this sheep is running *right now* than that one died
//! inside those two syscalls. Removing it would break the live deploy for
//! the sake of a case that almost never happens. The refusal costs little:
//! it lands at the swap, which is before anything has been disturbed, so
//! the running app keeps serving and the operator's fix is one `rm` once
//! they have checked no deploy is in flight.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::{Instant, sleep};

use shep_client::RequestError;
use shep_client::shep_core::config::AppConfig;
use shep_client::shep_core::protocol::RpcErrorCode;

use crate::daemon::Daemon;
use crate::error::Error;
use crate::paths::Tree;
use crate::state::{State, Verify, Watch};
use crate::verify::Generation;
use crate::{build, flockfile, git, retention, shared, swap, verify};

/// What one deploy did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The tracked branch's head is already deployed.
    UpToDate,
    /// A new release was built, swapped in, and verified.
    Deployed {
        /// The sha now live.
        sha: String,
    },
    /// A new release was swapped in, did not come up, and the previous
    /// release was put back.
    RolledBack {
        /// The sha that is live again.
        to: String,
        /// Why the new release was rejected.
        why: String,
    },
}

/// The slack shep adds to its own reload deadline, per instance.
///
/// Copied from `RELOAD_DEADLINE_SLACK` in shep's `supervisor.rs`, where it
/// is a five-second constant covering "scheduling jitter and nothing else"
/// between the two timeouts below. Named here rather than folded into
/// [`budget`]'s arithmetic so a reader can put the two files side by side.
///
/// `crate::optin`'s cutover budget reads this one rather than copying shep's
/// constant into a second file.
pub(crate) const RELOAD_DEADLINE_SLACK: Duration = Duration::from_secs(5);

/// How long a reload of `instances` instances of this app gets before
/// verification gives up on it.
///
/// This is shep's own formula, not an estimate of it. `Actor::arm_reload_
/// deadline` (`supervisor.rs:3581`) bounds each swap at
/// `listen_timeout + graceful_timeout + RELOAD_DEADLINE_SLACK` and ends the
/// swap when that expires, and `advance_reload` walks the instance queue
/// with `pop_front`, one swap at a time, so a whole reload is that per
/// instance.
///
/// # What is measured and what is still read from a file
///
/// `instances` is measured. It comes from [`Generation::instances`], the
/// live pid count, and NOT from `AppConfig::instances`, which is the count
/// the release's Flockfile asks for. Those differ the moment anyone runs
/// `shep stock <sheep> <n>`, a first-class verb that changes what is running
/// without touching any file this crate reads - and the failure that
/// produced is the one this whole window exists to stop: a file saying one
/// instance against two really running gave a budget of 14s for a reload
/// that took about 17, and a healthy release was rolled back.
///
/// The two timeouts are still read from the release's Flockfile, and that
/// IS an inference, for the same reason
/// [`refuse_ungated_verification`] documents about its own read: nothing
/// re-registers a sheep, and `describe` reports status, pid and uptime but
/// never config, so there is no live source to read them from. The exposure
/// is narrower than the instance count's, because changing either one means
/// editing the Flockfile and redeploying, which is the path that brings the
/// new value here anyway - where `shep stock` is a single command that
/// bypasses it.
///
/// # Why it is copied rather than inferred
///
/// Three rounds of review answered "how long may a healthy reload take" by
/// reasoning about shep's timing from outside, and each answer was wrong
/// somewhere new. The first was fixed at ten seconds. The second derived
/// `listen_timeout * instances` and doubled it, which supplies exactly
/// `listen_timeout` of headroom per instance and so holds only while the
/// drain is shorter than the readiness wait - and shep's own defaults are
/// the other way round, `listen_timeout` three seconds against
/// `graceful_timeout` eight. The drain is not incidental to this crate
/// either: [`crate::verify`] requires every OLD instance to be gone, so the
/// whole of it has to fit inside the window.
///
/// There is no floor. A budget shorter than shep's deadline rolls back
/// healthy releases, and one longer buys nothing, because when that deadline
/// expires shep ends the swap itself - there is nothing left to wait for.
/// The five-second slack is already the margin, and it is shep's number for
/// the same jitter.
///
/// `graceful_timeout` is the drain window and `listen_timeout` bounds
/// readiness for EVERY source: `await_ready` (`probes/ready.rs`) wraps
/// `Channel`, `Probe` and `Heuristic` alike in `tokio::time::timeout` and
/// aborts the swap when it elapses. So a probed release's readiness is not
/// unbounded, as an earlier version of this comment claimed - it is bounded
/// by the same field, and an app that needs a minute to compile its client
/// says so by setting `listen_timeout`, which this reads.
fn budget(app: &AppConfig, instances: u32) -> Duration {
    let per_instance = app.listen_timeout.as_duration()
        + app.graceful_timeout.as_duration()
        + RELOAD_DEADLINE_SLACK;

    // A flock with nothing running still gets one instance's worth. Such a
    // reload replaces nothing and can never turn over, so what this buys is
    // a clean failure at a sensible moment rather than an instant one.
    per_instance.saturating_mul(instances.max(1))
}

/// Deploys the head of `state.branch` to the tree's sheep, rolling back if
/// it does not come up.
///
/// Returns [`Outcome::UpToDate`] without touching anything when the remote
/// head is already deployed, which is the ordinary answer for a poll that
/// found nothing new. `state.deployed` advances only after a verify
/// succeeds, and `deploy.toml` is written only when it does, so an
/// interrupted or rolled-back deploy leaves the record naming whatever is
/// actually serving.
///
/// A release directory that already exists is reused rather than rebuilt
/// from a fresh worktree: it is the leftover of an earlier attempt at this
/// same sha, `git worktree add` would refuse the path anyway, and the build
/// below runs again over it regardless.
///
/// # Errors
/// Anything the steps before the swap return - see [`crate::git::fetch`],
/// [`crate::shared::to_link`], [`crate::flockfile::app_config`] and
/// [`crate::build::run`] - in which case nothing has been disturbed and the
/// old release is still serving. [`Error::Raced`] if `current` moved while
/// this deploy was preparing, which is refused rather than swapped over.
///
/// After the swap, an error from the reload or from verification is rolled
/// back exactly as a failed verification is - swap, reload, correct the
/// record - and then returned as [`Error::RolledBack`], which names what is
/// live again and keeps the original reachable through
/// [`core::error::Error::source`]. The deploy still failed, so it is still
/// an error, but an operator reading it can tell a rollback happened.
///
/// A failure BEFORE the shepherd accepted the reload - the generation
/// capture, or the reload itself being refused - means nothing was ever
/// started on the new release, so `current` is put back and that failure is
/// returned as it arrived, unwrapped. The machine is left exactly as it was
/// before the deploy, which is why there is nothing extra to say about it.
///
/// A rollback that itself fails gives one of three errors, and a caller
/// that wants to detect "the rollback did not work" has to accept all of
/// them rather than matching [`Error::RollbackFailed`] alone:
///
/// - [`Error::Split`], bare, when the reload could not be issued and the
///   failure was one that might yet clear. It carries both halves itself,
///   which is why it is not wrapped.
/// - [`Error::RollbackFailed`], wrapping anything else the rollback met -
///   a swap that could not be made, a record that could not be written, a
///   reload refused for a reason that can never clear.
/// - [`Error::Unverified`], bare, when there was no earlier release to
///   return to at all. No rollback was attempted, so nothing frames it as
///   one that failed.
///
/// [`Error::RollbackFailed`] is also what a failed `undo_swap` gives, for
/// the same reason it wraps a failed `restore`: `current` is left naming a
/// release nothing is running.
///
/// One error here does NOT mean the deploy failed. [`Error::Io`] from
/// writing `deploy.toml`, which happens only after a successful verify, is
/// returned instead of [`Outcome::Deployed`] even though the new release is
/// live and verified. Nothing is rolled back, deliberately: there is nothing
/// to undo and undoing would take a working release out of service. Only the
/// record lags, so the next deploy of the same sha repeats the work and
/// writes it again. A caller that treats every error as "the old release is
/// still serving" is wrong about this one.
///
/// `keep` is the retention count: once a deploy verifies, [`crate::retention`]
/// reclaims every release beyond the newest `keep`, sparing whatever
/// `current` names regardless of age. A prune failure is reported with
/// `eprintln!` rather than returned - see that module's own doc - so it
/// never turns a deploy that already succeeded into one this function
/// reports as failed.
pub async fn deploy<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    keep: usize,
) -> Result<Outcome, Error> {
    let sheep = tree.sheep();

    // The one state where a deploy is not merely wrong but silently
    // convincing, and the reason it is refused in code rather than in prose.
    //
    // `deployed` is None on an existing tree only when the cutover never
    // landed: `prepare` writes the record with None and `cut_over` is what
    // fills it in, so nothing else produces this. The sheep is therefore
    // still registered at the operator's OWN checkout. A deploy from here
    // builds, swaps `current`, and reloads the sheep BY NAME - which really
    // does replace that instance, so `crate::verify::wait` sees a full pid
    // turnover, the deploy reports success for a release nothing ever
    // served, and the sha is written into the record.
    //
    // Three messages already tell an operator not to do this, and a fourth
    // would not help. The poll loop no longer arrives here on its own -
    // `prepare` writes `watch = "manual"` and the cutover is what promotes
    // it, so an abandoned tree is passed over rather than refused once an
    // interval forever - but this stays, because `shep deploy <sheep>` is
    // one command away and a record can be edited by hand.
    if state.deployed.is_none() {
        return Err(Error::Config(format!(
            "{sheep} has a deploy tree but was never cut over to it, so there is nothing to \
             deploy: its record names no released sha, and nothing has ever been served from \
             that tree. Deploying now would reload {sheep} at its own checkout and report \
             success for a release nothing ran. Finish the cutover instead - remove {} and run \
             `shep-deploy setup {sheep}`.",
            tree.root().display()
        )));
    }

    let started_at = swap::resolve(&tree.current())?;

    git::fetch(&tree.git(), &state.remote)?;
    let head = head_of(tree, &state.branch)?;
    if state.deployed.as_deref() == Some(head.as_str()) {
        return Ok(Outcome::UpToDate);
    }

    let release = tree.release(&head);
    if !release.exists() {
        git::worktree_add(&tree.git(), &release, &head)?;
    }
    shared::link_cache(&release, &tree.cache_target())?;
    shared::link_into(
        &release,
        &state.checkout,
        &shared::to_link(&state.checkout)?,
    )?;

    let app = flockfile::app_config(&release, sheep)?;
    refuse_ungated_verification(sheep, &app, state.verify)?;
    let spec = flockfile::build_spec(&release)?;
    build::run(sheep, &release, &spec, app.user.as_deref()).await?;

    // Nothing above here has touched the running app. Everything below has.
    //
    // Both sides of this comparison are `read_link` of the same file by the
    // same process, so they are the same spelling of any path that has not
    // changed - unlike `roll_back`'s check, which sees text from two
    // sources and compares shas for that reason.
    let previous = swap::resolve(&tree.current())?;
    if previous != started_at {
        return Err(Error::Raced {
            sheep: sheep.to_owned(),
            started: named(started_at.as_deref()),
            found: named(previous.as_deref()),
        });
    }
    swap::point_at(&tree.current(), &release)?;

    // THE BOUNDARY. Every step from here that can fail and might need
    // undoing lives inside `land`, so that this one `match` is the only
    // place that decides what to undo. The boundary used to sit around a
    // reload-and-verify call with the generation capture outside it, and
    // that one line outside was enough to leave `current` on a release
    // nothing had verified: see [`land`].
    match land(daemon, sheep, &app, state.verify).await {
        // The record write below is the one fallible thing past the swap
        // that `land` does not own, and it is deliberate rather than a
        // leak. It runs only after a verify has SUCCEEDED, so the new
        // release is up and serving: there is nothing to undo, and undoing
        // it would take a healthy release out of service. What its failure
        // costs is a record that lags, which the next poll resolves by
        // deploying the same sha again - wasteful, and safe. That is why it
        // is a `?` here rather than a `Landed` variant.
        Landed::Verified => {
            state.deployed = Some(head.clone());
            state.write(&tree.state_file())?;
            // After the record, and never fatal. See `crate::retention`'s
            // own doc for both halves: full turnover means nothing is
            // still executing from an older release, and a deploy that has
            // already verified and recorded itself did succeed.
            if let Err(err) = retention::prune(tree, keep) {
                eprintln!("shep-deploy: {sheep}: could not reclaim old releases: {err}");
            }
            Ok(Outcome::Deployed { sha: head })
        }
        // Nothing was ever started on the new release, so there is nothing
        // to reload onto the old one - only a symlink to put back.
        Landed::NotStarted(source) => {
            undo_swap(tree, previous.as_deref(), &source)?;
            Err(source)
        }
        Landed::NotVerified { why, patience } => {
            let to = roll_back(
                daemon,
                tree,
                state,
                previous.as_deref(),
                &head,
                &why,
                patience,
            )
            .await?;
            Ok(Outcome::RolledBack { to, why })
        }
        // The same rollback as above, not a lesser one. This error arrived
        // after a reload the shepherd accepted, so the new release may be
        // running: shep spawns the replacement, waits for it, and drains
        // the old instance, and only then did a `describe` fail. Moving a
        // symlink does not move a running process, so a swap without a
        // reload would leave the daemon on the new code while `current`
        // and `deploy.toml` both name the old one.
        Landed::Failed { source, patience } => {
            let to = roll_back(
                daemon,
                tree,
                state,
                previous.as_deref(),
                &head,
                &source.to_string(),
                patience,
            )
            .await?;
            Err(Error::RolledBack {
                to,
                source: Box::new(source),
            })
        }
    }
}

/// What became of a release between the swap and the verdict.
///
/// The distinction the whole enum exists for is whether the shepherd ever
/// ACCEPTED the reload, because that is what decides how much has to be
/// undone. Before it, the swap is the only thing that moved and putting
/// `current` back restores the machine exactly. After it, a process may be
/// running the new release, and a symlink moving back does not move a
/// running process.
///
/// That question is NOT the same as whether the reload request returned an
/// error, and reading it off `Result::is_err` was wrong in the direction
/// that leaves a machine split: see [`never_reached_the_shepherd`]. Only a
/// failure that is known to have happened before the shepherd could act
/// counts as [`Self::NotStarted`].
enum Landed {
    /// The flock turned over and the new release is serving.
    Verified,
    /// The reload was never issued at all, so nothing was started on the
    /// new release.
    NotStarted(Error),
    /// The reload was issued, and the release did not come up.
    NotVerified {
        /// What to tell an operator, measured rather than budgeted.
        why: String,
        /// How long the rollback's own reload may keep retrying.
        patience: Duration,
    },
    /// The reload was issued, and something failed after it.
    Failed {
        /// What failed.
        source: Error,
        /// As [`Self::NotVerified::patience`].
        patience: Duration,
    },
}

/// Reloads `sheep` onto the release that has just been swapped in, and
/// waits for the verdict `mode` asks for.
///
/// Every failure a deploy can meet after the swap AND might have to undo
/// happens here, and that is the point rather than tidiness. This function
/// cannot return an error: each failure is a [`Landed`] variant, so the
/// caller's `match` is exhaustive over "what has to be undone" instead of
/// over "what went wrong", and a step added here later cannot quietly
/// escape the rollback decision the way `Generation::of` did - one `?`
/// outside the boundary left `current` on a release nothing had verified,
/// `deploy.toml` on the old one, no reload ever sent, and any later restart
/// bringing the sheep up on the unverified release.
///
/// One fallible step past the swap is deliberately NOT here: writing the
/// record after a successful verify. It has nothing to undo, because by
/// then the new release is up and serving. `deploy`'s own `Verified` arm
/// says so where that write is.
///
/// The generation is captured before the reload, and both readings of it
/// matter: [`crate::verify::wait`] needs to know which processes were
/// already serving, and [`budget`] needs to know how many of them there
/// are.
async fn land<D: Daemon>(daemon: &D, sheep: &str, app: &AppConfig, mode: Verify) -> Landed {
    let before = match Generation::of(daemon, sheep).await {
        Ok(before) => before,
        Err(source) => return Landed::NotStarted(source),
    };
    let patience = budget(app, before.instances());

    if let Err(source) = daemon.reload(sheep).await {
        return if never_reached_the_shepherd(&source) {
            Landed::NotStarted(source)
        } else {
            Landed::Failed { source, patience }
        };
    }
    // After the RPC returns, so "{n}s after the reload" is measured from
    // the reload rather than from a request before it.
    let reloaded_at = Instant::now();

    match verify::wait(daemon, sheep, mode, &before, patience).await {
        Ok(true) => Landed::Verified,
        // What actually elapsed, not the budget. An `Alive` verdict takes
        // its turnover wait PLUS the dwell, so quoting the budget
        // understated the wait every time that mode failed.
        Ok(false) => Landed::NotVerified {
            why: format!(
                "it did not come up and stay up, {}s after the reload",
                reloaded_at.elapsed().as_secs()
            ),
            patience,
        },
        Err(source) => Landed::Failed { source, patience },
    }
}

/// Whether a failed reload request definitely never started a swap.
///
/// An allowlist, not a denylist, and that direction is the whole of the
/// decision: being wrong here in the "nothing started" direction leaves a
/// process running the new release with `current` pointed back at the old
/// one and no reload ever sent, which is the split state
/// [`Error::Split`] exists to name. Being wrong the other way costs one
/// needless reload of a healthy sheep. So anything not known to have failed
/// before the shepherd could act is treated as though it might have acted.
///
/// Two shapes qualify, both from [`RequestError`]'s own documented meaning:
///
/// - `Rpc`, which is "the daemon accepted the request and answered it with a
///   structured error". shep answers a reload with an ACCEPTANCE before it
///   spawns anything (`handle_reload` refuses on the presence of the map key
///   first), so an error answer means no swap began. This is the case
///   `ReloadInFlight` arrives as.
/// - `Wire`, which is the request body failing to encode. It never left this
///   process.
///
/// Everything else is ambiguous and is treated as an acceptance that may
/// have happened. `Timeout` is "no reply within the deadline plus
/// `DEADLINE_GRACE`", five seconds and two, which is a real window in which
/// the shepherd can have accepted and begun spawning from the new `current`.
/// `Closed` is the connection dropping before the reply arrived, which says
/// nothing about whether the request was acted on. [`Error::Protocol`] means
/// the shepherd answered SOMETHING, so it processed the request, and a
/// daemon answering the wrong variant tells us nothing about what it did.
/// `RequestError` is `#[non_exhaustive]`, so a variant added later lands in
/// the same fail-safe direction rather than in this list.
///
/// [`is_retryable`] draws its own line in the same place for the same
/// reason, and the two disagreeing is what let this through: the rollback's
/// reload already treated `Timeout` as "might yet clear" while the deploy's
/// reload treated it as "never happened".
fn never_reached_the_shepherd(err: &Error) -> bool {
    matches!(
        err,
        Error::Request(RequestError::Rpc(_) | RequestError::Wire(_))
    )
}

/// Puts `current` back after a deploy that never started anything.
///
/// No reload, deliberately, and no [`Error::Split`]. Nothing was spawned on
/// the release this swap installed, so the running process is the one that
/// was already there and the symlink is the only thing out of step; once it
/// is back, `current`, `deploy.toml` and the process all agree again and
/// there is nothing left for an operator to do. Sending a reload here would
/// restart a healthy sheep for no reason, and reporting a split state would
/// describe a machine that is not split - the variant reserved for what this
/// crate cannot repair, spent on something it just did.
///
/// `previous` of `None` is a target's first deploy: `current` names the new
/// release and there is nothing earlier to name instead, so it is left
/// where it is.
///
/// # Residual, and it is the same one the module doc names for
/// `current.tmp`
///
/// Between the swap and the reload there is a window a handful of syscalls
/// and one RPC wide. A reload started by somebody else inside it would spawn
/// from the new `current`, and undoing the swap would then leave that
/// process on a release `current` no longer names. Nothing here detects
/// that. The check it would take - re-reading the flock and comparing pids -
/// cannot tell that case apart from an ordinary crash restart, so it would
/// report a split state for a healthy machine, which is the defect this
/// function exists to stop rather than a cure for it.
///
/// # Errors
/// [`Error::RollbackFailed`] carrying `why` if `current` cannot be put back,
/// which is the one case here that does leave something for an operator:
/// `current` naming a release nothing is running.
fn undo_swap(tree: &Tree, previous: Option<&Path>, why: &Error) -> Result<(), Error> {
    let Some(previous) = previous else {
        return Ok(());
    };

    swap::point_at(&tree.current(), previous).map_err(|source| Error::RollbackFailed {
        why: why.to_string(),
        source: Box::new(source),
    })
}

/// Sets `watch` on the target and records it, without deploying.
///
/// The verb an operator reaches this through is `deploy`, so it is worth
/// being explicit: this function takes no [`Daemon`] at all, and therefore
/// cannot start, reload or restart anything even by accident. `manual` is
/// what somebody sets in the middle of an incident, and a deploy firing at
/// that moment is the exact opposite of what they asked for.
///
/// # Errors
/// [`Error::Io`] if `deploy.toml` cannot be written - see
/// [`State::write`]. `state` is updated in memory either way.
pub fn set_watch(tree: &Tree, state: &mut State, watch: Watch) -> Result<(), Error> {
    state.watch = watch;
    state.write(&tree.state_file())
}

/// The sha `branch` points at in the tree's bare clone, with the one
/// failure that means "the branch is gone" said in words.
///
/// Only that one failure is reworded, matching what
/// [`crate::git::current_branch`] does for a detached `HEAD`: `git
/// rev-parse --verify` answers a missing ref with `fatal: Needed a single
/// revision`, which names the branch without ever saying the branch is not
/// there. Every other failure - the bare clone is not a repository at all,
/// say - is left exactly as git reported it, so a real breakage is never
/// mislabelled as a deleted branch.
///
/// The fetch that precedes this call prunes, so a branch deleted upstream
/// really has disappeared from the clone by the time this runs. That is the
/// whole point of pruning: without it this would keep resolving a stale sha
/// and the target would stall silently forever.
fn head_of(tree: &Tree, branch: &str) -> Result<String, Error> {
    match git::remote_head(&tree.git(), branch) {
        Ok(sha) => Ok(sha),
        Err(Error::Git {
            command,
            status,
            stderr,
        }) if stderr.contains("Needed a single revision") => Err(Error::Git {
            command,
            status,
            stderr: format!(
                "the fetch succeeded but the remote has no branch named {branch:?} - it was \
                 deleted or renamed upstream, or never existed - so there is nothing to deploy: \
                 {stderr}"
            ),
        }),
        Err(other) => Err(other),
    }
}

/// Refuses a `Probed` deploy of a sheep whose readiness is not gated by
/// anything.
///
/// `Probed` means "wait for the sheep to reach `Online`", and `Online` is
/// only a health verdict when something gates it. shep has two such gates
/// and either will do: a `readiness_probe`, or `wait_ready`, which makes it
/// wait for the app to say so on the shepherd channel. With neither, shep
/// waits out the app's `listen_timeout` and calls the process `Online`
/// because it has not died yet, so a `Probed` deploy verifies nothing while
/// claiming to verify the most.
///
/// Refused before the build and long before the swap, so a misconfigured
/// target costs an operator a message rather than a deploy. The message
/// names all three ways out, `verify = "alive"` among them, because that is
/// the deliberate, visible downgrade the design offers rather than a synonym
/// for going without a check.
///
/// # This is a heuristic, and it cannot be more
///
/// It reads the app definition from the release's own Flockfile, which is
/// not necessarily the config the shepherd is running. Nothing in this crate
/// re-registers a sheep, so the readiness gate a reload actually uses is
/// whatever was registered when the sheep was started, and
/// [`Daemon::describe`] reports status, pid and uptime - never config. There
/// is no request that would answer the real question.
///
/// So the check catches the case worth catching, a target whose Flockfile
/// declares no gate at all, and misses the asymmetric one: a sheep
/// REGISTERED without a gate whose release Flockfile has since added one
/// passes here and then gets `listen_timeout`-elapsed verification under the
/// `probed` label. The turnover requirement in [`crate::verify`] still
/// applies to it, so what it loses is the readiness half of the check, not
/// the whole of it.
///
/// # Errors
/// [`Error::Config`] naming the sheep and every way out.
fn refuse_ungated_verification(sheep: &str, app: &AppConfig, mode: Verify) -> Result<(), Error> {
    if mode == Verify::Probed && app.readiness_probe.is_none() && !app.wait_ready {
        return Err(Error::Config(format!(
            "{sheep} has verify = \"probed\" but neither a readiness_probe nor wait_ready, so \
             there is nothing for a deploy to wait on: shep reports a sheep with no readiness \
             gate Online as soon as it has not died, which would verify every release, including \
             a broken one. Add a [readiness_probe] to its Flockfile, or set wait_ready if the app \
             announces itself on the channel, or set verify = \"alive\" in deploy.toml to accept \
             the weaker check deliberately."
        )));
    }
    Ok(())
}

/// Puts `previous` back after `attempted` was rejected, and answers with
/// the sha the target is live on again.
///
/// A failure while rolling back is wrapped in [`Error::RollbackFailed`]
/// alongside `why`, at this one place, so a caller never has to choose
/// between the failure that made a rollback necessary and the failure of
/// the rollback itself. Both are what an operator needs.
///
/// [`Error::Split`] is the exception and passes through untouched. It
/// already carries `why`, and wrapping it produced a message that ended by
/// telling an operator to go and compare the three things its inner half
/// had just finished telling them.
///
/// "Nothing to roll back to" is not wrapped that way, because no rollback
/// was attempted: there was nothing to attempt one against.
/// [`Error::Unverified`] says the whole thing in one sentence instead. It
/// is the failure a new user is most likely to meet, a first deploy that
/// never comes up, and framing it as a rollback that failed described a
/// machine nobody could recognise.
///
/// Whether there is anything to roll back to at all is
/// [`rollback_target`]'s decision, and it reads both `current` and
/// `deploy.toml` rather than only the first.
///
/// It compares shas, not paths. Both spellings name the same release when
/// they agree, but `previous` is link text read off disk and `attempted` is
/// a sha this process just resolved, and the two need not be written the
/// same way: `$SHEP_HOME` reaches this crate through
/// [`std::path::absolute`], which does not resolve symlinks, and the dog
/// and the operator's own CLI can each arrive at a different literal path
/// for one directory. A sha comparison has no such degree of freedom.
///
/// # Errors
/// [`Error::Unverified`] if [`rollback_target`] finds nothing to roll back
/// to: neither `current` nor `deploy.toml` names an earlier release that is
/// still on disk.
/// [`Error::Split`], unwrapped, if that is what [`restore`] returned.
/// [`Error::RollbackFailed`] wrapping anything else it returned.
async fn roll_back<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    previous: Option<&Path>,
    attempted: &str,
    why: &str,
    patience: Duration,
) -> Result<String, Error> {
    let Some(previous) = rollback_target(tree, state, previous, attempted) else {
        return Err(Error::Unverified {
            sheep: tree.sheep().to_owned(),
            sha: attempted.to_owned(),
            why: why.to_owned(),
        });
    };

    let to = sha_of(&previous);
    restore(daemon, tree, state, &previous, attempted, why, patience)
        .await
        // `Split` already carries `why`, and says more about the machine
        // than the wrapper's own tail could - wrapping it produced a
        // message that ended by telling an operator to go and check the
        // three things the inner half had just told them.
        .map_err(|source| match source {
            split @ Error::Split { .. } => split,
            source => Error::RollbackFailed {
                why: why.to_owned(),
                source: Box::new(source),
            },
        })?;

    Ok(to)
}

/// Which release a rollback should return to, or `None` when there is
/// genuinely no earlier one.
///
/// Two places know of an earlier release and they can disagree, so both are
/// asked. `current` is the first choice: it is what was serving a moment
/// ago. `deploy.toml` is the second, and it is not a nicety - a deploy
/// killed between its swap and its `State::write` is precisely the
/// interruption [`restore`]'s record correction exists to repair, and it
/// leaves `current` already naming the release being attempted while the
/// record still names a good one. Reading `current` alone, the retry
/// concluded that `previous == attempted` meant a first deploy with nothing
/// to roll back to, and left a release that would not come up serving.
///
/// A candidate has to name a release other than the one that just failed,
/// and it has to still be on disk. Both conditions apply to both sources,
/// which they did not at first: retention removes old worktrees, so neither
/// a sha in `deploy.toml` nor the target of `current` is a promise that
/// anything is there to point at, and a rollback onto a path that does not
/// exist would swap `current` to a dangling link - a sheep whose `cwd`
/// resolves to nothing, which is worse than the release that would not come
/// up. Guarding only the record's branch left the first source able to do
/// exactly the harm the guard was added for.
fn rollback_target(
    tree: &Tree,
    state: &State,
    previous: Option<&Path>,
    attempted: &str,
) -> Option<PathBuf> {
    let usable =
        |release: PathBuf| (sha_of(&release) != attempted && release.exists()).then_some(release);

    if let Some(release) = previous.map(Path::to_path_buf).and_then(usable) {
        return Some(release);
    }

    let recorded = state.deployed.as_deref()?;
    usable(tree.release(recorded))
}

/// Points `current` back at `previous`, corrects the record to match, and
/// reloads onto it.
///
/// The three steps are in that order and none of them is optional.
///
/// The record is written BEFORE the reload rather than after it, which is
/// the one ordering choice here worth arguing. Written after, a reload the
/// shepherd would not accept leaves `current` naming the old release and
/// `deploy.toml` naming whatever it named before - two disagreements to
/// explain instead of one. Written first, the filesystem and the record
/// always agree with each other, and the only thing that can be out of step
/// is the running process, which is exactly what [`Error::Split`] says. It
/// also leaves the record naming a release the next poll will see as behind
/// the branch head, so an unattended target retries rather than settling.
///
/// The reload is not best-effort: a swap back that is never reloaded leaves
/// the old code on disk and the new, unhealthy instance still running, and
/// an operator told the rollback succeeded. Moving a symlink does not move
/// a running process.
///
/// The record is corrected rather than assumed already right. `deploy` only
/// advances `state.deployed` after a verify, so it usually already names
/// `previous` - but a deploy killed between its `Reload` and its
/// `State::write` leaves `deploy.toml` lagging `current`, and without this
/// write the next rollback would repair the filesystem and leave the record
/// wrong indefinitely. Written only when it actually differs, so the
/// ordinary rollback costs no write at all.
///
/// # Errors
/// [`Error::Io`] if `current` cannot be repointed or `deploy.toml` cannot be
/// written. [`Error::Split`] if the reload could not be issued within
/// `patience` and the failure was one that might yet have cleared - see
/// [`reload_until`] for which those are, and [`Error::Split`] for what an
/// operator is told. The reload's own error, unchanged and unwrapped, if it
/// was one that never could have cleared: a `NotFound` is that failure and
/// not a split, since there is no sheep for a process to be running.
async fn restore<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    previous: &Path,
    attempted: &str,
    why: &str,
    patience: Duration,
) -> Result<(), Error> {
    let sheep = tree.sheep();
    let to = sha_of(previous);

    swap::point_at(&tree.current(), previous)?;

    if state.deployed.as_deref() != Some(to.as_str()) {
        state.deployed = Some(to.clone());
        state.write(&tree.state_file())?;
    }

    match reload_until(daemon, sheep, patience).await {
        Ok(()) => Ok(()),
        // A failure that could have cleared and did not is the split state:
        // the reload may yet be pending, so the process may be on either
        // release. A failure that could never clear is not - it is just
        // that failure, and dressing it as a split would claim a running
        // process that a `NotFound` says is not there.
        Err(source) if is_retryable(&source) => Err(Error::Split {
            sheep: sheep.to_owned(),
            on: to,
            running: attempted.to_owned(),
            why: why.to_owned(),
            source: Box::new(source),
        }),
        Err(source) => Err(source),
    }
}

/// How long to leave between attempts at a reload the shepherd would not
/// take.
const RETRY_EVERY: Duration = Duration::from_millis(500);

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
fn is_retryable(err: &Error) -> bool {
    match err {
        Error::Request(RequestError::Timeout { .. }) => true,
        Error::Request(RequestError::Rpc(rpc)) => matches!(
            rpc.code,
            RpcErrorCode::Internal | RpcErrorCode::DeadlineExceeded
        ),
        _ => false,
    }
}

/// Reloads `sheep`, retrying a failure that could clear until `patience`
/// runs out.
///
/// The refusal this exists for is transient and self-clearing: the shepherd
/// will not start a reload while another is in flight, and the one in flight
/// is precisely what verification just gave up waiting for. Retrying is
/// therefore the difference between a rollback that lands a few seconds late
/// and one that cannot happen at all. What is retried and what is not is
/// [`is_retryable`]'s decision.
///
/// # Errors
/// The failure, at once if it could never clear, or the last one after
/// `patience` has run out.
async fn reload_until<D: Daemon>(daemon: &D, sheep: &str, patience: Duration) -> Result<(), Error> {
    let deadline = Instant::now() + patience;
    loop {
        match daemon.reload(sheep).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if !is_retryable(&err) || Instant::now() >= deadline {
                    return Err(err);
                }
                sleep(RETRY_EVERY).await;
            }
        }
    }
}

/// A release path as it reads in a message, or "nothing" when `current`
/// has never been pointed anywhere - the zero-configuration case.
fn named(release: Option<&Path>) -> String {
    release.map_or_else(|| "nothing".to_owned(), sha_of)
}

/// The sha a release path names, which is its last component - see
/// [`Tree::release`]. Falls back to the whole path when that component is
/// not readable as a string, so an odd path degrades to a longer message
/// rather than to no message.
fn sha_of(release: &Path) -> String {
    release.file_name().map_or_else(
        || release.display().to_string(),
        |sha| sha.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::error::Error as _;
    use std::cell::Cell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use shep_client::shep_core::config::AppConfig;
    use shep_client::shep_core::protocol::{ProcessInfo, RpcError};
    use shep_client::shep_core::status::ProcStatus;
    use shep_client::shep_core::values::UpDuration;
    use tempfile::TempDir;

    use crate::swap;

    /// The fixture app: one sheep named `web`, with a readiness probe,
    /// because `Verify::Probed` refuses a sheep without one and `Probed` is
    /// the default every test here runs under.
    ///
    /// The probe's own target is never executed by anything in this file -
    /// no test here runs a real shepherd - so `true` is honest rather than
    /// lazy: what these tests need is an app that HAS a probe.
    const FLOCKFILE: &str = "[[app]]\nname = 'web'\nscript = './run.sh'\n\n\
                             [app.readiness_probe]\nkind = 'exec'\ntarget = 'true'\n";

    /// Runs a git subcommand for fixture setup, panicking if it fails.
    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// `dir`'s current `HEAD` sha.
    fn head_of(dir: &Path) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        String::from_utf8(out.stdout)
            .expect("utf-8 sha")
            .trim()
            .to_owned()
    }

    /// A deploy target with a bare clone, an origin repo, and one release
    /// already live.
    struct Fixture {
        /// The `$SHEP_HOME` the tree is rooted in. Held to keep it alive
        /// for the test's duration, and read by the one test that needs a
        /// second spelling of the same directory.
        home: TempDir,
        origin: TempDir,
        tree: Tree,
        state: State,
    }

    /// An origin repo with a Flockfile declaring one app named `web`, and
    /// a bare clone fetched from it - but no release built and no
    /// `current` at all, which is a target the moment before its first
    /// deploy.
    /// A sha whose release directory is not on disk.
    ///
    /// What a record naming a release retention has already reclaimed looks
    /// like, and the only way a real target reaches "there is nothing to
    /// roll back to" now that `deploy` refuses a tree the cutover never
    /// landed on. A `deployed` of `None` used to stand in for this, and no
    /// longer can.
    const RECLAIMED: &str = "0000000000000000000000000000000000000000";

    fn fixture_before_any_release() -> Fixture {
        let home = tempfile::tempdir().expect("tempdir");
        let origin = tempfile::tempdir().expect("tempdir");

        run(origin.path(), &["init", "-q", "-b", "main"]);
        run(origin.path(), &["config", "user.email", "test@example.com"]);
        run(origin.path(), &["config", "user.name", "test"]);
        fs::write(origin.path().join("Flockfile.toml"), FLOCKFILE).expect("write Flockfile");
        run(origin.path(), &["add", "."]);
        run(origin.path(), &["commit", "-q", "-m", "first"]);

        let tree = Tree::for_sheep(home.path(), "web");
        fs::create_dir_all(tree.git()).expect("create git dir");
        run(&tree.git(), &["init", "-q", "--bare"]);

        let remote = origin.path().to_str().expect("utf-8 path").to_owned();
        crate::git::fetch(&tree.git(), &remote).expect("fetch");

        let state = State {
            remote,
            branch: "main".to_owned(),
            deployed: None,
            verify: crate::state::Verify::Probed,
            watch: crate::state::Watch::Auto,
            origin_cwd: None,
            origin_script: None,
            checkout: origin.path().to_owned(),
        };

        // A real target always has one on disk, because that is where its
        // `State` was read from. Writing it here keeps assertions about
        // what `deploy.toml` says meaningful rather than vacuous.
        state.write(&tree.state_file()).expect("write deploy.toml");

        Fixture {
            home,
            origin,
            tree,
            state,
        }
    }

    /// [`fixture_before_any_release`] with its first commit already built,
    /// live under `current`, and recorded in the state.
    fn fixture_with_previous_release() -> Fixture {
        let mut fixture = fixture_before_any_release();
        let first = crate::git::remote_head(&fixture.tree.git(), "main").expect("head");
        install_release(&fixture, &first);
        fixture.state.deployed = Some(first);
        fixture
            .state
            .write(&fixture.tree.state_file())
            .expect("write deploy.toml");
        fixture
    }

    /// Builds `sha`'s worktree and points `current` at it, the way a
    /// successful deploy would have.
    fn install_release(fixture: &Fixture, sha: &str) {
        let tree = &fixture.tree;
        if !tree.release(sha).exists() {
            crate::git::worktree_add(&tree.git(), &tree.release(sha), sha).expect("worktree");
        }
        swap::point_at(&tree.current(), &tree.release(sha)).expect("swap");
    }

    /// Adds a commit to the fixture's origin and returns its sha.
    fn commit_on_origin(fixture: &Fixture, name: &str) -> String {
        fs::write(fixture.origin.path().join(name), "x").expect("write");
        run(fixture.origin.path(), &["add", "."]);
        run(fixture.origin.path(), &["commit", "-q", "-m", name]);
        head_of(fixture.origin.path())
    }

    /// A [`Daemon`] that models what a reload actually does to the flock:
    /// it replaces the running process with a different one, under a
    /// different pid. Counts its reloads, because a rollback that moves the
    /// symlink and never reloads is invisible to every filesystem
    /// assertion.
    ///
    /// A fake that answered `describe` however a test wanted is how the
    /// generation bug survived two review rounds: verification asked "is
    /// anything Online" and a fake always had something Online to show it.
    /// Every constructor below therefore describes a real shepherd
    /// behaviour rather than a convenient answer, and
    /// [`Self::keeps_the_old_instance`] is the one measured against a live
    /// shepherd.
    struct Shepherd {
        /// What the flock settles to after a reload: `Online` is a release
        /// that came up, `Starting` one that spawned and never became
        /// ready.
        settles_to: ProcStatus,
        /// Whether a reload actually puts a new process in place. `false`
        /// is shep's own behaviour when `AwaitReady` fails: it keeps the
        /// old instance serving, so the listing settles back to one
        /// `Online` entry under the pid that was already there.
        replaces: bool,
        /// From which reload count `describe` starts failing outright, or
        /// `None` for never.
        ///
        /// `Some(1)` is a shepherd that answers the generation capture and
        /// then stops: an error arriving after the reload has already
        /// drained the old instance, which is a different path from a sheep
        /// that will not come up. `Some(0)` fails the capture itself, which
        /// is the step that used to sit outside the rollback boundary.
        describe_fails_from: Option<u32>,
        /// Created just before answering the first `describe`, standing in
        /// for another process leaving a stale `current.tmp` behind at the
        /// worst possible moment - after the swap, before the rollback.
        plant: Option<PathBuf>,
        /// How many reloads after the first to refuse, standing in for the
        /// shepherd's own refusal to start a reload while one is in flight.
        /// The first is never refused, because that is the deploy's own and
        /// the case being modelled is a ROLLBACK arriving while it runs.
        refusals: Cell<u32>,
        /// The code those refusals carry. `Internal` is what a real
        /// `ReloadInFlight` arrives as; `NotFound` is the one that can
        /// never clear.
        refusal_code: RpcErrorCode,
        /// Whether the deploy's OWN reload is refused too, rather than only
        /// the rollback's.
        refuse_from_the_first: Cell<bool>,
        /// How many reloads LAND and then answer a transport error - the
        /// reply lost rather than the request refused. The swap goes ahead
        /// each time, so the flock turns over exactly as it would have; all
        /// the caller learns is that no answer came back. `u32::MAX` never
        /// stops losing them.
        replies_lost: Cell<u32>,
        /// Every `reload` call, refused ones included - which is how a test
        /// can see whether something was retried.
        attempts: Cell<u32>,
        /// Whether the pid moves on every `describe`, standing in for a
        /// release that comes up and then keeps dying and being restarted.
        flapping: bool,
        /// How many instances the flock is running. The whole point of the
        /// number: a reload costs one swap per RUNNING instance, whatever
        /// the Flockfile happens to ask for.
        instances: u32,
        /// How many `describe` calls the replacement takes to appear,
        /// standing in for a reload that is genuinely still in flight. `0`
        /// is the instantaneous swap every other test wants.
        turnover_after: u32,
        /// Counted only so [`Self::flapping`] has something to move with.
        describes: Cell<u32>,
        reloads: Cell<u32>,
    }

    /// The pid serving before any reload. Any other value would do; this
    /// one is only recognisable in a failure message.
    const FIRST_PID: u32 = 12835;

    impl Shepherd {
        /// A reload that replaces the process and brings it to `Online`.
        fn ready() -> Self {
            Self {
                settles_to: ProcStatus::Online,
                replaces: true,
                describe_fails_from: None,
                plant: None,
                refusals: Cell::new(0),
                refusal_code: RpcErrorCode::Internal,
                refuse_from_the_first: Cell::new(false),
                replies_lost: Cell::new(0),
                attempts: Cell::new(0),
                flapping: false,
                instances: 1,
                turnover_after: 0,
                describes: Cell::new(0),
                reloads: Cell::new(0),
            }
        }

        /// A reload that replaces the process with one that never becomes
        /// ready.
        fn never_ready() -> Self {
            Self {
                settles_to: ProcStatus::Starting,
                ..Self::ready()
            }
        }

        /// A reload shep accepted and then gave up on, keeping the old
        /// instance serving: one `Online` entry, under the pid that was
        /// already there. Measured against a real shepherd - see
        /// `crate::verify`'s module doc for the transcript.
        fn keeps_the_old_instance() -> Self {
            Self {
                replaces: false,
                ..Self::ready()
            }
        }

        /// Answers the generation capture and then fails every `describe`
        /// after the reload - the transient error `verify::wait`'s own doc
        /// anticipates.
        fn describe_fails() -> Self {
            Self {
                describe_fails_from: Some(1),
                ..Self::ready()
            }
        }

        /// Fails every `describe`, starting with the generation capture
        /// itself. A real shepherd does neither on command, which is why
        /// the only place this can be exercised is here.
        fn unreachable() -> Self {
            Self {
                describe_fails_from: Some(0),
                ..Self::ready()
            }
        }

        /// A shepherd too busy to accept even the deploy's own reload -
        /// an operator's `shep reload web` landing first, which shep
        /// refuses for as long as it is in flight.
        fn too_busy_to_start() -> Self {
            let shepherd = Self::ready();
            shepherd.refusals.set(u32::MAX);
            shepherd.refuse_from_the_first.set(true);
            shepherd
        }

        /// As [`Self::never_ready`], but leaves a stale `current.tmp` at
        /// `plant` on the way past.
        fn planting(plant: PathBuf) -> Self {
            Self {
                plant: Some(plant),
                ..Self::never_ready()
            }
        }

        /// A shepherd that is busy with the deploy's own reload when the
        /// rollback's arrives, and refuses the next `times` of them.
        /// `u32::MAX` never stops refusing.
        fn busy_for(times: u32) -> Self {
            let shepherd = Self::never_ready();
            shepherd.refusals.set(times);
            shepherd
        }

        /// A release that comes up and then will not stay up: a fresh pid
        /// on every look, which is what a crash loop looks like from
        /// `describe`.
        fn flapping() -> Self {
            Self {
                flapping: true,
                ..Self::ready()
            }
        }

        /// A reload that succeeds, but only once `describes` more looks
        /// have gone by - a swap that is genuinely still in flight rather
        /// than one that failed. At `verify`'s 100ms poll, `describes` is
        /// tenths of a second of budget.
        fn ready_after(describes: u32) -> Self {
            Self {
                turnover_after: describes,
                ..Self::ready()
            }
        }

        /// The same shepherd, running `instances` instances.
        fn running(self, instances: u32) -> Self {
            Self { instances, ..self }
        }

        /// A shepherd whose next `count` reloads are accepted and acted on
        /// with their replies never coming back: `Timeout` after the swap
        /// has already begun.
        ///
        /// The window is real rather than theoretical - `DEFAULT_DEADLINE`
        /// is five seconds and `DEADLINE_GRACE` two, and a reload of
        /// several instances outlives both by design, which is why shep
        /// answers it as an acceptance in the first place.
        fn losing_replies(count: u32) -> Self {
            let shepherd = Self::ready();
            shepherd.replies_lost.set(count);
            shepherd
        }

        /// A shepherd that has never heard of this sheep: every reload
        /// after the first is `NotFound`, which no amount of asking again
        /// can change.
        fn unregistered() -> Self {
            Self {
                refusal_code: RpcErrorCode::NotFound,
                ..Self::busy_for(u32::MAX)
            }
        }

        fn attempt_count(&self) -> u32 {
            self.attempts.get()
        }

        fn reload_count(&self) -> u32 {
            self.reloads.get()
        }

        /// How many reloads have actually landed. A reload that has been
        /// accepted but whose replacement has not appeared yet counts for
        /// nothing, which is what an in-flight swap looks like from
        /// `describe`: the old generation, still serving.
        fn landed(&self) -> u32 {
            if self.describes.get() > self.turnover_after {
                self.reloads.get()
            } else {
                0
            }
        }

        /// The pid of instance `index`: a fresh generation per landed
        /// reload, unless this shepherd keeps the old instance.
        fn pid(&self, index: u32) -> u32 {
            if !self.replaces {
                return FIRST_PID + index;
            }
            let churn = if self.flapping {
                self.describes.get()
            } else {
                0
            };
            FIRST_PID + (self.landed() + churn) * 100 + index
        }

        /// The status currently serving: whatever this shepherd settles to
        /// once a reload has landed, and `Online` before that, since the
        /// process that was already there is up.
        fn status(&self) -> ProcStatus {
            if self.landed() == 0 {
                ProcStatus::Online
            } else {
                self.settles_to
            }
        }
    }

    impl Daemon for Shepherd {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            unimplemented!()
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            unimplemented!()
        }
        async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            // `exists()` follows the link, and this one dangles on purpose,
            // so ask about the link itself or every poll plants it again.
            if let Some(plant) = &self.plant
                && plant.symlink_metadata().is_err()
            {
                std::os::unix::fs::symlink("somewhere", plant).expect("plant a stale tmp link");
            }
            self.describes.set(self.describes.get() + 1);
            if self
                .describe_fails_from
                .is_some_and(|from| self.reloads.get() >= from)
            {
                return Err(Error::Protocol("the shepherd stopped answering".to_owned()));
            }
            Ok((0..self.instances)
                .map(|index| {
                    ProcessInfo::builder(index, sheep, self.status())
                        .pid(Some(self.pid(index)))
                        .build()
                })
                .collect())
        }
        async fn start(&self, _apps: Vec<AppConfig>) -> Result<(), Error> {
            unimplemented!()
        }
        async fn delete(&self, _id: u32) -> Result<(), Error> {
            unimplemented!()
        }
        async fn reload(&self, sheep: &str) -> Result<(), Error> {
            self.attempts.set(self.attempts.get() + 1);
            let refusals = self.refusals.get();
            if (self.reloads.get() > 0 || self.refuse_from_the_first.get()) && refusals > 0 {
                if refusals != u32::MAX {
                    self.refusals.set(refusals - 1);
                }
                // The shape shep really answers with: `ReloadInFlight` has
                // no code of its own, so it arrives as `Internal`. A fake
                // that answered `Protocol` here would let a retry policy
                // that cannot read codes look correct.
                return Err(Error::Request(RequestError::Rpc(RpcError {
                    code: self.refusal_code,
                    message: format!("{sheep} is already being reloaded"),
                })));
            }
            self.reloads.set(self.reloads.get() + 1);

            let lost = self.replies_lost.get();
            if lost > 0 {
                if lost != u32::MAX {
                    self.replies_lost.set(lost - 1);
                }
                return Err(Error::Request(RequestError::Timeout {
                    after: Duration::from_secs(7),
                }));
            }
            Ok(())
        }
        async fn restart(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
        async fn save_roll(&self) -> Result<PathBuf, Error> {
            unimplemented!()
        }
    }

    /// fails if a deploy that never comes up leaves the new release live.
    /// This is the whole promise of the feature: verification without
    /// rollback is just a slower way to be broken.
    #[tokio::test(start_paused = true)]
    async fn a_release_that_never_comes_up_is_rolled_back() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(
            &Shepherd::never_ready(),
            &fixture.tree,
            &mut fixture.state,
            5,
        )
        .await
        .expect("completes");

        assert!(matches!(outcome, Outcome::RolledBack { .. }));
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if an unchanged remote head still does the work. Polling must
    /// be cheap; a deploy per tick would rebuild constantly.
    #[tokio::test]
    async fn an_unchanged_head_does_nothing() {
        let mut fixture = fixture_with_previous_release();
        let outcome = deploy(&Shepherd::ready(), &fixture.tree, &mut fixture.state, 5)
            .await
            .expect("completes");
        assert!(matches!(outcome, Outcome::UpToDate));
    }

    /// fails if a release that comes up healthy is not recorded as
    /// deployed. `state.deployed` is what the next poll compares against,
    /// so a deploy that succeeds without advancing it would redeploy the
    /// same sha on every tick forever.
    #[tokio::test]
    async fn a_release_that_comes_up_is_deployed_and_recorded() {
        let mut fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::ready();

        let outcome = deploy(&daemon, &fixture.tree, &mut fixture.state, 5)
            .await
            .expect("completes");

        // Swapping the symlink is not deploying. Without this the whole
        // reload can be deleted and the test still passes.
        assert_eq!(daemon.reload_count(), 1, "the sheep must be reloaded once");
        assert_eq!(
            outcome,
            Outcome::Deployed {
                sha: second.clone()
            }
        );
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&second))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(second.as_str()));

        let written = State::read(&fixture.tree.state_file()).expect("deploy.toml was written");
        assert_eq!(written.deployed.as_deref(), Some(second.as_str()));
    }

    /// fails if the process that was already serving passes for the
    /// release this deploy installed. Measured against a real shepherd:
    /// when its own `AwaitReady` fails, shep keeps the old instance, so
    /// `describe` settles back to one `Online` entry under the pid that was
    /// already there. A verification that reads status alone calls that
    /// deploy healthy, and every rollback in this crate becomes
    /// unreachable - which is what it was, for two review rounds.
    #[tokio::test(start_paused = true)]
    async fn a_reload_that_keeps_the_old_instance_is_rolled_back() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(
            &Shepherd::keeps_the_old_instance(),
            &fixture.tree,
            &mut fixture.state,
            5,
        )
        .await
        .expect("completes");

        assert!(matches!(outcome, Outcome::RolledBack { .. }), "{outcome:?}");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if this crate's reload window stops matching the shepherd's
    /// own reload deadline. That deadline is
    /// `listen_timeout + graceful_timeout + RELOAD_DEADLINE_SLACK` per
    /// instance (`Actor::arm_reload_deadline`, `supervisor.rs:3581`, with
    /// the five-second slack at `supervisor.rs:103`), and instances are
    /// swapped strictly one at a time.
    ///
    /// Be clear about what this test can and cannot do. It restates the
    /// formula, so it catches a change to OURS. It cannot see shep's, so it
    /// cannot catch a change to THEIRS - the numbers below are copied by
    /// hand from a file this crate does not depend on. What covers that
    /// direction is `a_reload_that_uses_its_whole_drain_window_still_deploys`
    /// in the integration tier, which puts a real shepherd through a reload
    /// that spends its drain window and fails if the budget no longer holds
    /// it.
    ///
    /// The arithmetic is spelled out rather than folded, because a test that
    /// recomputes the implementation's expression only pins that it was
    /// typed twice.
    #[test]
    fn the_window_matches_the_shepherds_own_reload_deadline() {
        let mut app: AppConfig =
            toml::from_str("name = 'web'\nscript = './run.sh'").expect("parses");

        // shep's shipping defaults: listen 3s, graceful 8s. 3 + 8 + 5 = 16
        // per instance.
        assert_eq!(app.listen_timeout.as_duration(), Duration::from_secs(3));
        assert_eq!(app.graceful_timeout.as_duration(), Duration::from_secs(8));
        assert_eq!(budget(&app, 1), Duration::from_secs(16));

        // The term round four's derivation dropped. Doubling
        // `listen_timeout` supplied 3s of headroom against an 8s drain, and
        // `verify` needs the whole drain to fit because it waits for every
        // old instance to be gone.
        assert_eq!(budget(&app, 2), Duration::from_secs(32));

        // The instance count is the caller's, measured off the running
        // flock. `AppConfig::instances` is what the file asks for, and
        // `shep stock` moves the two apart - see `budget`'s own doc.
        app.instances = 1;
        assert_eq!(budget(&app, 4), Duration::from_secs(64));

        // And it follows both timeouts, not just the one.
        app.graceful_timeout = UpDuration::from_millis(20_000);
        app.listen_timeout = UpDuration::from_millis(1_000);
        assert_eq!(budget(&app, 3), Duration::from_secs(78));

        // A flock with nothing running gets one instance's worth, so a
        // reload that can replace nothing fails at a sensible moment
        // instead of instantly.
        assert_eq!(budget(&app, 0), budget(&app, 1));
    }

    /// fails if the reload window is sized from the Flockfile's instance
    /// count rather than the flock's. `AppConfig::instances` is what the
    /// file ASKS for; `shep stock <sheep> <n>` changes what is running
    /// without touching any file this crate reads, and a reload costs one
    /// swap per running instance.
    ///
    /// The numbers here are the reviewer's reproduction, scaled to the
    /// fixture: one instance in the file, two actually running, and a
    /// replacement that takes about twenty seconds to appear. Sized off the
    /// file that is a sixteen-second budget and a healthy release is rolled
    /// back; sized off the flock it is thirty-two and the deploy stands.
    #[tokio::test(start_paused = true)]
    async fn the_window_follows_the_running_flock_not_the_flockfile() {
        let mut fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");
        // 200 polls at 100ms apart, so the turnover lands near 20s: inside
        // two instances' budget and outside one's.
        let daemon = Shepherd::ready_after(200).running(2);

        let outcome = deploy(&daemon, &fixture.tree, &mut fixture.state, 5)
            .await
            .expect("completes");

        assert_eq!(outcome, Outcome::Deployed { sha: second });
    }

    /// fails if a request that can never succeed is retried anyway. The
    /// retry exists for `ReloadInFlight`, which shep can only report as
    /// `Internal`; a `NotFound` means the selector matched nothing, and
    /// asking again cannot make a sheep exist. Retrying it burned the whole
    /// budget and then reported a split state claiming a running process
    /// there was none of.
    #[test]
    fn only_a_failure_that_could_clear_is_retried() {
        let rpc = |code| {
            Error::Request(RequestError::Rpc(RpcError {
                code,
                message: "web is already being reloaded".to_owned(),
            }))
        };

        assert!(is_retryable(&rpc(RpcErrorCode::Internal)));
        assert!(is_retryable(&rpc(RpcErrorCode::DeadlineExceeded)));
        assert!(is_retryable(&Error::Request(RequestError::Timeout {
            after: Duration::from_secs(1)
        })));

        assert!(!is_retryable(&rpc(RpcErrorCode::NotFound)));
        assert!(!is_retryable(&rpc(RpcErrorCode::InvalidConfig)));
        assert!(!is_retryable(&Error::Request(RequestError::Closed)));
        assert!(!is_retryable(&Error::Protocol("nonsense".to_owned())));
    }

    /// fails if a rollback gives up the first time the shepherd says it is
    /// busy. That refusal is transient and self-clearing - it means the
    /// reload verification just gave up on is still running - so retrying
    /// is the difference between a rollback that lands a few seconds late
    /// and one that cannot happen at all.
    #[tokio::test(start_paused = true)]
    async fn a_rollback_retries_a_reload_the_shepherd_is_too_busy_for() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");

        let daemon = Shepherd::busy_for(3);
        let outcome = deploy(&daemon, &fixture.tree, &mut fixture.state, 5)
            .await
            .expect("completes");

        assert!(matches!(outcome, Outcome::RolledBack { .. }), "{outcome:?}");
        // The deploy's own reload, three refusals, then the one that took.
        assert_eq!(daemon.attempt_count(), 5);
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if a rollback the shepherd never accepts is reported as
    /// anything vaguer than the state it leaves. This is the one situation
    /// this crate cannot repair on its own, and the operator gets three
    /// things to compare and one command to run: `current` and deploy.toml
    /// agree on the old release, and the process may still be on the new
    /// one.
    #[tokio::test(start_paused = true)]
    async fn a_rollback_the_shepherd_never_accepts_is_reported_as_a_split_state() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        let second = commit_on_origin(&fixture, "second.txt");

        let err = deploy(
            &Shepherd::busy_for(u32::MAX),
            &fixture.tree,
            &mut fixture.state,
            5,
        )
        .await
        .expect_err("the rollback cannot reload");

        // Not wrapped in `RollbackFailed`: that wrapper's tail tells an
        // operator to go and compare the three things `Split` has just
        // finished telling them.
        assert!(matches!(err, Error::Split { .. }), "{err:?}");
        let shown = err.to_string();
        assert!(!shown.contains("rolling back after"), "{shown}");
        assert!(
            shown.contains("no reload onto it could be confirmed"),
            "{shown}"
        );
        assert!(shown.contains(&previous), "{shown}");
        assert!(shown.contains(&second), "{shown}");
        assert!(shown.contains("shep reload web"), "{shown}");

        // The two things this crate does control agree with each other, so
        // the message has exactly one disagreement to explain.
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
        assert_eq!(
            State::read(&fixture.tree.state_file())
                .expect("deploy.toml was written")
                .deployed
                .as_deref(),
            Some(previous.as_str())
        );
    }

    /// fails if the reason a rollback happened quotes the budget instead of
    /// what elapsed. An `Alive` verdict takes its turnover wait PLUS the
    /// dwell, so a release that comes up and then will not stay up is
    /// rejected at a moment the budget never names - here ten seconds of
    /// dwell against a sixteen-second budget. An operator reading the
    /// budget back is being told how long the deploy was allowed to take,
    /// not how long it took.
    #[tokio::test(start_paused = true)]
    async fn the_reason_quotes_what_elapsed_not_what_was_allowed() {
        let mut fixture = fixture_with_previous_release();
        fixture.state.verify = crate::state::Verify::Alive;
        commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(&Shepherd::flapping(), &fixture.tree, &mut fixture.state, 5)
            .await
            .expect("completes");

        let Outcome::RolledBack { why, .. } = outcome else {
            panic!("a release that will not stay up must roll back: {outcome:?}");
        };
        // The dwell alone: the turnover was immediate, and the budget for
        // this app is 3 + 8 + 5 = 16s.
        assert!(why.contains("10s"), "{why}");
        assert!(!why.contains("16s"), "{why}");
    }

    /// fails if a reload the shepherd can never accept is retried anyway,
    /// or dressed up as a split state. A `NotFound` means no sheep of that
    /// name is registered: asking again cannot change it, so the retry
    /// burned the whole budget, and the `Split` it then reported claimed a
    /// running process there was none of and prescribed a `shep reload`
    /// that fails identically.
    #[tokio::test(start_paused = true)]
    async fn a_reload_that_can_never_succeed_is_not_retried_or_called_a_split() {
        let mut fixture = fixture_with_previous_release();
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::unregistered();

        let err = deploy(&daemon, &fixture.tree, &mut fixture.state, 5)
            .await
            .expect_err("the rollback cannot reload");

        assert!(!matches!(err, Error::Split { .. }), "{err:?}");
        assert!(matches!(err, Error::RollbackFailed { .. }), "{err:?}");
        assert!(err.to_string().contains("NotFound"), "{err}");
        // The deploy's own reload and exactly one rollback attempt.
        assert_eq!(daemon.attempt_count(), 2);
    }

    /// fails if a `probed` target with no readiness probe is deployed
    /// anyway. `Online` is only a health verdict when a probe is what gates
    /// it; without one shep waits out `listen_timeout` and reports a
    /// process `Online` for not having died, so this configuration
    /// verifies every release including a broken one. The spec requires the
    /// refusal by name.
    #[tokio::test]
    async fn a_probed_target_with_no_readiness_probe_is_refused() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        fs::write(
            fixture.origin.path().join("Flockfile.toml"),
            "[[app]]\nname = 'web'\nscript = './run.sh'\n",
        )
        .expect("write a probeless Flockfile");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::ready();

        let err = deploy(&daemon, &fixture.tree, &mut fixture.state, 5)
            .await
            .expect_err("no probe to wait on");

        let shown = err.to_string();
        assert!(shown.contains("readiness_probe"), "{shown}");
        assert!(shown.contains("alive"), "{shown}");
        // Refused before anything was touched, which is why it is worth
        // refusing at all rather than deploying and hoping.
        assert_eq!(daemon.reload_count(), 0, "nothing may be reloaded");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
    }

    /// fails if the refusal catches a sheep that waits on its channel.
    /// `wait_ready` is shep's other readiness gate and the stronger of the
    /// two: it reports `Online` only once the app says so itself, which is
    /// exactly as meaningful as a probe. Refusing it would name three ways
    /// out of a problem the operator had already solved.
    #[tokio::test(start_paused = true)]
    async fn a_probed_target_that_waits_on_its_channel_is_not_refused() {
        let mut fixture = fixture_with_previous_release();
        fs::write(
            fixture.origin.path().join("Flockfile.toml"),
            "[[app]]\nname = 'web'\nscript = './run.sh'\nwait_ready = true\n",
        )
        .expect("write a channel-gated Flockfile");
        let second = commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(&Shepherd::ready(), &fixture.tree, &mut fixture.state, 5)
            .await
            .expect("completes");

        assert_eq!(outcome, Outcome::Deployed { sha: second });
    }

    /// fails if the refusal above catches `alive` too. `alive` is the
    /// deliberate downgrade the refusal's own message points at, so a
    /// target that has taken it must deploy without a probe - otherwise
    /// the message names a way out that does not exist.
    #[tokio::test(start_paused = true)]
    async fn an_alive_target_needs_no_readiness_probe() {
        let mut fixture = fixture_with_previous_release();
        fixture.state.verify = crate::state::Verify::Alive;
        fs::write(
            fixture.origin.path().join("Flockfile.toml"),
            "[[app]]\nname = 'web'\nscript = './run.sh'\n",
        )
        .expect("write a probeless Flockfile");
        let second = commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(&Shepherd::ready(), &fixture.tree, &mut fixture.state, 5)
            .await
            .expect("completes");

        assert_eq!(outcome, Outcome::Deployed { sha: second });
    }

    /// fails if a failing build reaches the swap. Steps one through five
    /// must never touch the running app: the build happens in a directory
    /// the live release does not share, so a build that exits non-zero has
    /// to leave `current` and `state.deployed` exactly where they were.
    #[tokio::test]
    async fn a_failing_build_never_moves_current() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        fs::write(
            fixture.origin.path().join("Flockfile.toml"),
            format!("{FLOCKFILE}\n[build]\ncommand = 'exit 3'\n"),
        )
        .expect("write Flockfile");
        commit_on_origin(&fixture, "second.txt");

        let err = deploy(&Shepherd::ready(), &fixture.tree, &mut fixture.state, 5)
            .await
            .expect_err("the build fails");

        assert!(matches!(err, Error::Build { status: Some(3) }), "{err:?}");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if a branch that is not on the remote comes back as git's own
    /// `fatal: Needed a single revision`, which names the branch without
    /// ever saying the branch is not there. A branch deleted upstream is
    /// the one failure a watched target hits unattended, so it is the one
    /// that most needs to explain itself.
    #[tokio::test]
    async fn a_branch_that_is_not_on_the_remote_says_so() {
        let mut fixture = fixture_with_previous_release();
        fixture.state.branch = "gone".to_owned();

        let err = deploy(&Shepherd::ready(), &fixture.tree, &mut fixture.state, 5)
            .await
            .expect_err("no such branch");

        let shown = err.to_string();
        assert!(shown.contains("gone"), "{shown}");
        assert!(shown.contains("no branch named"), "{shown}");
        // Every word of it, not just the words before the first line break
        // in the source. A `\\` where a line continuation was meant puts a
        // literal backslash and seventeen spaces in the middle of the one
        // message a watched target hits unattended, and assertions that
        // stop early never see it.
        assert!(shown.contains("nothing to deploy"), "{shown}");
        assert!(!shown.contains('\\'), "{shown}");
    }

    /// fails if a failure between the swap and the reload escapes the
    /// rollback boundary. The generation capture is a real request and can
    /// fail like any other; when it did, `deploy` returned the bare error
    /// with `current` still naming the new release, `deploy.toml` naming
    /// the old one, and no reload ever sent. Any later restart of that
    /// sheep - a crash restart included - would have brought it up on a
    /// release nothing verified. It was the only fallible call past the
    /// swap that sat outside the arm that owns rollback.
    #[tokio::test(start_paused = true)]
    async fn a_failure_before_the_reload_puts_current_back() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::unreachable();

        let err = deploy(&daemon, &fixture.tree, &mut fixture.state, 5)
            .await
            .expect_err("the shepherd cannot be asked anything");

        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
        assert_eq!(daemon.attempt_count(), 0, "no reload may be sent");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous)),
            "current must not be left on a release nothing verified"
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if a reload whose REPLY was lost is treated as one that never
    /// happened. `Timeout` is "no reply within the deadline plus
    /// `DEADLINE_GRACE`" - five seconds and two - and shep answers a reload
    /// as an acceptance precisely because a real one outlives that. So the
    /// shepherd can have accepted it and begun spawning from the new
    /// `current` before the client gave up waiting.
    ///
    /// Sent down the "nothing was started" path, that becomes a genuinely
    /// split machine reported as a bare transport error: `current` and
    /// `deploy.toml` on the old release, the flock running the new one, and
    /// no reload ever sent onto the old release.
    #[tokio::test(start_paused = true)]
    async fn a_reload_whose_reply_is_lost_is_rolled_back() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        // The deploy's own reply is lost; the rollback's gets through.
        let daemon = Shepherd::losing_replies(1);

        let err = deploy(&daemon, &fixture.tree, &mut fixture.state, 5)
            .await
            .expect_err("no reply came back");

        // Rolled back, not shrugged off: the deploy failed, and the error
        // says what is live again with the timeout still underneath it.
        assert!(matches!(err, Error::RolledBack { .. }), "{err:?}");
        assert!(
            matches!(
                err.source()
                    .and_then(<dyn core::error::Error>::downcast_ref),
                Some(Error::Request(RequestError::Timeout { .. }))
            ),
            "{err:?}"
        );
        // The deploy's own reload, and the rollback's onto the old release.
        // The second is the one the "nothing started" path never sent.
        assert_eq!(daemon.attempt_count(), 2);
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if a rollback whose own reload is never confirmed stops
    /// reporting the split state. This is the arm that had no test at any
    /// tier until the fake learned to lose a reply: `restore` swaps back,
    /// corrects the record, and then cannot get an answer about the reload
    /// for the whole budget, so which release the instances are on is
    /// genuinely unknown.
    #[tokio::test(start_paused = true)]
    async fn a_rollback_that_is_never_confirmed_reports_the_split_state() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");

        let err = deploy(
            &Shepherd::losing_replies(u32::MAX),
            &fixture.tree,
            &mut fixture.state,
            5,
        )
        .await
        .expect_err("no reload is ever confirmed");

        assert!(matches!(err, Error::Split { .. }), "{err:?}");
        let shown = err.to_string();
        assert!(shown.contains("could be confirmed"), "{shown}");
        assert!(shown.contains(&previous), "{shown}");
        // The two things this crate controls still agree with each other.
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if the "did it start" question stops being read off the error
    /// shape. Only a failure that is known to have happened before the
    /// shepherd could act counts, because being wrong that way leaves a
    /// split machine while being wrong the other way costs one needless
    /// reload of a healthy sheep.
    #[test]
    fn only_a_definite_refusal_counts_as_never_started() {
        let rpc = Error::Request(RequestError::Rpc(RpcError {
            code: RpcErrorCode::Internal,
            message: "web is already being reloaded".to_owned(),
        }));
        assert!(never_reached_the_shepherd(&rpc));

        // Ambiguous: the shepherd may have accepted and acted.
        assert!(!never_reached_the_shepherd(&Error::Request(
            RequestError::Timeout {
                after: Duration::from_secs(7)
            }
        )));
        assert!(!never_reached_the_shepherd(&Error::Request(
            RequestError::Closed
        )));
        // It answered something, so it processed the request.
        assert!(!never_reached_the_shepherd(&Error::Protocol(
            "a Flock in answer to Reload".to_owned()
        )));
    }

    /// fails if a refusal of the deploy's OWN reload is treated as though
    /// the release had been started. An operator running `shep reload web`
    /// by hand during a poll-loop deploy produces exactly this: the
    /// shepherd refuses a second reload while one is in flight, so nothing
    /// was ever spawned on the new release.
    ///
    /// What the post-reload rollback did with it was swap back, correct the
    /// record, retry the reload for the whole budget, get refused
    /// throughout, and report `Error::Split` - the variant reserved for a
    /// state this crate cannot repair - about a machine that was entirely
    /// consistent, telling the operator to finish a rollback that had
    /// already finished.
    #[tokio::test(start_paused = true)]
    async fn a_refused_reload_is_not_a_split_state() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::too_busy_to_start();

        let err = deploy(&daemon, &fixture.tree, &mut fixture.state, 5)
            .await
            .expect_err("the shepherd would not take the reload");

        assert!(!matches!(err, Error::Split { .. }), "{err:?}");
        assert!(err.to_string().contains("already being reloaded"), "{err}");
        // One attempt, not a budget's worth of retries: there is nothing to
        // retry towards when nothing was started.
        assert_eq!(daemon.attempt_count(), 1);
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if a rollback moves the symlink and never reloads. Moving a
    /// symlink does not move a running process: without the second reload
    /// the daemon keeps running the release that was just rejected, while
    /// `current` and `deploy.toml` both name the old one, and an operator
    /// is told the rollback succeeded.
    #[tokio::test(start_paused = true)]
    async fn a_rollback_reloads_onto_the_previous_release() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::never_ready();

        let outcome = deploy(&daemon, &fixture.tree, &mut fixture.state, 5)
            .await
            .expect("completes");

        assert!(matches!(outcome, Outcome::RolledBack { .. }));
        assert_eq!(
            daemon.reload_count(),
            2,
            "one reload onto the new release, one onto the old"
        );
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
    }

    /// fails if an error arriving after the reload gets a lesser rollback
    /// than a failed verification does. `land` reloads before it verifies,
    /// so a `describe` that fails transiently - which `verify::wait`'s own
    /// doc anticipates - arrives with shep already running the new release
    /// and the old instance already drained. A swap without a reload there
    /// leaves the daemon on the new code while both signals an operator
    /// would check name the old one.
    #[tokio::test]
    async fn a_verify_error_after_the_reload_still_reloads_the_rollback() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::describe_fails();

        let err = deploy(&daemon, &fixture.tree, &mut fixture.state, 5)
            .await
            .expect_err("describe fails");

        // The deploy still failed, so this is still an error - but it
        // names the rollback, and the shepherd's own complaint is still
        // reachable underneath it.
        assert!(matches!(err, Error::RolledBack { .. }), "{err:?}");
        assert!(
            err.to_string().contains("the shepherd stopped answering"),
            "{err}"
        );
        assert!(
            matches!(
                err.source().and_then(|s| s.downcast_ref()),
                Some(Error::Protocol(_))
            ),
            "{err:?}"
        );
        assert_eq!(
            daemon.reload_count(),
            2,
            "one reload onto the new release, one onto the old"
        );
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if a rollback decides "there is nothing to go back to" from
    /// `current` alone. A deploy killed between its swap and its
    /// `State::write` is exactly the interruption `restore`'s record
    /// correction exists to repair, and it leaves `current` already naming
    /// the release being attempted while `deploy.toml` still names a good
    /// one whose worktree is on disk.
    ///
    /// Read `current` alone and the retry concludes `previous == attempted`,
    /// reports "there is nothing to roll back to", attempts no rollback
    /// reload, and leaves a release that will not come up serving.
    #[tokio::test(start_paused = true)]
    async fn a_rollback_reads_deploy_toml_when_current_is_already_the_new_release() {
        let mut fixture = fixture_with_previous_release();
        let live = fixture.state.deployed.clone().expect("a previous release");
        let second = commit_on_origin(&fixture, "second.txt");

        // What an interrupted deploy leaves behind: the swap happened, the
        // record write did not.
        crate::git::fetch(&fixture.tree.git(), &fixture.state.remote).expect("fetch");
        install_release(&fixture, &second);
        assert_eq!(fixture.state.deployed.as_deref(), Some(live.as_str()));

        let daemon = Shepherd::never_ready();
        let outcome = deploy(&daemon, &fixture.tree, &mut fixture.state, 5)
            .await
            .expect("completes");

        assert_eq!(
            outcome,
            Outcome::RolledBack {
                to: live.clone(),
                why: "it did not come up and stay up, 16s after the reload".to_owned(),
            }
        );
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&live))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(live.as_str()));
        assert_eq!(
            State::read(&fixture.tree.state_file())
                .expect("deploy.toml")
                .deployed
                .as_deref(),
            Some(live.as_str())
        );
        // The deploy's own reload, and the rollback's onto the recorded
        // release. The second is the one the old code never sent.
        assert_eq!(daemon.attempt_count(), 2);
    }

    /// fails if the release `current` names is trusted to exist. The
    /// existence guard was added for the record's branch and not for this
    /// one, which left the first source able to do the exact harm the guard
    /// was added for: `current` naming a release that retention has swept
    /// is a dangling link, and rolling back "onto" it repoints `current` at
    /// a path that is not there.
    ///
    /// The sheep's `cwd` is `current`, permanently, so that is not a
    /// cosmetic failure - it is an app that cannot start at all, which is
    /// worse than the release that would not come up.
    #[tokio::test(start_paused = true)]
    async fn a_current_naming_a_swept_release_is_not_a_rollback_target() {
        let mut fixture = fixture_with_previous_release();
        let live = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");

        // Retention swept the release `current` still points at, so the
        // link is there and its target is not.
        crate::git::worktree_remove(&fixture.tree.git(), &fixture.tree.release(&live))
            .expect("retention removes the worktree");
        assert!(swap::resolve(&fixture.tree.current()).unwrap().is_some());
        assert!(!fixture.tree.release(&live).exists());

        let err = deploy(
            &Shepherd::never_ready(),
            &fixture.tree,
            &mut fixture.state,
            5,
        )
        .await
        .expect_err("nothing usable to roll back to");

        assert!(matches!(err, Error::Unverified { .. }), "{err:?}");
        assert!(
            swap::resolve(&fixture.tree.current())
                .unwrap()
                .is_some_and(|path| path.exists()),
            "current must never be repointed at a release that is not there"
        );
    }

    /// fails if a sha in `deploy.toml` is treated as a release that is
    /// there. Retention removes old worktrees, so the record can name one
    /// that no longer exists on disk, and rolling back onto it would point
    /// `current` at a dangling link - a sheep whose `cwd` resolves to
    /// nothing, which is worse than the release that would not come up.
    #[tokio::test(start_paused = true)]
    async fn a_recorded_release_that_is_gone_is_not_a_rollback_target() {
        let mut fixture = fixture_with_previous_release();
        let live = fixture.state.deployed.clone().expect("a previous release");
        let second = commit_on_origin(&fixture, "second.txt");

        // The same interrupted deploy as above, except that retention has
        // since swept the release the record names.
        crate::git::fetch(&fixture.tree.git(), &fixture.state.remote).expect("fetch");
        install_release(&fixture, &second);
        crate::git::worktree_remove(&fixture.tree.git(), &fixture.tree.release(&live))
            .expect("retention removes the old worktree");
        assert!(!fixture.tree.release(&live).exists());

        let err = deploy(
            &Shepherd::never_ready(),
            &fixture.tree,
            &mut fixture.state,
            5,
        )
        .await
        .expect_err("nothing left to roll back to");

        assert!(matches!(err, Error::Unverified { .. }), "{err:?}");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&second)),
            "current stays where it was rather than dangling"
        );
    }

    /// fails if a tree the cutover never landed on can be deployed. This is
    /// the deploy that does not merely go wrong, it goes wrong
    /// convincingly. `deployed` is `None` on an existing tree only when
    /// `cut_over` never ran, so the sheep is still registered at the
    /// operator's OWN checkout: a deploy builds, swaps `current`, and
    /// reloads the sheep BY NAME, which really does replace that instance,
    /// so verification sees a full pid turnover, the run prints
    /// `deployed <sha>` and exits 0, and the sha is written into the
    /// record. Measured against a real shepherd on a release whose script
    /// is `exit 1`.
    ///
    /// Three messages already tell an operator not to do this and none of
    /// them refused it. Prose is also the wrong instrument here: an
    /// abandoned tree is now left `manual`, so the poll loop passes over
    /// it, but `shep deploy <sheep>` still reaches this by hand.
    #[tokio::test(start_paused = true)]
    async fn a_tree_the_cutover_never_landed_on_is_refused_before_anything_happens() {
        let mut fixture = fixture_before_any_release();

        let err = deploy(
            &Shepherd::never_ready(),
            &fixture.tree,
            &mut fixture.state,
            5,
        )
        .await
        .expect_err("refuses");

        assert!(matches!(err, Error::Config(_)), "{err:?}");
        let shown = err.to_string();
        assert!(shown.contains("never cut over"), "{shown}");
        assert!(shown.contains("shep-deploy setup web"), "{shown}");
        assert_eq!(fixture.state.deployed, None, "nothing was recorded");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).expect("reads"),
            None,
            "and nothing was swapped: the refusal is before any of it"
        );
    }

    /// fails if a deploy that never comes up with nothing to fall back to
    /// is reported as a rollback that failed. Nothing was rolled back,
    /// because there was nothing to roll back to, and the wrapped form said
    /// "no previous release" and "still pointed at" once each in both of
    /// its layers.
    ///
    /// The record names a release retention has already reclaimed, which is
    /// how a real target reaches this. It used to name nothing at all, and
    /// that shape is now refused before the deploy starts: a tree whose
    /// cutover never landed has no business being deployed.
    #[tokio::test(start_paused = true)]
    async fn a_deploy_with_nothing_left_to_fall_back_to_says_so_plainly() {
        let mut fixture = fixture_before_any_release();
        fixture.state.deployed = Some(RECLAIMED.to_owned());
        let first = crate::git::remote_head(&fixture.tree.git(), "main").expect("head");

        let err = deploy(
            &Shepherd::never_ready(),
            &fixture.tree,
            &mut fixture.state,
            5,
        )
        .await
        .expect_err("nothing to roll back to");

        assert!(matches!(err, Error::Unverified { .. }), "{err:?}");
        let shown = err.to_string();
        assert!(shown.contains("web"), "{shown}");
        assert!(shown.contains(&first), "{shown}");
        // Not the number, which follows the app: what matters here is that
        // the reason survives into the sentence at all.
        assert!(shown.contains("did not come up and stay up"), "{shown}");
        assert!(shown.contains("after the reload"), "{shown}");
        assert!(!shown.contains("rolling back after"), "{shown}");
        assert_eq!(shown.matches("roll back").count(), 1, "{shown}");
        // It says what it looked at rather than asserting which case this
        // is. "This is its first deploy" is true here and false on a retry
        // after an interrupted one, which reaches the same variant.
        assert!(shown.contains("current"), "{shown}");
        assert!(shown.contains("deploy.toml"), "{shown}");
        assert!(!shown.contains("first deploy"), "{shown}");
        // The record never advanced, because nothing verified.
        assert_eq!(fixture.state.deployed.as_deref(), Some(RECLAIMED));
    }

    /// fails if "is there anything to roll back to" is decided by comparing
    /// path text. `previous` is link text read off disk and `attempted` is
    /// a sha this process just resolved, and one directory can be spelled
    /// two ways: `$SHEP_HOME` reaches this crate through
    /// `std::path::absolute`, which does not resolve symlinks, and the
    /// supervised dog and the operator's own CLI can each arrive at a
    /// different literal path. With a `deploy.toml` that lags `current`,
    /// which is the very case `restore` exists to repair, a path
    /// comparison then reports rolling back to the release that just
    /// failed - the same one, under its other name.
    #[tokio::test(start_paused = true)]
    async fn a_previous_release_under_another_path_is_not_a_rollback_target() {
        let mut fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");
        crate::git::fetch(&fixture.tree.git(), &fixture.state.remote).expect("fetch");
        install_release(&fixture, &second);
        // What an interrupted deploy leaves: `current` is on `second` and
        // the record still names an older release, one retention has since
        // reclaimed, so `current` is the only candidate left.
        fixture.state.deployed = Some(RECLAIMED.to_owned());

        // The same directory, spelled through a symlink - one process's
        // $SHEP_HOME, another process's.
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let link = elsewhere.path().join("home");
        std::os::unix::fs::symlink(fixture.home.path(), &link).expect("symlink the home");
        let same_tree = Tree::for_sheep(&link, "web");

        let err = deploy(&Shepherd::never_ready(), &same_tree, &mut fixture.state, 5)
            .await
            .expect_err("there is no other release to go back to");

        assert!(matches!(err, Error::Unverified { .. }), "{err:?}");
    }

    /// fails if a rollback repairs the filesystem and leaves the record
    /// wrong. A deploy killed between its reload and its `State::write`
    /// leaves `deploy.toml` lagging `current`; the next rollback is the
    /// moment that can be noticed, and a rollback that never writes leaves
    /// the record wrong indefinitely.
    #[tokio::test(start_paused = true)]
    async fn a_rollback_corrects_a_stale_deployed_record() {
        let mut fixture = fixture_with_previous_release();
        let live = fixture.state.deployed.clone().expect("a previous release");
        // What an interrupted deploy leaves behind: `current` names the
        // live release and the record still names an older one, since
        // reclaimed.
        fixture.state.deployed = Some(RECLAIMED.to_owned());
        commit_on_origin(&fixture, "second.txt");

        deploy(
            &Shepherd::never_ready(),
            &fixture.tree,
            &mut fixture.state,
            5,
        )
        .await
        .expect("completes");

        assert_eq!(fixture.state.deployed.as_deref(), Some(live.as_str()));
        assert_eq!(
            State::read(&fixture.tree.state_file())
                .expect("deploy.toml was written")
                .deployed
                .as_deref(),
            Some(live.as_str())
        );
    }

    /// fails if a rollback that cannot swap back reports only half of what
    /// happened. Either half alone is undiagnosable: the original failure
    /// says what the machine was trying to do, and the rollback's own
    /// failure says what state it is in now - `current` still naming a
    /// release that never came up.
    #[tokio::test(start_paused = true)]
    async fn a_swap_back_that_fails_reports_both_failures() {
        let mut fixture = fixture_with_previous_release();
        commit_on_origin(&fixture, "second.txt");
        let stale = fixture.tree.current().with_file_name("current.tmp");

        let err = deploy(
            &Shepherd::planting(stale),
            &fixture.tree,
            &mut fixture.state,
            5,
        )
        .await
        .expect_err("the swap back collides with the stale tmp link");

        assert!(matches!(err, Error::RollbackFailed { .. }), "{err:?}");
        let shown = err.to_string();
        assert!(shown.contains("did not come up"), "{shown}");
        assert!(shown.contains("current.tmp"), "{shown}");
    }

    /// fails if a deploy swaps over a `current` that moved while it was
    /// building. The release that moved it may be newer and already
    /// verified, so swapping on top of it silently reverts something that
    /// is live and healthy - and nothing anywhere reports a problem. The
    /// build command below is what a concurrent deploy's swap looks like
    /// from inside this one.
    // Paused, though nothing here waits on the clock when the guard holds:
    // if it ever stops holding, this deploy falls through to a real
    // verification window, and a test that fails in 90 seconds reads as a
    // hang rather than as the regression it is.
    #[tokio::test(start_paused = true)]
    async fn a_current_that_moved_during_the_build_is_not_swapped_over() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        fs::write(
            fixture.origin.path().join("Flockfile.toml"),
            format!("{FLOCKFILE}\n[build]\ncommand = 'ln -sfn \"$PWD\" ../../current'\n"),
        )
        .expect("write Flockfile");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::never_ready();

        let err = deploy(&daemon, &fixture.tree, &mut fixture.state, 5)
            .await
            .expect_err("current moved under it");

        assert!(matches!(err, Error::Raced { .. }), "{err:?}");
        assert_eq!(daemon.reload_count(), 0, "no reload may be sent at all");
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if setting the watch mode also deploys. `--watch manual` is
    /// what an operator reaches for during an incident, and a deploy firing
    /// at that moment is the exact opposite of what they asked for. The
    /// verb says deploy, which is why this needs a test rather than a
    /// comment.
    ///
    /// The brief's version of this test asserted on a recording daemon's
    /// call list. [`set_watch`] takes no daemon at all, so that assertion
    /// cannot be written - and its absence is the stronger guarantee: no
    /// daemon traffic is a type error here, not a test failure. What is
    /// left to check is the filesystem half, which a fall-through could
    /// still touch: no new release, `current` unmoved, `deployed`
    /// unchanged.
    #[tokio::test]
    async fn setting_the_watch_mode_does_not_deploy() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        let second = commit_on_origin(&fixture, "second.txt");

        set_watch(&fixture.tree, &mut fixture.state, Watch::Manual).expect("sets");

        assert_eq!(fixture.state.watch, Watch::Manual);
        assert_eq!(
            State::read(&fixture.tree.state_file())
                .expect("deploy.toml was written")
                .watch,
            Watch::Manual
        );
        assert!(
            !fixture.tree.release(&second).exists(),
            "no release may be built"
        );
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if a prune failure fails the deploy that triggered it. The
    /// `Landed::Verified` arm's `if let Err` exists precisely so that this
    /// cannot happen: a deploy that verified and wrote its record already
    /// succeeded, and a worktree that will not delete costs disk, not
    /// correctness.
    ///
    /// The failure is real, not simulated: `first`'s worktree admin
    /// bookkeeping is removed out from under it, so `git worktree remove
    /// --force` on the directory that is still sitting there answers
    /// "is not a working tree" - the same failure an operator's own `rm -rf`
    /// on a release directory would leave behind.
    #[tokio::test]
    async fn a_prune_failure_does_not_fail_the_deploy() {
        let mut fixture = fixture_with_previous_release();
        let first = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        deploy(&Shepherd::ready(), &fixture.tree, &mut fixture.state, 5)
            .await
            .expect("second release verifies");

        fs::remove_dir_all(fixture.tree.git().join("worktrees").join(&first))
            .expect("corrupt first's worktree bookkeeping");

        commit_on_origin(&fixture, "third.txt");
        let outcome = deploy(&Shepherd::ready(), &fixture.tree, &mut fixture.state, 2)
            .await
            .expect("a prune failure must not fail the deploy");

        assert!(matches!(outcome, Outcome::Deployed { .. }), "{outcome:?}");
    }
}
