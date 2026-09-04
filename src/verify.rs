//! Deciding whether a freshly reloaded sheep came up healthy.
//!
//! shep reports [`ProcStatus::Starting`] ("spawned, not yet ready") and
//! [`ProcStatus::Online`] ("running and, if configured, ready") for every
//! instance it supervises, and [`wait`] reads that verdict back through
//! [`Daemon::describe`] until either it holds or the deploy's grace period
//! runs out.
//!
//! # Why a status alone is not the verdict
//!
//! `Request::Reload` is an ACCEPTANCE, not a completion. shep answers it and
//! then runs `SpawnNew`, `AwaitReady`, `DrainOld`, `ReapOld` in its own actor
//! loop, so at the instant polling starts the OLD instance is still there and
//! still `Online`. A check that asks only "is any instance of this sheep
//! Online" therefore passes on the process that was already serving, before
//! the replacement has proved anything at all.
//!
//! It is worse than a race. When shep's own `AwaitReady` fails it
//! deliberately keeps the old instance serving, so the listing settles back
//! to exactly one `Online` entry: the old one. Measured against a real
//! shepherd, a release whose readiness probe could never pass looked like
//! this, one line per second:
//!
//! ```text
//! before  [(id 1, online,   pid 12835)]
//! t=1s    [(id 1, stopping, pid 12835), (id 2, starting, pid 13002)]
//! t=3s    [(id 1, online,   pid 12835)]      <- and forever after
//! ```
//!
//! A status-only check reports that deploy healthy on its first poll and on
//! every poll after it. Every rollback this crate can perform sits behind
//! that check, so getting it wrong disables the feature rather than
//! degrading it.
//!
//! # Generations
//!
//! [`Generation`] is the set of pids serving a sheep at some moment.
//! Verification captures one BEFORE the reload and then waits for the flock
//! to have turned over: every instance the shepherd reports must be running
//! under a pid that generation never had, and there has to be at least one.
//!
//! Pid, rather than [`ProcessInfo::id`] or `uptime_ms`. A pid is the
//! operating system's own answer to "is this a different process", which is
//! the exact question, and it needs no reasoning about shep's bookkeeping to
//! interpret. `id` would work today (the measurement above shows the
//! replacement getting a fresh one), but it is shep's numbering rather than
//! the machine's fact, and `uptime_ms` only ever supports an inference about
//! which process is which. The one thing pid cannot rule out is the kernel
//! recycling a pid onto the replacement within the same reload, which is not
//! worth defending against here.
//!
//! Every instance, not any: a sheep scaled to several instances reloads them
//! one at a time, and "any new instance is Online" would report success while
//! shep was still keeping an old process alive for the ones whose
//! `AwaitReady` failed. Waiting for a total turnover also absorbs the drain
//! window for free, since an old instance still draining is reported as
//! `Stopping` under its old pid and simply is not a turnover yet.

use std::collections::BTreeSet;
use std::time::Duration;

use shep_client::shep_core::protocol::ProcessInfo;
use shep_client::shep_core::status::ProcStatus;
use tokio::time::{Instant, sleep};

use crate::daemon::Daemon;
use crate::error::Error;
use crate::state::Verify;

/// The pids serving one sheep at a moment in time.
///
/// Captured before a reload so that [`wait`] can tell the replacement from
/// the process that was already there. See the module doc for why this
/// exists at all, and why it is pids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Generation {
    /// The pids that had a process.
    pids: BTreeSet<u32>,
    /// Every row the shepherd listed, with or without a pid.
    ///
    /// Kept apart from `pids` because they can differ, and the difference
    /// used to fail healthy deploys: a replica in `WaitingRestart` has no
    /// pid, so it was missing from `pids` and present in the listing, and
    /// [`Self::has_turned_over`] compared the pid count against the row
    /// count and never found them equal.
    rows: u32,
}

impl Generation {
    /// The generation serving `sheep` right now.
    ///
    /// A sheep the shepherd has never heard of, or one with no running
    /// instance, gives an empty generation rather than an error: nothing was
    /// serving, so anything that comes up is new.
    ///
    /// # Errors
    /// Whatever [`Daemon::describe`] returns.
    pub async fn of<D: Daemon>(daemon: &D, sheep: &str) -> Result<Self, Error> {
        let flock = daemon.describe(sheep).await?;
        Ok(Self::of_infos(&flock.iter().collect::<Vec<_>>()))
    }

    /// How many instances this generation has.
    ///
    /// Measured, which is the point: `AppConfig::instances` is the count the
    /// release's Flockfile ASKS for, and `shep stock <sheep> <n>` changes
    /// what is actually running without touching that file. A reload costs
    /// one swap per instance the shepherd lists, so this is the number
    /// `crate::deploy`'s budget has to multiply by, and the number a
    /// turnover has to come back at.
    #[must_use]
    pub fn instances(&self) -> u32 {
        self.rows
    }

