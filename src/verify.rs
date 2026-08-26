//! Deciding whether a freshly started sheep came up healthy.
//!
//! shep already reports [`ProcStatus::Starting`] ("spawned, not yet ready")
//! and [`ProcStatus::Online`] ("running and, if configured, ready") for every
//! instance it supervises. [`wait`] does nothing more than read that verdict
//! back through [`Daemon::describe`] until either it holds or the deploy's
//! grace period runs out.

use std::time::Duration;

use shep_client::shep_core::protocol::ProcessInfo;
use shep_client::shep_core::status::ProcStatus;
use tokio::time::{Instant, sleep};

use crate::daemon::Daemon;
use crate::error::Error;
use crate::state::Verify;

/// Watch `sheep` for up to `timeout`, judging health the way `mode` says to.
///
/// `Probed` polls `describe` every 100ms (and once immediately) and returns
/// as soon as any instance is [`ProcStatus::Online`] - a reload leaves the
/// old, draining instance behind for a while, so demanding every instance be
/// `Online` would fail a perfectly healthy deploy. `Alive` has a different
/// shape on purpose: it sleeps the whole window once and then checks a
/// single time whether at least one instance is `Starting` or `Online`. A
/// `WaitingRestart` instance in particular means the process already died
/// and is backed off waiting to be restarted, which is exactly the failure a
/// deploy is trying to catch, so it counts as a fail. An empty `describe`
/// result is a fail in both modes: a sheep the shepherd cannot find has not
/// been verified.
///
/// # Errors
/// Whatever [`Daemon::describe`] returns - a `Probed` wait can surface a
/// transient error mid-poll rather than only at the deadline.
pub async fn wait<D: Daemon>(
    daemon: &D,
    sheep: &str,
    mode: Verify,
    timeout: Duration,
) -> Result<bool, Error> {
    match mode {
        Verify::Probed => {
            let deadline = Instant::now() + timeout;
            loop {
                let flock = daemon.describe(sheep).await?;
                if flock.iter().any(|info| info.status == ProcStatus::Online) {
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
            Ok(flock.iter().any(is_alive))
        }
    }
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

    fn instance(status: ProcStatus) -> ProcessInfo {
        ProcessInfo::builder(0, "web", status).build()
    }

    /// A [`Daemon`] whose `describe` walks through a fixed sequence of
    /// statuses, one per call, and repeats the last one once the sequence is
    /// exhausted rather than panicking or returning an empty `Vec`. `Probed`
    /// calls `describe` many times inside a single wait, and these tests
    /// only need to name the interesting statuses, not one per poll.
    struct Statuses {
        sequence: Vec<ProcStatus>,
        next: Cell<usize>,
    }

    impl Statuses {
        fn new(sequence: Vec<ProcStatus>) -> Self {
            Self {
                sequence,
                next: Cell::new(0),
            }
        }
    }

    impl Daemon for Statuses {
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
            Ok(vec![instance(self.sequence[index])])
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

    /// fails if Starting is mistaken for success. ProcStatus::Starting means
    /// "spawned, not yet ready" and Online means "running and (if configured)
    /// ready", so Online is the ONLY status that means the probe passed.
    #[tokio::test]
    async fn starting_is_not_success() {
        let daemon = Statuses::new(vec![ProcStatus::Starting, ProcStatus::Starting]);
        let ok = wait(&daemon, "web", Verify::Probed, Duration::from_millis(50))
            .await
            .unwrap();
        assert!(!ok);
    }

    /// fails if reaching Online is not treated as success.
    #[tokio::test]
    async fn reaching_online_is_success() {
        let daemon = Statuses::new(vec![ProcStatus::Starting, ProcStatus::Online]);
        assert!(
            wait(&daemon, "web", Verify::Probed, Duration::from_secs(5))
                .await
                .unwrap()
        );
    }

    /// fails if Verify::Alive starts demanding a probe. Alive is the
    /// deliberate, visible downgrade for a sheep with no probe: still
    /// running after the window is enough.
    #[tokio::test]
    async fn alive_accepts_a_still_running_process() {
        let daemon = Statuses::new(vec![ProcStatus::Starting, ProcStatus::Starting]);
        assert!(
            wait(&daemon, "web", Verify::Alive, Duration::from_millis(50))
                .await
                .unwrap()
        );
    }

    /// fails if a sheep the shepherd cannot find (an empty `describe`
    /// result) is ever treated as verified. Both modes must fail closed
    /// here: there is nothing to have come up healthy.
    #[tokio::test]
    async fn an_empty_describe_is_failure_in_both_modes() {
        let empty = Statuses::new(Vec::new());
        assert!(
            !wait(&empty, "web", Verify::Probed, Duration::from_millis(50))
                .await
                .unwrap()
        );
        let empty = Statuses::new(Vec::new());
        assert!(
            !wait(&empty, "web", Verify::Alive, Duration::from_millis(50))
                .await
                .unwrap()
        );
    }

    /// fails if the fixture stops repeating its last status once its
    /// sequence is exhausted, whether by panicking or by returning an empty
    /// `Vec`. `Probed` polls every 100ms, so a 350ms window against a
    /// two-element sequence outlives it several times over; every one of
    /// those extra polls must keep reading the last status rather than
    /// running off the end.
    #[tokio::test]
    async fn the_fixture_repeats_its_last_status_past_exhaustion() {
        let daemon = Statuses::new(vec![ProcStatus::Starting, ProcStatus::Starting]);
        let ok = wait(&daemon, "web", Verify::Probed, Duration::from_millis(350))
            .await
            .unwrap();
        assert!(!ok);
    }
}
