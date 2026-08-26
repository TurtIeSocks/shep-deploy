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
/// old release is still serving. After the swap, an error from the reload
/// or from verification swaps `current` back first, on a best-effort basis,
/// and then returns the original error rather than whatever the swap back
/// said: the original is the one that explains why a rollback was wanted at
/// all. [`Error::Unverified`] if the release does not come up and there is
/// no previous release to return to, which can only be a target's very
/// first deploy.
pub async fn deploy<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
) -> Result<Outcome, Error> {
    let sheep = tree.sheep();

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
            roll_back(daemon, tree, previous.as_deref(), &head, why).await
        }
        Err(err) => {
            if let Some(previous) = previous {
                let _ = swap::point_at(&tree.current(), &previous);
            }
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

/// Points `current` back at `previous` and reloads, after a release failed
/// to come up.
///
/// The reload is not best-effort: a swap back that is never reloaded leaves
/// the old code on disk and the new, unhealthy instance still running, and
/// an operator told the rollback succeeded.
///
/// # Errors
/// [`Error::Unverified`] if there is nothing to roll back to - `current`
/// never pointed anywhere, or it already pointed at the release that just
/// failed, which between them mean this was the target's first deploy.
/// Otherwise whatever the swap or the reload returns.
async fn roll_back<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    previous: Option<&Path>,
    attempted: &str,
    why: String,
) -> Result<Outcome, Error> {
    let sheep = tree.sheep();
    let attempted_release = tree.release(attempted);

    let Some(previous) = previous.filter(|path| *path != attempted_release) else {
        return Err(Error::Unverified {
            sheep: sheep.to_owned(),
            sha: attempted.to_owned(),
        });
    };

    swap::point_at(&tree.current(), previous)?;
    daemon.reload(sheep).await?;

    Ok(Outcome::RolledBack {
        to: sha_of(previous),
        why,
    })
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

    use std::fs;
    use std::path::Path;
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