    /// The generation a listing already in hand describes.
    ///
    /// [`Self::of`] takes a [`Daemon`] and issues a request; this takes the
    /// answer to one. The cutover's first phase already holds the listing it
    /// wants to freeze, and asking again would both cost a round trip and
    /// risk freezing a DIFFERENT set of pids than the one it just decided
    /// on, which is the sort of gap a crash-looping release fits through.
    ///
    /// `crate::optin`'s cutover uses it twice for that reason: once on the
    /// listing it reads before the `Start`, whose ids it also keeps, so the
    /// ids to delete and the pids to compare against describe one instant
    /// rather than two.
    pub(crate) fn of_infos(infos: &[&ProcessInfo]) -> Self {
        Self {
            pids: infos.iter().filter_map(|info| info.pid).collect(),
            // A flock of more than u32::MAX processes is not a thing this
            // host could be running, but saturating says so without a cast
            // that could wrap.
            rows: u32::try_from(infos.len()).unwrap_or(u32::MAX),
        }
    }

    /// Whether `info` is running under a pid this generation never had.
    ///
    /// An instance with no pid at all is never new. That is the honest
    /// reading rather than a defensive one: a `WaitingRestart` or errored
    /// instance has no process, and a deploy verified against one would be
    /// verified against nothing.
    ///
    /// Two callers, asking the same question for different reasons. [`wait`]
    /// asks it inside [`Self::has_turned_over`], which additionally requires
    /// EVERY instance to be new - a reload replaces them all, so an old one
    /// still present means the reload is not finished. `crate::optin`'s
    /// cutover asks it bare, because a cutover deliberately leaves the old
    /// instance running beside the new one and a full turnover would never
    /// arrive. Do not "fix" either call site by tightening it toward the
    /// other: they are different requests to the shepherd, not two spellings
    /// of one.
    pub(crate) fn is_new(&self, info: &ProcessInfo) -> bool {
        info.pid.is_some_and(|pid| !self.pids.contains(&pid))
    }

    /// Whether `info` is one of this generation's own instances.
    ///
    /// The mirror of [`Self::is_new`], for the dwell: after a turnover the
    /// question stops being "is this different" and becomes "is this still
    /// the same one". `crate::optin`'s cutover dwells on the same question
    /// about the instance its own `Start` spawned.
    pub(crate) fn holds(&self, info: &ProcessInfo) -> bool {
        info.pid.is_some_and(|pid| self.pids.contains(&pid))
    }

    /// Whether `flock` is entirely made of instances this generation never
    /// had, in a state `accept` is happy with.
    ///
    /// Empty is never a turnover: a sheep the shepherd cannot find has not
    /// been verified.
    fn has_turned_over(&self, flock: &[ProcessInfo], accept: fn(&ProcessInfo) -> bool) -> bool {
        // The count as well, measured against what was running BEFORE. A
        // two-instance sheep that loses a replica during the reload lists one
        // new instance, and one new instance is "all new": the turnover was
        // accepted, `settled` became that single pid, and every later check
        // compared the survivor against itself. A sheep that came back at half
        // strength verified.
        //
        // Checked here rather than at the dwell, though the dwell is where it
        // was found. `Verify::Probed` returns as soon as the turnover lands and
        // never reaches a dwell, so a check placed there would leave the
        // stricter of the two modes the blind one.
        //
        // Skipped when nothing was running before, because then there is no
        // count to hold to and `!flock.is_empty()` is the whole of what can be
        // asked.
        // Rows against rows. `instances()` counts every row the shepherd
        // listed before the reload, pid or no pid, so a replica that was
        // between restarts at capture time still counts as one the reload
        // has to bring back. Counting pids instead undercounted by exactly
        // those, and a flock that came back whole could never match.
        let count_holds = self.instances() == 0
            || u32::try_from(flock.len()).is_ok_and(|listed| listed == self.instances());

        !flock.is_empty()
            && count_holds
            && flock.iter().all(|info| self.is_new(info) && accept(info))
    }
}

/// How long `Alive` watches a turned-over flock before believing it.
///
/// The spec's own wording for `alive` is "wait N seconds and confirm the
/// process is still running", and this is that N. It is a dwell after the
/// turnover rather than a window the turnover has to fit inside: how long a
/// reload takes is the app's business, and `crate::deploy` derives that from
/// the app, while how long a new release has to survive before anyone
/// believes in it is this crate's.
pub(crate) const DWELL: Duration = Duration::from_secs(10);

/// How often either mode asks the shepherd again.
///
/// One cadence for the crate: `crate::optin`'s cutover polls on this too,
/// because "how often do we ask the shepherd again" has one answer here.
pub(crate) const POLL: Duration = Duration::from_millis(100);

/// The most any wait on the shepherd may last, whatever the Flockfile says.
///
/// An hour. Every budget in this crate is built from the release's own
/// `listen_timeout` and `graceful_timeout`, and those come out of a file
/// the deployed repository commits. `UpDuration` accepts values up to
/// `u64::MAX` milliseconds, so a Flockfile could ask this dog to watch a
/// reload for half a billion years, holding the tree's lock and the poll
/// loop with it, and a value large enough overflowed `Instant + Duration`
/// and took the dog down instead. An hour is past any reload that could
/// still succeed and short of any of that.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(60 * 60);

/// `budget`, held under [`MAX_WAIT`].
///
/// Every budget derived from a Flockfile goes through here before it is
/// added to a clock. Both callers say so at the site.
#[must_use]
pub(crate) fn bounded(budget: Duration) -> Duration {
    budget.min(MAX_WAIT)
}

