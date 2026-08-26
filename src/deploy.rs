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

use std::path::Path;
use std::time::Duration;

use crate::daemon::Daemon;
use crate::error::Error;
use crate::paths::Tree;
use crate::state::{State, Verify, Watch};
use crate::{build, flockfile, git, shared, swap, verify};

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

/// How long a freshly reloaded sheep has to come up.
///
/// Two windows, because the two [`Verify`] modes spend them differently.
/// `Probed` returns the moment the sheep reports `Online`, so its window is
/// only a give-up point and can afford to be generous - an app that
/// compiles its client at startup takes a while, and giving up on it early
/// would roll back a release that was about to be fine. `Alive` sleeps the
/// whole window every single time before it looks, so the same number there
/// would add a minute and a half to every healthy deploy.
///
/// Neither is configurable yet. The `[dog.<name>]` section that would carry
/// them is read by [`crate::daemon::Daemon::dog_config`], which belongs to
/// the poll loop and is not built.
const fn grace(mode: Verify) -> Duration {
    match mode {
        Verify::Probed => Duration::from_secs(90),
        Verify::Alive => Duration::from_secs(10),
    }
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
/// record - and then the original error is returned, because that is the
/// one explaining why a rollback was wanted at all. A rollback that itself
/// fails replaces it with [`Error::Rollback`], which carries both halves:
/// discarding either leaves a machine an operator cannot diagnose. That
/// wrapper is also where [`Error::Unverified`] surfaces, for a release that
/// does not come up with no previous release to return to - a target's very
/// first deploy.
pub async fn deploy<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
) -> Result<Outcome, Error> {
    let sheep = tree.sheep();
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
    shared::link_into(
        &release,
        &state.checkout,
        &shared::to_link(&state.checkout)?,
    )?;

    let app = flockfile::app_config(&release, sheep)?;
    let spec = flockfile::build_spec(&release)?;
    build::run(&release, &spec, app.user.as_deref()).await?;

    // Nothing above here has touched the running app. Everything below has.
    let previous = swap::resolve(&tree.current())?;
    if previous != started_at {
        return Err(Error::Raced {
            sheep: sheep.to_owned(),
            started: named(started_at.as_deref()),
            found: named(previous.as_deref()),
        });
    }
    swap::point_at(&tree.current(), &release)?;

    match settle(daemon, sheep, state.verify).await {
        Ok(true) => {
            state.deployed = Some(head.clone());
            state.write(&tree.state_file())?;
            Ok(Outcome::Deployed { sha: head })
        }
        Ok(false) => {
            let why = format!(
                "it did not come up within {}s of the reload",
                grace(state.verify).as_secs()
            );
            roll_back(daemon, tree, state, previous.as_deref(), &head, why).await
        }
        // The same rollback as above, not a lesser one. `settle` reloads
        // before it verifies, so an error here can arrive with the new
        // release already running: shep has spawned it, waited for it, and
        // drained the old instance, and only then did a `describe` fail.
        // Moving a symlink does not move a running process, so a swap
        // without a reload would leave the daemon on the new code while
        // `current` and `deploy.toml` both name the old one.
        Err(err) => {
            roll_back(
                daemon,
                tree,
                state,
                previous.as_deref(),
                &head,
                err.to_string(),
            )
            .await?;
            Err(err)
        }
    }
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
                "the fetch succeeded but the remote has no branch named {branch:?} - it was \\
                 deleted or renamed upstream, or never existed - so there is nothing to deploy: \\
                 {stderr}"
            ),
        }),
        Err(other) => Err(other),
    }
}

/// Reloads `sheep` and waits for the verdict its `mode` asks for.
async fn settle<D: Daemon>(daemon: &D, sheep: &str, mode: Verify) -> Result<bool, Error> {
    daemon.reload(sheep).await?;
    verify::wait(daemon, sheep, mode, grace(mode)).await
}

/// Puts `previous` back after `attempted` was rejected, and reports what
/// the target is left on.
///
/// Every failure this can meet is wrapped in [`Error::Rollback`] alongside
/// `why`, at this one place, so a caller never has to choose between the
/// failure that made a rollback necessary and the failure of the rollback
/// itself. Both are what an operator needs.
///
/// # Errors
/// [`Error::Rollback`] wrapping [`Error::Unverified`] if there is nothing
/// to roll back to - `current` never pointed anywhere, or it already
/// pointed at the release that just failed, which between them mean this
/// was the target's first deploy. [`Error::Rollback`] wrapping whatever
/// [`restore`] returned otherwise.
async fn roll_back<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    previous: Option<&Path>,
    attempted: &str,
    why: String,
) -> Result<Outcome, Error> {
    let attempted_release = tree.release(attempted);
    let failed = |source: Error| Error::Rollback {
        why: why.clone(),
        source: Box::new(source),
    };

    let Some(previous) = previous.filter(|path| *path != attempted_release) else {
        return Err(failed(Error::Unverified {
            sheep: tree.sheep().to_owned(),
            sha: attempted.to_owned(),
        }));
    };

    let to = sha_of(previous);
    restore(daemon, tree, state, previous, &to)
        .await
        .map_err(failed)?;

    Ok(Outcome::RolledBack { to, why })
}

