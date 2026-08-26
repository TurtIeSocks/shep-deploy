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
    pids: BTreeSet<u32>,
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
        Ok(Self {
            pids: flock.iter().filter_map(|info| info.pid).collect(),
        })
    }

    /// Whether `info` is running under a pid this generation never had.
    ///
    /// An instance with no pid at all is never new. That is the honest
    /// reading rather than a defensive one: a `WaitingRestart` or errored
    /// instance has no process, and a deploy verified against one would be
    /// verified against nothing.
    fn is_new(&self, info: &ProcessInfo) -> bool {
        info.pid.is_some_and(|pid| !self.pids.contains(&pid))
    }

    /// Whether `flock` is entirely made of instances this generation never
    /// had, in a state `accept` is happy with.
    ///
    /// Empty is never a turnover: a sheep the shepherd cannot find has not
    /// been verified.
    fn has_turned_over(&self, flock: &[ProcessInfo], accept: fn(&ProcessInfo) -> bool) -> bool {
        !flock.is_empty() && flock.iter().all(|info| self.is_new(info) && accept(info))
    }
}

/// Watch `sheep` for up to `timeout`, judging health the way `mode` says to,
/// against the generation that was serving before the reload.
///
/// `Probed` polls `describe` every 100ms (and once immediately) and returns
/// as soon as the flock has turned over into instances that are all
/// [`ProcStatus::Online`]. It can only mean anything for a sheep with a
/// readiness probe configured, since that is what makes shep withhold
/// `Online` from a process that is running but not ready; `deploy` refuses a
/// `Probed` target with no probe before it touches anything.
///
/// `Alive` has a different shape on purpose: it sleeps the whole window once
/// and then checks a single time. It still demands a turnover - a deploy
/// that reloaded nothing is not verified in either mode - but it accepts
/// `Starting` as well as `Online`, because a sheep with no probe is exactly
/// the case `Alive` exists for and there is nothing to wait for beyond the
/// process still being there. `WaitingRestart` in particular is a fail: it
/// means the process already died and is backed off waiting to be restarted,
/// which is the failure a deploy is trying to catch.
///
/// # Errors
/// Whatever [`Daemon::describe`] returns - a `Probed` wait can surface a
/// transient error mid-poll rather than only at the deadline.
pub async fn wait<D: Daemon>(
    daemon: &D,
    sheep: &str,
    mode: Verify,
    before: &Generation,
    timeout: Duration,
) -> Result<bool, Error> {
    match mode {
        Verify::Probed => {
            let deadline = Instant::now() + timeout;
            loop {
                let flock = daemon.describe(sheep).await?;
                if before.has_turned_over(&flock, is_online) {
                    return Ok(true);
                }
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                sleep(Duration::from_millis(100)).await;
            }
        }
        Verify::Alive => {
            sleep(timeout).await;
            let flock = daemon.describe(sheep).await?;
            Ok(before.has_turned_over(&flock, is_alive))
        }
    }
}

/// Whether one instance has passed its readiness probe, for [`Verify::Probed`].
fn is_online(info: &ProcessInfo) -> bool {
    info.status == ProcStatus::Online
}

/// Whether one instance still counts as "running" for [`Verify::Alive`].
fn is_alive(info: &ProcessInfo) -> bool {
    matches!(info.status, ProcStatus::Starting | ProcStatus::Online)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use shep_client::shep_core::config::AppConfig;

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

    impl Daemon for Listings {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            unimplemented!()
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            unimplemented!()
        }
        async fn describe(&self, _sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            let Some(last) = self.sequence.len().checked_sub(1) else {
                return Ok(Vec::new());
            };
            let index = self.next.get().min(last);
            self.next.set((index + 1).min(last));
            Ok(self.sequence[index].clone())
        }
        async fn start(&self, _apps: Vec<AppConfig>) -> Result<(), Error> {
            unimplemented!()
        }
        async fn reload(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
        async fn restart(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
    }

    /// fails if the process that was already serving passes for the one the
    /// deploy just installed. This is the shape a real shepherd settles into
    /// when its own `AwaitReady` fails: it keeps the old instance, so the
    /// listing is one `Online` entry under the old pid, forever. A check
    /// that asks only about status reports that deploy healthy and every
    /// rollback in this crate becomes unreachable.
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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

    /// fails if `Verify::Alive` starts demanding a probe. Alive is the
    /// deliberate, visible downgrade for a sheep with no probe: a new
    /// process, still running after the window, is enough.
    #[tokio::test]
    async fn alive_accepts_a_new_process_that_is_still_running() {
        let daemon = Listings::new(vec![vec![instance(2, ProcStatus::Starting, 13002)]]);
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
    #[tokio::test]
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

    /// fails if an instance with no pid at all is treated as a new one.
    /// `WaitingRestart` covers the status side of the same case; this
    /// covers the pid side, which is what `Generation::is_new` actually
    /// asks. A deploy verified against an instance with no process has been
    /// verified against nothing.
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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
                pids: [12835, 12836].into_iter().collect()
            }
        );
    }
}
