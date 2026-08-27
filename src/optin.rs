//! Opt-in, part one: taking a git checkout shep already runs and turning it
//! into a deploy target, up to and including its first release.
//!
//! **Everything here happens before anything is registered or reloaded.** A
//! failure anywhere in [`prepare`] leaves the sheep running exactly as it
//! was, from the operator's own checkout, with nothing about the flock
//! changed. What it leaves behind on a failure is a directory, and the
//! operator's fix is to run the command again once they have fixed whatever
//! it named.
//!
//! # Why this ordering is load-bearing, not tidy
//!
//! `origin_cwd`, `origin_script` and `previous_config` are all read from the
//! shepherd's muster roll, and the roll is poisoned the instant a `Start` is
//! accepted: `FlockRegistry` is name-keyed, so from that moment the roll's
//! entry for this sheep is the DOG's own config, not the operator's.
//! [`prepare`] captures all three before it ever sends one, and the refusal
//! at its own top is what stops a re-run after an abandoned cutover from
//! capturing the deploy tree as the thing to restore to.
//!
//! # Why `git init --bare` and a fetch, rather than a clone
//!
//! [`crate::git::fetch`] is anonymous by URL with a mirror refspec and
//! `--prune`, and needs no configured remote - which is why `git_dir` never
//! gets a `git remote add`. An empty bare repository plus that same fetch
//! reaches exactly the state a clone would, through the one code path the
//! poll loop already uses every 30 seconds, rather than through a second
//! one that would only ever run once. The cost is real and worth naming:
//! opt-in downloads the repository from the remote rather than hardlinking
//! from the operator's checkout next door. Cloning locally would be
//! faster, and it was rejected because it entangles the dog's object store
//! with a checkout the design says the dog only ever reads, and because a
//! first-run-only code path is the one nothing exercises.

use std::path::{Path, PathBuf};

use shep_client::shep_core::config::AppConfig;

use crate::build;
use crate::daemon::Daemon;
use crate::error::Error;
use crate::flockfile;
use crate::git;
use crate::paths::Tree;
use crate::roll;
use crate::shared;
use crate::state::{State, Verify, Watch};
use crate::swap;

/// Everything opt-in built, ready for a cutover to register and swap into
/// place.
///
/// `Clone` is needed by Task 8's own tests, which assert on what the
/// cutover did with a value they still hold afterwards.
// The whole struct is only ever built by this module's own tests until
// Task 8's cutover consumes it; `tree`, `state` and `sha` are read there
// already, `app` and `previous_config` are not until Task 8 reads them.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The deploy tree this release was built into.
    #[cfg_attr(not(test), expect(dead_code))]
    pub tree: Tree,
    /// The tree's own record, with `current` already pointed at `sha`.
    #[cfg_attr(not(test), expect(dead_code))]
    pub state: State,
    /// The sha `tree.current()` resolves to.
    #[cfg_attr(not(test), expect(dead_code))]
    pub sha: String,
    /// The release's app definition, which the cutover registers after
    /// overriding `cwd`.
    // Read by Task 8's cutover, which registers this after overriding
    // `cwd`; this task's own tests only assert on `state`, `sha` and
    // `tree`.
    #[expect(dead_code)]
    pub app: AppConfig,
    /// The app definition as the shepherd has it RIGHT NOW, carried so an
    /// abandoned cutover can put the shepherd's persisted record back.
    /// See Task 8's `undo_start`: an accepted `Start` records against the
    /// sheep's name, and deleting the instance does not undo that.
    #[expect(dead_code)]
    pub previous_config: AppConfig,
}