/// Watch `sheep` until the flock has turned over, which it gets `budget` to
/// do, judging health the way `mode` says to against the generation that
/// was serving before the reload.
///
/// Both modes poll, and both wait for the same turnover: every instance the
/// shepherd reports running under a pid `before` never had. They differ only
/// in what they then accept, and in what they do afterwards.
///
/// `Probed` returns as soon as the turned-over flock is all
/// [`ProcStatus::Online`]. That can only mean anything for a sheep whose
/// readiness is gated - a probe, or `wait_ready` on the channel - since
/// otherwise shep reports `Online` for a process that merely has not died;
/// `deploy` refuses a `Probed` target with neither before it touches
/// anything.
///
/// `Alive` accepts `Starting` as well, because a sheep with no gate is
/// exactly what it exists for, and then dwells: it sleeps [`DWELL`], then
/// polls for the same pids to still be there AND to have reached `Online`,
/// for as long as the turnover left of `budget` or one more [`DWELL`],
/// whichever is longer. So `Alive` can run past `budget` by up to three
/// dwells (the sleep, the floor on the deadline, and one `describe` that
/// starts just inside it), and that is the shape the spec asks for: "wait N
/// seconds and confirm the process is still running". A process that came up and died
/// inside the dwell has a different pid by then (shep restarts it) or none
/// at all, either of which fails. `WaitingRestart` is a fail throughout for
/// the same reason.
///
/// # Why `Alive` polls at all
///
/// It used to sleep one fixed window and sample once. That shape cannot tell
/// "the reload is still in flight" from "the release failed", and the
/// difference matters as soon as a wrong answer rolls back: a probeless app
/// takes shep's heuristic readiness path, which sleeps the app's whole
/// `listen_timeout` per instance, so a reload can legitimately outlive any
/// fixed window. Measured, a one-instance probeless app with
/// `listen_timeout = "15s"` had a perfectly good release given up on at 10
/// seconds, rolled back, and the rollback's own reload refused because the
/// first one was still running.
///
/// # Errors
/// Whatever [`Daemon::describe`] returns - either mode can surface a
/// transient error mid-poll rather than only at the deadline.
pub async fn wait<D: Daemon>(
    daemon: &D,
    sheep: &str,
    mode: Verify,
    before: &Generation,
    budget: Duration,
) -> Result<bool, Error> {
    let accept = match mode {
        Verify::Probed => is_online,
        Verify::Alive => is_alive,
    };

    let started = Instant::now();
    let Some(settled) = turnover(daemon, sheep, before, accept, budget).await? else {
        return Ok(false);
    };

    if mode == Verify::Probed {
        return Ok(true);
    }

    sleep(DWELL).await;
    // How long the replacement gets to reach `Online` after the dwell: what
    // is left of the budget, and never less than one more dwell.
    //
    // The budget, because the budget is what knows the app. A probeless
    // instance is marked `Online` by shep's heuristic path when its
    // `listen_timeout` elapses, and `budget` is built from that field. A
    // fixed second dwell here capped the whole wait at twenty seconds
    // whatever the app declared, so `verify = "alive"` on an app with
    // `listen_timeout = "30s"` was rolled back, healthy, every time, at the
    // cost of a second live reload. The README's own example uses fifteen
    // seconds, which is why it looked fine.
    let deadline = (started + budget).max(Instant::now() + DWELL);
    // Retried like the one in `turnover`, and for the same reason. Retrying
    // there and not here left `Verify::Alive` with exactly the bug the retry
    // was added to fix: one transient answer after the dwell propagates,
    // `land` reads it as a failure, and a healthy release is rolled back at
    // the cost of a second live reload. Found on 2026-08-28, in review of the
    // commit that added the first retry.
    // Polled for `Online`, not sampled once for "still alive", and the
    // difference is what shep 0.1.10 made necessary.
    //
    // A probed app now reloads serially, so by the time readiness resolves the
    // old instance has drained and there is nothing to abort back to. Rather
    // than mark a replacement `Online` it knows never became ready, shep
    // leaves it `Starting` and fires `ReloadAbandoned`. Its own comment gives
    // the reason: answering `online` for an instance that never came up "is
    // how a broken release gets recorded as deployed". A replacement stuck
    // that way has no liveness loop either, because those are armed at
    // `went_online`.
    //
    // Accepting `Starting` here threw that away and recorded exactly what
    // shep had just refused to claim.
    //
    // Requiring `Online` at a single sample was the first attempt and is
    // wrong. It would fail a healthy probeless app whose `listen_timeout`
    // outlasts the dwell, which is not hypothetical: this crate's own README
    // measures a one-instance probeless app at `listen_timeout = "15s"` being
    // given up on at ten seconds, and calls that the bug the polling turnover
    // above exists to prevent. Reintroducing it here would be the same
    // mistake one phase later.
    //
    // So the question is not "is it Starting" but "is it STILL Starting after
    // long enough". A slow app reaches `Online` on its own; an abandoned
    // replacement never does, because nothing is left to move it.
    loop {
        let flock = describe_within(daemon, sheep, DWELL).await?;
        // The COUNT as well as the pids. `all` over a listing that shrank is
        // vacuously happy: with `settled` holding two, a listing of one
        // `Online` pid from that generation satisfies every element and the
        // sibling that vanished goes unmentioned. `alive_rejects_a_flock_that_empties_during_the_dwell`
        // only pins the all-gone case.
        let settled_and_ready = u32::try_from(flock.len()).is_ok_and(|n| n == settled.instances())
            && flock
                .iter()
                .all(|info| settled.holds(info) && is_online(info));
        if settled_and_ready {
            return Ok(true);
        }
        // A pid that changed, or an instance that died, is a verdict rather
        // than something more waiting can fix.
        if flock.is_empty()
            || !flock
                .iter()
                .all(|info| settled.holds(info) && is_alive(info))
        {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        sleep(POLL).await;
    }
}

/// [`Daemon::describe`], retrying an answer that could clear on its own.
///
/// The judgement is [`Error::is_retryable`]'s, which the crate already applies
/// to `reload` over the same socket: a `Timeout` or an `Internal` is the
/// shepherd being busy, a `NotFound` is a question that will never have a
/// different answer.
///
/// `budget` bounds the retrying, so a shepherd that is genuinely gone still
/// fails rather than hanging.
///
/// It does NOT bound the asking, and that is deliberate rather than loose.
/// A caller's wall-clock can therefore exceed its own `budget` by one slow
/// answer, which round 7 of the founder's review raised. Kept, for two
/// reasons. The overrun is bounded without any help from here: every
/// `describe` goes through `shep_client`'s `Client::request`, which waits
/// `DEFAULT_DEADLINE` plus `DEADLINE_GRACE`, five seconds and two, the same
/// pair `crate::deploy` already names in four places.
/// And the obvious fix is worse than the problem: wrapping the call in a
/// timeout against what is left of the budget means the last poll of a
/// `turnover` gets almost no time and fails for that reason, which is how a
/// healthy release gets rolled back at the cost of a second live reload. That
/// is the exact failure the retry above this line was added to prevent.
///
/// # Errors
/// Whatever [`Daemon::describe`] returns, once a failure is either fatal or
/// the budget is spent.
async fn describe_within<D: Daemon>(
    daemon: &D,
    sheep: &str,
    budget: Duration,
) -> Result<Vec<ProcessInfo>, Error> {
    let deadline = Instant::now() + budget;
    loop {
        match daemon.describe(sheep).await {
            Ok(flock) => return Ok(flock),
            Err(err) if !err.is_retryable() => return Err(err),
            Err(err) => {
                // `budget` bounds the RETRYING, never the asking. The caller
                // wanted one describe and gets it whatever the clock says;
                // `turnover` in particular hands over whatever is left of its
                // own deadline and still expects the question put, because its
                // loop is what decides that time is up.
                //
                // The sleep is clamped so a retry cannot run past the
                // deadline, which is the part that was genuinely loose:
                // `POLL` is 100ms and a budget with less than that remaining
                // used to overshoot it.
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(err);
                }
                sleep(POLL.min(left)).await;
            }
        }
    }
}