/// Points `current` back at `previous`, reloads onto it, and corrects the
/// record to match.
///
/// The three steps are in that order and none of them is optional.
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
/// [`Error::Io`] if `current` cannot be repointed or `deploy.toml` cannot
/// be written, or whatever [`Daemon::reload`] returns.
async fn restore<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    previous: &Path,
    to: &str,
) -> Result<(), Error> {
    swap::point_at(&tree.current(), previous)?;
    daemon.reload(tree.sheep()).await?;

    if state.deployed.as_deref() != Some(to) {
        state.deployed = Some(to.to_owned());
        state.write(&tree.state_file())?;
    }

    Ok(())
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

    use std::cell::Cell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use shep_client::shep_core::config::AppConfig;
    use shep_client::shep_core::protocol::ProcessInfo;
    use shep_client::shep_core::status::ProcStatus;
    use tempfile::TempDir;

    use crate::swap;

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
        /// Held only to keep the deploy tree alive for the test's duration.
        _home: TempDir,
        origin: TempDir,
        tree: Tree,
        state: State,
    }

    /// An origin repo with a Flockfile declaring one app named `web`, a
    /// bare clone fetched from it, and `current` pointing at a release
    /// built from its first commit.
    fn fixture_with_previous_release() -> Fixture {
        let home = tempfile::tempdir().expect("tempdir");
        let origin = tempfile::tempdir().expect("tempdir");

        run(origin.path(), &["init", "-q", "-b", "main"]);
        run(origin.path(), &["config", "user.email", "test@example.com"]);
        run(origin.path(), &["config", "user.name", "test"]);
        fs::write(
            origin.path().join("Flockfile.toml"),
            "[[app]]\nname = 'web'\nscript = './run.sh'\n",
        )
        .expect("write Flockfile");
        run(origin.path(), &["add", "."]);
        run(origin.path(), &["commit", "-q", "-m", "first"]);

        let tree = Tree::for_sheep(home.path(), "web");
        fs::create_dir_all(tree.git()).expect("create git dir");
        run(&tree.git(), &["init", "-q", "--bare"]);

        let remote = origin.path().to_str().expect("utf-8 path").to_owned();
        crate::git::fetch(&tree.git(), &remote).expect("fetch");
        let first = crate::git::remote_head(&tree.git(), "main").expect("head");
        crate::git::worktree_add(&tree.git(), &tree.release(&first), &first).expect("worktree");
        swap::point_at(&tree.current(), &tree.release(&first)).expect("first swap");

        let state = State {
            remote,
            branch: "main".to_owned(),
            deployed: Some(first),
            verify: crate::state::Verify::Probed,
            watch: crate::state::Watch::Auto,
            origin_cwd: None,
            origin_script: None,
            checkout: origin.path().to_owned(),
        };

        Fixture {
            _home: home,
            origin,
            tree,
            state,
        }
    }

    /// Adds a commit to the fixture's origin and returns its sha.
    fn commit_on_origin(fixture: &Fixture, name: &str) -> String {
        fs::write(fixture.origin.path().join(name), "x").expect("write");
        run(fixture.origin.path(), &["add", "."]);
        run(fixture.origin.path(), &["commit", "-q", "-m", name]);
        head_of(fixture.origin.path())
    }

    /// A [`Daemon`] that counts its reloads, for the tests that care how
    /// many times a deploy reloaded rather than only where `current` ended
    /// up. A rollback that swaps the symlink and never reloads leaves the
    /// daemon running the rejected code, and no filesystem assertion can
    /// see that.
    struct Counting {
        /// Whether `describe` fails outright instead of reporting a sheep
        /// that has not come up. The two are different paths through
        /// `deploy`: one is a verdict, the other is an error arriving after
        /// the reload has already drained the old instance.
        describe_fails: bool,
        /// Created just before answering the first `describe`, standing in
        /// for another process leaving a stale `current.tmp` behind at the
        /// worst possible moment - after the swap, before the rollback.
        plant: Option<PathBuf>,
        reloads: Cell<u32>,
    }

    impl Counting {
        /// Answers every `describe` with a sheep still `Starting`.
        fn never_ready() -> Self {
            Self {
                describe_fails: false,
                plant: None,
                reloads: Cell::new(0),
            }
        }

        /// Fails every `describe`, the transient error `verify::wait`'s own
        /// doc anticipates.
        fn describe_fails() -> Self {
            Self {
                describe_fails: true,
                ..Self::never_ready()
            }
        }

        /// As [`Self::never_ready`], but leaves a stale `current.tmp` at
        /// `plant` on the way past.
        fn planting(plant: PathBuf) -> Self {
            Self {
                plant: Some(plant),
                ..Self::never_ready()
            }
        }

        fn reload_count(&self) -> u32 {
            self.reloads.get()
        }
    }

    impl Daemon for Counting {
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
            if self.describe_fails {
                return Err(Error::Protocol("the shepherd stopped answering".to_owned()));
            }
            Ok(vec![
                ProcessInfo::builder(0, sheep, ProcStatus::Starting).build(),
            ])
        }
        async fn start(&self, _apps: Vec<AppConfig>) -> Result<(), Error> {
            unimplemented!()
        }
        async fn reload(&self, _sheep: &str) -> Result<(), Error> {
            self.reloads.set(self.reloads.get() + 1);
            Ok(())
        }
        async fn restart(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
    }

    /// A [`Daemon`] whose sheep never reaches `Online`.
    struct NeverReady;

    impl Daemon for NeverReady {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            unimplemented!()
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            unimplemented!()
        }
        async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            Ok(vec![
                ProcessInfo::builder(0, sheep, ProcStatus::Starting).build(),
            ])
        }
        async fn start(&self, _apps: Vec<AppConfig>) -> Result<(), Error> {
            unimplemented!()
        }
        async fn reload(&self, _sheep: &str) -> Result<(), Error> {
            Ok(())
        }
        async fn restart(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
    }

    /// A [`Daemon`] whose sheep is always `Online`.
    struct Ready;

    impl Daemon for Ready {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            unimplemented!()
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            unimplemented!()
        }
        async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            Ok(vec![
                ProcessInfo::builder(0, sheep, ProcStatus::Online).build(),
            ])
        }
        async fn start(&self, _apps: Vec<AppConfig>) -> Result<(), Error> {
            unimplemented!()
        }
        async fn reload(&self, _sheep: &str) -> Result<(), Error> {
            Ok(())
        }
        async fn restart(&self, _sheep: &str) -> Result<(), Error> {
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

        let outcome = deploy(&NeverReady, &fixture.tree, &mut fixture.state)
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
        let outcome = deploy(&Ready, &fixture.tree, &mut fixture.state)
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

        let outcome = deploy(&Ready, &fixture.tree, &mut fixture.state)
            .await
            .expect("completes");

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
            "[[app]]\nname = 'web'\nscript = './run.sh'\n\n[build]\ncommand = 'exit 3'\n",
        )
        .expect("write Flockfile");
        commit_on_origin(&fixture, "second.txt");

        let err = deploy(&Ready, &fixture.tree, &mut fixture.state)
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

        let err = deploy(&Ready, &fixture.tree, &mut fixture.state)
            .await
            .expect_err("no such branch");

        let shown = err.to_string();
        assert!(shown.contains("gone"), "{shown}");
        assert!(shown.contains("no branch named"), "{shown}");
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
        let daemon = Counting::never_ready();

        let outcome = deploy(&daemon, &fixture.tree, &mut fixture.state)
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
    /// than a failed verification does. `settle` reloads before it
    /// verifies, so a `describe` that fails transiently - which
    /// `verify::wait`'s own doc anticipates - arrives with shep already
    /// running the new release and the old instance already drained. A swap
    /// without a reload there leaves the daemon on the new code while both
    /// signals an operator would check name the old one.
    #[tokio::test]
    async fn a_verify_error_after_the_reload_still_reloads_the_rollback() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Counting::describe_fails();

        let err = deploy(&daemon, &fixture.tree, &mut fixture.state)
            .await
            .expect_err("describe fails");

        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
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

    /// fails if a rollback repairs the filesystem and leaves the record
    /// wrong. A deploy killed between its reload and its `State::write`
    /// leaves `deploy.toml` lagging `current`; the next rollback is the
    /// moment that can be noticed, and a rollback that never writes leaves
    /// the record wrong indefinitely.
    #[tokio::test(start_paused = true)]
    async fn a_rollback_corrects_a_stale_deployed_record() {
        let mut fixture = fixture_with_previous_release();
        let live = fixture.state.deployed.clone().expect("a previous release");
        // What an interrupted deploy leaves behind: `current` names a
        // release the record has never heard of.
        fixture.state.deployed = None;
        commit_on_origin(&fixture, "second.txt");

        deploy(&Counting::never_ready(), &fixture.tree, &mut fixture.state)
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
            &Counting::planting(stale),
            &fixture.tree,
            &mut fixture.state,
        )
        .await
        .expect_err("the swap back collides with the stale tmp link");

        assert!(matches!(err, Error::Rollback { .. }), "{err:?}");
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
            "[[app]]\nname = 'web'\nscript = './run.sh'\n\n[build]\ncommand = 'ln -sfn \"$PWD\" \
             ../../current'\n",
        )
        .expect("write Flockfile");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Counting::never_ready();

        let err = deploy(&daemon, &fixture.tree, &mut fixture.state)
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
}