/// Builds `sheep`'s deploy tree and its first release, from the checkout
/// shep currently has it registered against.
///
/// Refuses outright, before touching anything, if `sheep` is already a
/// target - see the module doc for why a re-run has to be refused rather
/// than treated as a retry. Everything after that refusal either succeeds
/// all the way through to a release under `current`, or fails partway and
/// leaves a directory behind with no `deploy.toml` in it, which is not a
/// target and does not trip that same refusal on a second attempt.
///
/// # Errors
/// [`Error::Config`] if `sheep` is already a deploy target, if the
/// shepherd has no sheep by that name registered, or if it records no
/// working directory for it. Otherwise, whatever [`roll::registered`],
/// [`git::remote_url`], [`git::current_branch`], [`git::init_bare`],
/// [`git::fetch`], [`git::remote_head`], [`git::worktree_add`],
/// [`shared::link_cache`], [`shared::link_into`], [`flockfile::app_config`],
/// [`flockfile::build_spec`], [`build::run`] or [`swap::point_at`] return.
// The cutover that calls this is Task 8; until that lands, only this
// module's own tests do.
#[cfg_attr(not(test), expect(dead_code))]
pub async fn prepare<D: Daemon>(
    daemon: &D,
    shep_home: &Path,
    sheep: &str,
) -> Result<Prepared, Error> {
    let tree = Tree::for_sheep(shep_home, sheep);
    if tree.state_file().is_file() {
        return Err(Error::Config(format!(
            "{sheep} is already a deploy target: its tree is at {}. Deploy it with \
             `shep deploy {sheep}`, or change how it is watched with \
             `shep deploy {sheep} --watch auto|manual`.",
            tree.releases().display()
        )));
    }

    // Cloned rather than borrowed, and named `previous_config` rather than
    // `app`, because the release's own definition shadows the name below and
    // these two must never be confused: this one is the operator's, and it
    // is the only copy of it that exists once Task 8 sends its Start.
    let registered = roll::registered(daemon).await?;
    let previous_config = registered.get(sheep).cloned().ok_or_else(|| {
        Error::Config(format!(
            "the shepherd has no sheep named {sheep:?} registered, so there is nothing to take \
             over: `shep-deploy survey` lists every sheep and where it stands"
        ))
    })?;
    let checkout = PathBuf::from(previous_config.cwd.as_deref().ok_or_else(|| {
        Error::Config(format!(
            "shep records no working directory for {sheep}, so there is no checkout to deploy \
             from"
        ))
    })?);

    // Both of these refuse rather than guess: a checkout with no `origin`
    // gets git's own complaint, and a detached HEAD is refused by name,
    // because there is no branch to track and a target created that way
    // would silently never deploy again.
    let remote = git::remote_url(&checkout)?;
    let branch = git::current_branch(&checkout)?;

    let state = State {
        remote,
        branch,
        deployed: None,
        verify: Verify::default(),
        watch: Watch::default(),
        // The two fields the whole restore depends on, captured once, here,
        // and never touched again.
        origin_cwd: Some(checkout.clone()),
        origin_script: Some(previous_config.script.clone()),
        checkout,
    };

    std::fs::create_dir_all(tree.releases()).map_err(|source| Error::Io {
        path: tree.releases(),
        source,
    })?;
    git::init_bare(&tree.git())?;
    git::fetch(&tree.git(), &state.remote)?;
    let sha = git::remote_head(&tree.git(), &state.branch)?;

    let release = tree.release(&sha);
    git::worktree_add(&tree.git(), &release, &sha)?;
    shared::link_cache(&release, &tree.cache_target())?;
    shared::link_into(
        &release,
        &state.checkout,
        &shared::to_link(&state.checkout)?,
    )?;

    let app = flockfile::app_config(&release, sheep)?;
    let spec = flockfile::build_spec(&release)?;
    build::run(sheep, &release, &spec, app.user.as_deref()).await?;

    // `current` now points through a real release carrying the operator's
    // shared files - the whole deliverable of part one. `state.write` comes
    // AFTER the swap, not before: a run that dies between them leaves
    // `current` set but no `deploy.toml`, so `tree.state_file().is_file()`
    // at the top of this function still reads false and the operator can
    // simply run the command again, rather than tripping the "already a
    // deploy target" refusal on a tree that never finished becoming one.
    // Do not move this above the swap: a process killed between the two
    // statements is not something a single-threaded test can arrange, so
    // this ordering rests on this comment and on review, the same way
    // `swap::point_at`'s own atomicity claim rests on its doc rather than
    // on a test.
    swap::point_at(&tree.current(), &release)?;
    state.write(&tree.state_file())?;

    Ok(Prepared {
        tree,
        state,
        sha,
        app,
        previous_config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A [`Daemon`] whose `save_roll` writes a roll JSON naming each
    /// `(sheep, cwd)` pair, with `script = "./run.sh"`, into a fresh
    /// tempdir, and answers with the path it wrote. Every other method is
    /// `unimplemented!()`, since `prepare` never calls them.
    struct RollOf<'a>(&'a [(&'a str, &'a Path)]);

    impl Daemon for RollOf<'_> {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            unimplemented!()
        }
        async fn list_flock(
            &self,
        ) -> Result<Vec<shep_client::shep_core::protocol::ProcessInfo>, Error> {
            unimplemented!()
        }
        async fn describe(
            &self,
            _sheep: &str,
        ) -> Result<Vec<shep_client::shep_core::protocol::ProcessInfo>, Error> {
            unimplemented!()
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
        async fn save_roll(&self) -> Result<PathBuf, Error> {
            let dir = tempfile::tempdir().expect("tempdir");
            let apps: Vec<String> = self
                .0
                .iter()
                .map(|(name, cwd)| {
                    format!(
                        "{{\"app\":{{\"name\":{name:?},\"script\":\"./run.sh\",\"cwd\":{:?}}}}}",
                        cwd.to_str().expect("utf-8 cwd")
                    )
                })
                .collect();
            let path = dir.keep().join("flock.json");
            std::fs::write(&path, format!("{{\"apps\":[{}]}}", apps.join(",")))
                .expect("write roll");
            Ok(path)
        }
    }

    /// Runs a git subcommand for fixture setup, panicking if it fails.
    fn git_in(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// `dir`'s current `HEAD` sha.
    fn head_sha(dir: &Path) -> String {
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

    /// A tempdir that is a git checkout, on branch `main`, with an `origin`
    /// remote pointing at itself and one commit declaring an app named
    /// `bpm` whose script is `./run.sh`.
    ///
    /// `origin` points at the checkout's own path rather than a second
    /// fixture repository: `prepare` fetches from whatever `git::remote_url`
    /// reads, and a checkout is a perfectly fetchable remote for its own
    /// history.
    fn checkout_with_commit() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git_in(dir.path(), &["init", "-q", "-b", "main"]);
        git_in(dir.path(), &["config", "user.email", "test@example.com"]);
        git_in(dir.path(), &["config", "user.name", "test"]);
        git_in(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                dir.path().to_str().expect("utf-8 tempdir path"),
            ],
        );
        std::fs::write(
            dir.path().join("Flockfile.toml"),
            "[[app]]\nname = \"bpm\"\nscript = \"./run.sh\"\n",
        )
        .expect("write Flockfile");
        std::fs::write(dir.path().join("run.sh"), "#!/bin/sh\necho hi\n").expect("write run.sh");
        git_in(dir.path(), &["add", "."]);
        git_in(dir.path(), &["commit", "-q", "-m", "initial"]);
        dir
    }

    /// Writes `<home>/deploy/<sheep>/deploy.toml` through `State::write`,
    /// standing in for a sheep that is already a deploy target.
    fn write_target(home: &Path, sheep: &str, watch: Watch, sha: Option<&str>) {
        let tree = Tree::for_sheep(home, sheep);
        std::fs::create_dir_all(tree.state_file().parent().expect("has a parent"))
            .expect("create target dir");
        let state = State {
            remote: "https://example.com/x".to_owned(),
            branch: "main".to_owned(),
            deployed: sha.map(str::to_owned),
            verify: Verify::default(),
            watch,
            origin_cwd: None,
            origin_script: None,
            checkout: PathBuf::from("/srv/x"),
        };
        state.write(&tree.state_file()).expect("write state");
    }

    /// fails if a sheep that is already a target can be opted in again.
    /// The second run would `git init --bare` over a live tree and rebuild
    /// a release for an app whose sheep is already running from `current`,
    /// which is a way to break a working deployment with a command whose
    /// name sounds safe.
    #[tokio::test]
    async fn opting_in_twice_is_refused_by_name() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        write_target(home.path(), "bpm", Watch::Auto, Some("old"));
        let daemon = RollOf(&[("bpm", checkout.path())]);

        let err = prepare(&daemon, home.path(), "bpm")
            .await
            .expect_err("refuses");
        let shown = err.to_string();
        assert!(shown.contains("bpm"), "{shown}");
        assert!(shown.contains("already"), "{shown}");
    }

    /// fails if opt-in stops recording how the sheep ran BEFORE it took
    /// over. These two fields are the only record there is, and losing them
    /// means removing the dog leaves the app running from a path under
    /// $SHEP_HOME the operator has no reason to know about, which is the
    /// exact failure the restore exists to prevent.
    #[tokio::test]
    async fn the_pre_adoption_cwd_and_script_are_recorded() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        let daemon = RollOf(&[("bpm", checkout.path())]);

        let prepared = prepare(&daemon, home.path(), "bpm")
            .await
            .expect("prepares");

        assert_eq!(prepared.state.origin_cwd.as_deref(), Some(checkout.path()));
        assert_eq!(prepared.state.origin_script.as_deref(), Some("./run.sh"));
        assert_eq!(prepared.state.checkout, checkout.path());
    }

    /// fails if the branch stops coming from the operator's own checkout.
    /// That was the best idea in this design: `git checkout stable`
    /// retargets the deploy and nobody learns a new config key. A --branch
    /// flag would give one fact two sources of truth, which is why there
    /// is not one.
    #[tokio::test]
    async fn the_branch_comes_from_the_checkouts_own_head() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        git_in(checkout.path(), &["checkout", "-q", "-b", "stable"]);
        let daemon = RollOf(&[("bpm", checkout.path())]);

        let prepared = prepare(&daemon, home.path(), "bpm")
            .await
            .expect("prepares");
        assert_eq!(prepared.state.branch, "stable");
    }

    /// fails if `current` does not end up pointing at a real release
    /// holding the shared files. This is the whole deliverable of part one:
    /// a tree a cutover can point a sheep at.
    #[tokio::test]
    async fn current_ends_up_on_a_release_carrying_the_shared_files() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        std::fs::write(checkout.path().join(".gitignore"), "local.json\n").expect("write");
        std::fs::write(checkout.path().join("local.json"), "{}").expect("write");
        let daemon = RollOf(&[("bpm", checkout.path())]);

        let prepared = prepare(&daemon, home.path(), "bpm")
            .await
            .expect("prepares");

        let current = swap::resolve(&prepared.tree.current())
            .expect("reads")
            .expect("current is set");
        assert_eq!(current, prepared.tree.release(&prepared.sha));
        assert_eq!(
            std::fs::read_to_string(current.join("local.json")).expect("reads through the link"),
            "{}"
        );
    }

    /// fails if a checkout on a detached HEAD is accepted. There is no
    /// branch to track, so there is nothing to poll, and a target created
    /// this way would silently never deploy again.
    #[tokio::test]
    async fn a_detached_checkout_is_refused() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        let head = head_sha(checkout.path());
        git_in(checkout.path(), &["checkout", "-q", &head]);
        let daemon = RollOf(&[("bpm", checkout.path())]);

        let err = prepare(&daemon, home.path(), "bpm")
            .await
            .expect_err("refuses");
        assert!(err.to_string().contains("detached"), "{err}");
    }
}