/// Polls until `before` has been replaced by instances `accept` is happy
/// with, and answers with the generation that replaced it.
///
/// `None` is the deadline passing with no such turnover, which is the
/// failure both modes report.
///
/// # Errors
/// Whatever [`Daemon::describe`] returns, once a failure is either fatal or
/// the budget has run out. A retryable failure inside the budget is polled
/// through rather than reported: see the body for why one blip must not cost
/// a live reload.
async fn turnover<D: Daemon>(
    daemon: &D,
    sheep: &str,
    before: &Generation,
    accept: fn(&ProcessInfo) -> bool,
    budget: Duration,
) -> Result<Option<Generation>, Error> {
    let deadline = Instant::now() + budget;
    loop {
        // A transient answer is not a verdict. This polls roughly a hundred
        // and fifty times over a ten-to-twenty second budget, so a single
        // `Timeout` or `Internal` anywhere in that run used to abort
        // verification and send `land` down the rollback path: a second real
        // reload, under live traffic, for a release that was healthy.
        //
        // Same judgement `Error::is_retryable` already made for `reload` over
        // the same socket. Retried inside the budget rather than beyond it,
        // so a shepherd that is genuinely gone still fails at the deadline
        // instead of hanging.
        let flock = describe_within(
            daemon,
            sheep,
            deadline.saturating_duration_since(Instant::now()),
        )
        .await?;
        if before.has_turned_over(&flock, accept) {
            return Ok(Some(Generation::of_infos(
                &flock.iter().collect::<Vec<_>>(),
            )));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        sleep(POLL).await;
    }
}

/// Whether one instance has passed its readiness probe, for [`Verify::Probed`].
fn is_online(info: &ProcessInfo) -> bool {
    info.status == ProcStatus::Online
}

/// Whether one instance still counts as "running" for [`Verify::Alive`].
///
/// Shared with `crate::optin`'s cutover rather than copied into it: the
/// cutover's accept predicate IS this one, and two spellings of it would
/// drift apart with nothing to notice.
pub(crate) fn is_alive(info: &ProcessInfo) -> bool {
    matches!(info.status, ProcStatus::Starting | ProcStatus::Online)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use shep_client::RequestError;

    use super::*;

    /// One instance, with the id and pid the tests care about.
    fn instance(id: u32, status: ProcStatus, pid: u32) -> ProcessInfo {
        ProcessInfo::builder(id, "web", status)
            .pid(Some(pid))
            .build()
    }

    /// The generation a reload would be replacing: one instance at `pid`.
    fn serving(pid: u32) -> Generation {
        Generation {
            pids: [pid].into_iter().collect(),
            rows: 1,
        }
    }

    /// The generation a reload would be replacing: one instance per pid.
    fn serving_all(pids: &[u32]) -> Generation {
        Generation {
            pids: pids.iter().copied().collect(),
            rows: u32::try_from(pids.len()).expect("a small flock"),
        }
    }

    /// A [`Daemon`] whose `describe` walks through a fixed sequence of whole
    /// flock listings, one per call, repeating the last once the sequence is
    /// exhausted rather than panicking or returning an empty `Vec`. `Probed`
    /// calls `describe` many times inside one wait, and these tests only
    /// need to name the interesting moments.
    ///
    /// Whole listings rather than statuses: what `wait` judges is the shape
    /// of the flock - which pids are in it, not only what state one of them
    /// is in - so a fake that could only vary the status could not express
    /// the failure this module exists to catch.
    struct Listings {
        sequence: Vec<Vec<ProcessInfo>>,
        next: Cell<usize>,
    }

    impl Listings {
        fn new(sequence: Vec<Vec<ProcessInfo>>) -> Self {
            Self {
                sequence,
                next: Cell::new(0),
            }
        }
    }

    /// A daemon that fails its first `describe` with a retryable error, then
    /// answers with a flock that has turned over.
    struct Blips {
        calls: Cell<u32>,
        after: Vec<ProcessInfo>,
    }

    impl Daemon for Blips {
        async fn describe(&self, _sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            if n == 0 {
                return Err(Error::Request(RequestError::Timeout {
                    after: Duration::from_secs(1),
                }));
            }
            Ok(self.after.clone())
        }

        crate::fixtures::daemon_methods!(unimplemented;
            dog_config, list_flock, start, delete, reload, restart, save_roll, set_smit,
        );
    }

    /// fails if one transient `describe` failure costs a live reload.
    ///
    /// `turnover` polls roughly a hundred and fifty times over a ten-second
    /// budget. Propagating any one of those used to make `land` report
    /// `Landed::Failed`, which rolls back: `current` is put back and the sheep
    /// is reloaded a SECOND time, under live traffic, for a release that was
    /// healthy and one poll from Online.
    ///
    /// The error here is `Timeout`, which `Error::is_retryable` already
    /// classifies as transient for `reload` over the same socket. A fatal code
    /// must still fail, which `only_a_failure_that_could_clear_is_retried` in
    /// `crate::error` pins from the other side.
    #[tokio::test]
    async fn a_transient_describe_failure_does_not_fail_verification() {
        let before = serving(111);
        let daemon = Blips {
            calls: Cell::new(0),
            after: vec![instance(0, ProcStatus::Online, 222)],
        };

        let seen = turnover(&daemon, "web", &before, is_online, Duration::from_secs(10))
            .await
            .expect("a retryable blip must not end verification");

        assert!(
            seen.is_some(),
            "the turnover after the blip must still be seen"
        );
        assert!(daemon.calls.get() >= 2, "it must have asked again");
    }

    impl Daemon for Listings {
        async fn describe(&self, _sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            let Some(last) = self.sequence.len().checked_sub(1) else {
                return Ok(Vec::new());
            };
            let index = self.next.get().min(last);
            self.next.set((index + 1).min(last));
            Ok(self.sequence[index].clone())
        }

        crate::fixtures::daemon_methods!(unimplemented;
            dog_config, list_flock, start, delete, reload, restart, save_roll, set_smit,
        );
    }

    /// fails if the process that was already serving passes for the one the
    /// deploy just installed. This is the shape a real shepherd settles into
    /// when its own `AwaitReady` fails: it keeps the old instance, so the
    /// listing is one `Online` entry under the old pid, forever. A check
    /// that asks only about status reports that deploy healthy and every
    /// rollback in this crate becomes unreachable.
    #[tokio::test(start_paused = true)]
    async fn the_old_instance_still_online_is_not_a_new_release() {
        let daemon = Listings::new(vec![vec![instance(1, ProcStatus::Online, 12835)]]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if a reload that shep accepted but never completed is treated
    /// as verified. `Request::Reload` is an acceptance, not a completion, so
    /// the first poll of a perfectly healthy deploy sees exactly this: the
    /// old instance, alone and Online, because the replacement has not been
    /// spawned yet.
    #[tokio::test(start_paused = true)]
    async fn a_reload_that_has_not_happened_yet_is_not_success() {
        let daemon = Listings::new(vec![
            vec![instance(1, ProcStatus::Online, 12835)],
            vec![instance(1, ProcStatus::Online, 12835)],
        ]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if a replacement that reaches Online is not treated as success.
    /// The listing walks the real sequence: the old instance alone, then the
    /// two overlapping while the old drains, then the new one alone.
    #[tokio::test(start_paused = true)]
    async fn a_new_instance_reaching_online_is_success() {
        let daemon = Listings::new(vec![
            vec![instance(1, ProcStatus::Online, 12835)],
            vec![
                instance(1, ProcStatus::Stopping, 12835),
                instance(2, ProcStatus::Starting, 13002),
            ],
            vec![instance(2, ProcStatus::Online, 13002)],
        ]);
        assert!(
            wait(
                &daemon,
                "web",
                Verify::Probed,
                &serving(12835),
                Duration::from_secs(5)
            )
            .await
            .unwrap()
        );
    }

    /// fails if a draining old instance is enough to call the deploy done.
    /// During a reload the listing carries both, and the new one can be
    /// Online while the old one is still Stopping - a turnover is not
    /// finished until the old process is gone.
    #[tokio::test(start_paused = true)]
    async fn a_draining_old_instance_is_not_a_finished_turnover() {
        let daemon = Listings::new(vec![vec![
            instance(1, ProcStatus::Stopping, 12835),
            instance(2, ProcStatus::Online, 13002),
        ]]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if Starting is mistaken for success under `Probed`.
    /// `ProcStatus::Starting` means "spawned, not yet ready" and `Online`
    /// means "running and, if configured, ready", so `Online` is the only
    /// status that means the probe passed.
    #[tokio::test(start_paused = true)]
    async fn a_new_instance_that_never_leaves_starting_is_not_success() {
        let daemon = Listings::new(vec![vec![instance(2, ProcStatus::Starting, 13002)]]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if `Verify::Alive` starts demanding a prompt probe. Alive is the
    /// deliberate, visible downgrade for a sheep with no probe: a new process
    /// that comes up and stays up is enough, and it is given the whole dwell
    /// to get there where `Probed` wants `Online` at the turnover.
    ///
    /// The fake reaches `Online` on its second answer, which it did not have
    /// to before. A probeless app takes shep's heuristic path and is marked
    /// `Online` at its `listen_timeout`, so a listing that says `Starting`
    /// forever is not a slow healthy app: since shep 0.1.10 it is the shape of
    /// a reload shep abandoned, and `alive_rejects_a_replacement_shep_gave_up_on`
    /// below is what pins that.
    #[tokio::test(start_paused = true)]
    async fn alive_accepts_a_new_process_that_is_still_running() {
        let daemon = Listings::new(vec![
            vec![instance(2, ProcStatus::Starting, 13002)],
            vec![instance(2, ProcStatus::Online, 13002)],
        ]);
        assert!(
            wait(
                &daemon,
                "web",
                Verify::Alive,
                &serving(12835),
                Duration::from_millis(50)
            )
            .await
            .unwrap()
        );
    }

    /// fails if `Verify::Alive` accepts the process that was already there.
    /// Alive is a downgrade on what "healthy" means, not on whether a
    /// deploy happened at all: a reload that put nothing new in place has
    /// not been verified by either mode.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_the_old_process_still_running() {
        let daemon = Listings::new(vec![vec![instance(1, ProcStatus::Online, 12835)]]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &serving(12835),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if a new instance that already died and is backed off waiting
    /// to be restarted counts as alive. This is the reject side of
    /// `is_alive`, and it is the failure `Alive` exists to catch: the
    /// process was spawned, so its pid is new, and it did not survive the
    /// window.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_an_instance_waiting_to_be_restarted() {
        let daemon = Listings::new(vec![vec![instance(2, ProcStatus::WaitingRestart, 13002)]]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &serving(12835),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if `Alive` gives up on a reload that is still in flight. A
    /// probeless app takes shep's heuristic readiness path, which sleeps
    /// the app's whole `listen_timeout` per instance, so the old instance
    /// is all there is to see for as long as that takes. Sampling once at a
    /// fixed ten seconds called a perfectly good release dead, rolled it
    /// back, and then could not reload because the first reload was still
    /// running.
    #[tokio::test(start_paused = true)]
    async fn alive_waits_out_a_reload_that_is_still_in_flight() {
        let daemon = Listings::new(vec![
            vec![instance(1, ProcStatus::Online, 12835)],
            vec![instance(1, ProcStatus::Online, 12835)],
            vec![instance(1, ProcStatus::Online, 12835)],
            vec![instance(2, ProcStatus::Starting, 13002)],
            // Then ready, which is what the heuristic path does at
            // `listen_timeout`. Without this the fake describes a replacement
            // that never becomes ready, which is a different case entirely and
            // has its own test below.
            vec![instance(2, ProcStatus::Online, 13002)],
        ]);
        assert!(
            wait(
                &daemon,
                "web",
                Verify::Alive,
                &serving(12835),
                Duration::from_secs(120)
            )
            .await
            .unwrap()
        );
    }

    /// fails if a flock that came back SMALLER is accepted as a turnover.
    ///
    /// "All new" is true of one new instance, so a two-instance sheep that
    /// loses a replica during the reload turns over on the survivor alone.
    /// `settled` then becomes that single pid and every later check compares
    /// the survivor against itself, including the dwell's own count. A sheep
    /// at half strength verified, and both modes believed it.
    ///
    /// Pinned at the turnover rather than the dwell because `Verify::Probed`
    /// returns as soon as the turnover lands and never dwells at all.
    #[tokio::test(start_paused = true)]
    async fn a_turnover_that_lost_a_replica_is_not_a_turnover() {
        // Two before, one after, and that one is new and healthy.
        let daemon = Listings::new(vec![vec![instance(1, ProcStatus::Online, 200)]]);
        let before = serving_all(&[100, 101]);

        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &before,
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        assert!(!ok, "one replacement for two instances is not a turnover");
    }

    /// fails if the dwell stops counting how many instances are left.
    ///
    /// `all` over a listing that shrank is vacuously happy: with `settled`
    /// holding two pids, a listing of ONE `Online` pid from that generation
    /// satisfies every element, and the sibling that vanished goes unmentioned.
    /// A scaled sheep that comes back at half strength is a release that did
    /// not deploy, and the dwell is the only thing looking.
    ///
    /// `alive_rejects_a_flock_that_empties_during_the_dwell` pins the all-gone
    /// case, which is the easy half.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_a_flock_that_came_back_smaller() {
        let daemon = Listings::new(vec![
            vec![
                instance(1, ProcStatus::Online, 200),
                instance(2, ProcStatus::Online, 201),
            ],
            // One of the two is gone by the dwell. The survivor is healthy and
            // from the right generation, which is what let this pass without a
            // count.
            vec![instance(1, ProcStatus::Online, 200)],
        ]);
        let before = serving_all(&[100, 101]);

        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &before,
            Duration::from_secs(120),
        )
        .await
        .unwrap();

        assert!(!ok, "half a flock is not a deployed release");
    }

    /// fails if `Alive` believes a replacement shep gave up on.
    ///
    /// shep 0.1.10 reloads a probed app serially, so when readiness resolves
    /// the old instance has already drained and there is nothing to abort back
    /// to. Rather than claim `Online` for a replacement it knows never became
    /// ready, shep leaves it `Starting` and fires `ReloadAbandoned`. Its own
    /// comment says why: answering `online` for an instance that never came up
    /// "is how a broken release gets recorded as deployed". Such an instance
    /// has no liveness loop either, since those are armed at `went_online`.
    ///
    /// Accepting that as alive threw the signal away and recorded the thing
    /// shep had just refused to claim. Caught against a real shepherd by the
    /// integration tier, which is the only place it could be caught: no fake
    /// in this module knew `Starting` had acquired a meaning.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_a_replacement_shep_gave_up_on() {
        // Starting forever, which is exactly what an abandoned reload leaves.
        let daemon = Listings::new(vec![vec![instance(2, ProcStatus::Starting, 13002)]]);

        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &serving(12835),
            Duration::from_secs(120),
        )
        .await
        .unwrap();

        assert!(
            !ok,
            "a replacement left at `starting` never came up, whatever verify mode asked"
        );
    }

    /// fails if `Alive` believes a release that came up and then died. The
    /// dwell is the whole of what `alive` promises - "wait N seconds and
    /// confirm the process is still running" - and a single sample taken at
    /// the moment of turnover confirms nothing. shep restarts the corpse,
    /// so what the dwell sees is a pid that is new again.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_a_process_that_dies_during_the_dwell() {
        let daemon = Listings::new(vec![
            vec![instance(2, ProcStatus::Starting, 13002)],
            vec![instance(2, ProcStatus::Starting, 13456)],
        ]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &serving(12835),
            Duration::from_secs(120),
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if the dwell settles for SOME instance still being the one that
    /// came up, rather than all of them.
    ///
    /// The module argues this for the turnover phase and
    /// `a_draining_old_instance_is_not_a_finished_turnover` pins it there.
    /// The identical requirement in the post-dwell check had no test with more
    /// than one instance in it, so round 9 of the founder's review changed
    /// that `.all` to `.any` and both existing dwell tests stayed green: over
    /// a one-element iterator the two are the same function.
    ///
    /// A scaled sheep is why it matters. One replica of two coming up, dying
    /// and being restarted under a new pid inside the dwell is a release that
    /// does not work, and `.any` calls it verified because its sibling is
    /// fine.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_one_replica_of_two_restarting_during_the_dwell() {
        let daemon = Listings::new(vec![
            // Turned over: both replicas are new, so `settled` is {200, 201}.
            vec![
                instance(1, ProcStatus::Starting, 200),
                instance(2, ProcStatus::Starting, 201),
            ],
            // After the dwell 201 has died and come back as 202. The first
            // replica still holds, which is exactly what `.any` would accept.
            vec![
                instance(1, ProcStatus::Starting, 200),
                instance(2, ProcStatus::Starting, 202),
            ],
        ]);
        let before = serving_all(&[100, 101]);

        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &before,
            Duration::from_secs(120),
        )
        .await
        .unwrap();

        assert!(
            !ok,
            "every instance must still be the one that came up, not merely one of them"
        );
    }

    /// fails if the dwell stops noticing a process that is gone entirely by
    /// the end of it - the other half of
    /// `alive_rejects_a_process_that_dies_during_the_dwell`, where shep has
    /// not restarted it yet and there is nothing left to describe.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_a_flock_that_empties_during_the_dwell() {
        let daemon = Listings::new(vec![
            vec![instance(2, ProcStatus::Starting, 13002)],
            Vec::new(),
        ]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &serving(12835),
            Duration::from_secs(120),
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if an instance with no pid at all is treated as a new one.
    /// `WaitingRestart` covers the status side of the same case; this
    /// covers the pid side, which is what `Generation::is_new` actually
    /// asks. A deploy verified against an instance with no process has been
    /// verified against nothing.
    #[tokio::test(start_paused = true)]
    async fn an_instance_with_no_pid_is_not_a_new_generation() {
        let daemon = Listings::new(vec![vec![
            ProcessInfo::builder(2, "web", ProcStatus::Online).build(),
        ]]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if a sheep the shepherd cannot find (an empty `describe`
    /// result) is ever treated as verified. Both modes must fail closed
    /// here: there is nothing to have come up healthy.
    #[tokio::test(start_paused = true)]
    async fn an_empty_describe_is_failure_in_both_modes() {
        let empty = Listings::new(Vec::new());
        assert!(
            !wait(
                &empty,
                "web",
                Verify::Probed,
                &serving(12835),
                Duration::from_millis(50)
            )
            .await
            .unwrap()
        );
        let empty = Listings::new(Vec::new());
        assert!(
            !wait(
                &empty,
                "web",
                Verify::Alive,
                &serving(12835),
                Duration::from_millis(50)
            )
            .await
            .unwrap()
        );
    }

    /// fails if the fixture stops repeating its last listing once its
    /// sequence is exhausted, whether by panicking or by returning an empty
    /// `Vec`. `Probed` polls every 100ms, so a 350ms window against a
    /// two-element sequence outlives it several times over; every one of
    /// those extra polls must keep reading the last listing rather than
    /// running off the end.
    #[tokio::test(start_paused = true)]
    async fn the_fixture_repeats_its_last_listing_past_exhaustion() {
        let daemon = Listings::new(vec![
            vec![instance(1, ProcStatus::Online, 12835)],
            vec![instance(1, ProcStatus::Online, 12835)],
        ]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_millis(350),
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if capturing the pre-reload generation loses a pid, or invents
    /// one for an instance that has none. Everything above rests on this
    /// set being what was actually serving.
    #[tokio::test(start_paused = true)]
    async fn a_generation_is_the_pids_that_are_serving() {
        let daemon = Listings::new(vec![vec![
            instance(1, ProcStatus::Online, 12835),
            instance(2, ProcStatus::Online, 12836),
            ProcessInfo::builder(3, "web", ProcStatus::WaitingRestart).build(),
        ]]);
        let generation = Generation::of(&daemon, "web").await.unwrap();
        assert_eq!(
            generation,
            Generation {
                pids: [12835, 12836].into_iter().collect(),
                rows: 3,
            }
        );
    }

    /// fails if a replica that was between restarts when the generation was
    /// captured stops the reload from ever being seen as complete.
    ///
    /// `WaitingRestart` has no pid, so it was missing from the pid set and
    /// present in the shepherd's listing. `has_turned_over` compared the two
    /// counts, found two against three forever, and rolled back a flock that
    /// had come back whole. Three rows before, three new pids after, must be
    /// a turnover.
    #[tokio::test(start_paused = true)]
    async fn a_replica_between_restarts_before_the_reload_still_counts() {
        let capture = Listings::new(vec![vec![
            instance(1, ProcStatus::Online, 100),
            instance(2, ProcStatus::Online, 101),
            ProcessInfo::builder(3, "web", ProcStatus::WaitingRestart).build(),
        ]]);
        let before = Generation::of(&capture, "web").await.unwrap();

        let daemon = Listings::new(vec![vec![
            instance(1, ProcStatus::Online, 200),
            instance(2, ProcStatus::Online, 201),
            instance(3, ProcStatus::Online, 202),
        ]]);
        assert!(
            wait(
                &daemon,
                "web",
                Verify::Probed,
                &before,
                Duration::from_secs(5)
            )
            .await
            .unwrap(),
            "three rows replaced by three new pids is a turnover"
        );
    }

    /// fails if `Alive` gives a replacement a fixed twenty seconds to reach
    /// `Online` whatever the app declared.
    ///
    /// A probeless app is marked `Online` by shep's heuristic path when its
    /// `listen_timeout` elapses, and the budget is built from that field.
    /// The post-dwell deadline was a second `DWELL` instead, so any app with
    /// `listen_timeout` past twenty seconds was rolled back healthy. Here
    /// the fake reaches `Online` at the thirty-first poll of a sixty-second
    /// budget, which the old deadline never saw.
    #[tokio::test(start_paused = true)]
    async fn alive_waits_for_online_as_long_as_the_budget_allows() {
        // The turnover lands at once; then `Starting` for well past two
        // dwells worth of polls; then `Online`.
        let mut sequence = vec![vec![instance(2, ProcStatus::Starting, 13002)]; 2];
        // The dwell's own polls: 10s of DWELL at 100ms is 100 polls, and the
        // old cap was another 100. Stay `Starting` for 250 polls.
        sequence.extend(std::iter::repeat_n(
            vec![instance(2, ProcStatus::Starting, 13002)],
            250,
        ));
        sequence.push(vec![instance(2, ProcStatus::Online, 13002)]);
        let daemon = Listings::new(sequence);

        assert!(
            wait(
                &daemon,
                "web",
                Verify::Alive,
                &serving(12835),
                Duration::from_secs(60)
            )
            .await
            .unwrap(),
            "a replacement that reaches Online inside the budget is healthy"
        );
    }

    /// fails if a budget past [`MAX_WAIT`] reaches a clock. Added to a
    /// `tokio::time::Instant`, `Duration::MAX` panics, and a release's
    /// Flockfile can ask for any `listen_timeout` up to `u64::MAX`
    /// milliseconds.
    #[test]
    fn a_budget_is_held_under_the_ceiling() {
        assert_eq!(bounded(Duration::MAX), MAX_WAIT);
        assert_eq!(bounded(Duration::from_secs(5)), Duration::from_secs(5));
        // And the ceiling itself is something a clock can hold.
        let _ = Instant::now() + MAX_WAIT;
    }
}
