//! Opt-in: taking a git checkout shep already runs and turning it into a
//! deploy target, then cutting the sheep over to it.
//!
//! Two halves, and the line between them is the only thing in this crate
//! with no rollback. [`prepare`] builds the tree and the first release, and
//! [`cut_over`] registers the sheep against `current` and removes the
//! instances it replaced.
//!
//! **Nothing [`prepare`] does touches the flock.** A failure anywhere in it
//! leaves the sheep running exactly as it was, from the operator's own
//! checkout, with nothing about the flock changed. What it leaves behind on
//! a failure is a directory, and the operator's fix is to run the command
//! again once they have fixed whatever it named.
//!
//! **Everything [`cut_over`] does is past that line.** The moment the
//! shepherd accepts its `Start` a process has been spawned and a config has
//! been recorded against the sheep's name, and those are two separate things
//! to put back: see [`undo_start`].
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
use std::time::Duration;

use shep_client::shep_core::config::AppConfig;
use shep_client::shep_core::protocol::ProcessInfo;
use tokio::time::{Instant, sleep};

use crate::build;
use crate::daemon::Daemon;
use crate::deploy::RELOAD_DEADLINE_SLACK;
use crate::error::Error;
use crate::flockfile;
use crate::git;
use crate::paths::Tree;
use crate::roll;
use crate::shared;
use crate::state::{State, Verify, Watch};
use crate::swap;
use crate::verify::{DWELL, Generation, POLL, is_alive};

/// Everything opt-in built, ready for a cutover to register and swap into
/// place.
///
/// `Clone` is needed by the cutover's own tests, which assert on what it
/// did with a value they still hold afterwards.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The deploy tree this release was built into.
    pub tree: Tree,
    /// The tree's own record, with `current` already pointed at `sha`.
    pub state: State,
    /// The sha `tree.current()` resolves to.
    pub sha: String,
    /// The release's app definition, which [`cut_over`] registers after
    /// overriding `cwd`.
    pub app: AppConfig,
    /// The app definition as the shepherd has it RIGHT NOW, carried so an
    /// abandoned cutover can put the shepherd's persisted record back.
    /// See [`undo_start`]: an accepted `Start` records against the sheep's
    /// name, and deleting the instance does not undo that.
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
             over: `shep-deploy survey` lists every registered sheep and where it stands"
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

/// Registers the sheep against `current`, waits for the newcomer to start
/// and stay up, and deletes the instances it replaced.
///
/// # Errors
/// [`Error::CutOver`] if the newcomer never came up, in which case it has
/// been removed and the original is still serving, untouched. That error
/// also carries whether the shepherd's persisted record could be put back:
/// see [`undo_start`] for why deleting the newcomer is not enough on its
/// own, and why a failure there is something an operator cannot see in
/// `shep flock` and has to be told by name.
///
/// A refused `Start` is different in kind and is returned unchanged.
/// Nothing was spawned and, just as importantly, nothing was recorded, so
/// the machine really is exactly as it was. The two [`Daemon::describe`]
/// calls before the `Start` are in the same position, and their errors are
/// passed through for the same reason.
///
/// [`Daemon::delete`] and [`State::write`] are the two that are not.
/// Both run only once the newcomer has been accepted, so an error from
/// either leaves the sheep serving the new release with the changeover
/// half finished: an old instance still registered, or `deploy.toml` not
/// yet naming the sha. Neither is repaired here, because the release those
/// steps were finishing is the one that just proved itself, and the next
/// `shep-deploy deploy` writes the record anyway.
pub async fn cut_over<D: Daemon>(daemon: &D, prepared: Prepared) -> Result<String, Error> {
    let Prepared {
        tree,
        mut state,
        sha,
        app,
        previous_config,
    } = prepared;
    let sheep = tree.sheep().to_owned();

    // Captured BEFORE the Start, and this is the only moment it can be:
    // afterwards two rows share this name and nothing on the wire says
    // which of them was already here. Reading it later would risk deleting
    // the release that was just deployed.
    let before = daemon.describe(&sheep).await?;
    let previous: Vec<u32> = before.iter().map(|info| info.id).collect();
    let generation = Generation::of(daemon, &sheep).await?;

    // THE line this task exists for. Over the socket a `cwd` of None stays
    // None and the child inherits the SHEPHERD's working directory, and a
    // Flockfile registered by the CLI from inside a release is canonicalised
    // to that release and pinned there. An explicit one is stored verbatim,
    // symlink and all, which is the only spelling that follows a swap.
    let mut registering = app;
    registering.cwd = Some(tree.current().display().to_string());

    match attempt(daemon, &sheep, registering, &generation).await {
        CutOver::Done => {
            // By id, because Start added the newcomer BESIDE these under
            // the same name and a name selector would take it down too.
            for id in previous {
                daemon.delete(id).await?;
            }
            state.deployed = Some(sha.clone());
            state.write(&tree.state_file())?;
            Ok(sha)
        }
        // Nothing was spawned and nothing was recorded, so there is nothing
        // to clean up and nothing to say beyond what the shepherd said.
        CutOver::NotStarted(source) => Err(source),
        CutOver::NotVerified(why) => {
            let repaired = undo_start(daemon, &sheep, &previous, previous_config).await;
            Err(Error::CutOver {
                sheep,
                why,
                repaired,
            })
        }
        CutOver::Failed(source) => {
            undo_start(daemon, &sheep, &previous, previous_config).await;
            Err(source)
        }
    }
}

/// What a cutover's `Start` and the watching that follows it came to.
///
/// The variants turn on what has to be cleaned up rather than on what went
/// wrong, which is why [`CutOver::NotStarted`] is its own arm: before the
/// shepherd accepts the `Start` nothing has been spawned and nothing has
/// been recorded.
enum CutOver {
    /// A new instance came up and was still the same instance after the
    /// dwell.
    Done,
    /// The shepherd refused the `Start`.
    NotStarted(Error),
    /// The `Start` was accepted and the newcomer did not come up, or did
    /// not stay up.
    NotVerified(String),
    /// The `Start` was accepted and something failed while watching.
    Failed(Error),
}

/// How long a newcomer gets to APPEAR before the cutover gives up on it.
///
/// Only to appear. The dwell below is what actually decides health, so
/// this does not have to cover a slow readiness path: a probeless app that
/// sits in `Starting` well past eight seconds is fine here, because
/// [`is_alive`] accepts `Starting` and the question at this stage is
/// only whether a process exists at all.
///
/// A whole `listen_timeout` plus the slack shep allows itself, rather than
/// `crate::deploy`'s per-instance product: a cutover spawns ONE
/// instance and drains nothing, where a reload replaces every instance one
/// at a time and has to wait out each drain.
fn cutover_budget(app: &AppConfig) -> Duration {
    app.listen_timeout.as_duration() + RELOAD_DEADLINE_SLACK
}

/// Registers `app`, waits for a newcomer, and watches it for a dwell.
///
/// This function cannot return an error, and that is the point rather than
/// tidiness: every fallible step after the shepherd accepts the `Start`
/// happens here, so [`cut_over`]'s match is exhaustive over what has to be
/// cleaned up instead of over what went wrong, and a step added here later
/// has nowhere to escape to. Same structure as `crate::deploy`'s `land`,
/// for the same reason.
///
/// # This check is `Verify::Alive`, and it is weaker than the deploy path's
///
/// It establishes that a new process was spawned and that after [`DWELL`]
/// it is the same process, not errored, and has not restarted. It does NOT
/// establish that the release serves anything, and it cannot.
///
/// The reason is shep's, not this crate's. `handle_ready_result` marks a
/// FRESH SPAWN `Online` when `listen_timeout` elapses whatever the readiness
/// probe said (Rin, 2026-08-08: erroring instead would turn a slow start
/// into a restart loop), and defers to `reload_ready_result`, which DOES
/// abort on a readiness timeout, only for a reload replacement. So `Online`
/// on this path is "the probe passed OR the timeout elapsed" with nothing
/// on the wire distinguishing the two, and [`crate::verify`] is sound
/// precisely because it verifies a reload. A check of `is_new && Online`
/// here returns success in milliseconds for a release that is already dead.
///
/// `state.verify` is deliberately not consulted. `Probed` is unavailable on
/// this path, so honouring it would be a lie and refusing a target that
/// lacks a gate it could not use would be incoherent. The weakness is
/// bounded rather than permanent: once this lands the target is an ordinary
/// deploy target and every deploy after it gets full turnover verification
/// and auto-rollback.
///
/// What it does catch is the case this task is arranged around. A newcomer
/// that cannot bind its port because the original still holds it exits, and
/// shep either errors it or respawns it under a new pid, and the dwell sees
/// both. What passes anyway is a release that starts, stays up, and serves
/// nothing, which is what `alive` has always meant.
async fn attempt<D: Daemon>(
    daemon: &D,
    sheep: &str,
    app: AppConfig,
    before: &Generation,
) -> CutOver {
    let patience = cutover_budget(&app);

    if let Err(source) = daemon.start(vec![app]).await {
        return CutOver::NotStarted(source);
    }
    let started_at = Instant::now();
    let deadline = started_at + patience;

    // Phase one: a newcomer exists and has not already failed.
    let arrived = loop {
        let flock = match daemon.describe(sheep).await {
            Ok(flock) => flock,
            Err(source) => return CutOver::Failed(source),
        };

        let newcomers: Vec<&ProcessInfo> =
            flock.iter().filter(|info| before.is_new(info)).collect();

        if !newcomers.is_empty() && newcomers.iter().all(|info| is_alive(info)) {
            break Generation::of_infos(&newcomers);
        }
        if newcomers.iter().any(|info| !is_alive(info)) {
            return CutOver::NotVerified(
                "the new instance failed before it finished starting".to_owned(),
            );
        }
        if Instant::now() >= deadline {
            return CutOver::NotVerified(format!(
                "no new instance appeared within {}s",
                started_at.elapsed().as_secs()
            ));
        }
        sleep(POLL).await;
    };

    // Phase two: the dwell, which is what actually decides this.
    sleep(DWELL).await;
    let flock = match daemon.describe(sheep).await {
        Ok(flock) => flock,
        Err(source) => return CutOver::Failed(source),
    };

    let survivors: Vec<&ProcessInfo> = flock
        .iter()
        .filter(|info| arrived.holds(info) && is_alive(info))
        .collect();

    if survivors.len() != arrived.instances() as usize {
        // A different pid means shep respawned it, which means it died.
        return CutOver::NotVerified(format!(
            "the new instance did not stay up for {}s after starting",
            DWELL.as_secs()
        ));
    }
    // Belt and braces beside the pid check: a crash and respawn back onto a
    // pid this generation already held is vanishingly unlikely, and this
    // costs one comparison to rule out. No fixture arranges it, because a
    // real shepherd does not do it and a test that staged it would be
    // pinning fiction.
    if survivors.iter().any(|info| info.restarts > 0) {
        return CutOver::NotVerified(
            "the new instance restarted while it was being watched".to_owned(),
        );
    }

    CutOver::Done
}

/// Removes every instance this cutover added, and puts the shepherd's
/// persisted record back the way it was.
///
/// Two separate repairs, because the `Start` did two separate things.
///
/// Deleting the newcomer undoes the process. It does NOT undo the record:
/// `FlockRegistry` is keyed by name and records on every accepted `Start`,
/// and the surviving original keeps that name alive so the roll's own prune
/// never drops the poisoned entry. Left alone, `shep muster` after a reboot
/// brings the sheep back from the release that was just rejected, and this
/// dog's own [`roll::registered`] reads the wrong `cwd` from then on.
///
/// So the record is put back the only way the wire allows: a second `Start`
/// carrying the ORIGINAL config, which re-records it, followed by deleting
/// the instance that `Start` necessarily spawned. There is no request that
/// registers without spawning; `RegisterAtRest` is a supervisor command
/// muster uses and is not on the wire. The cost is a second instance of the
/// app alive for as long as those two calls take.
///
/// Answers whether the record was restored, because the error text differs:
/// a repair that failed leaves something an operator cannot see in
/// `shep flock` and has to be told about by name.
async fn undo_start<D: Daemon>(
    daemon: &D,
    sheep: &str,
    previous: &[u32],
    original: AppConfig,
) -> bool {
    let Ok(flock) = daemon.describe(sheep).await else {
        return false;
    };
    for info in flock.iter().filter(|info| !previous.contains(&info.id)) {
        // Failures dropped: this path is already failing, and an operator
        // needs the reason they got here rather than a second error about
        // the cleanup. What a failure leaves is a newcomer beside the
        // original, which `shep flock` shows plainly.
        let _ = daemon.delete(info.id).await;
    }

    if daemon.start(vec![original]).await.is_err() {
        return false;
    }
    let Ok(flock) = daemon.describe(sheep).await else {
        return false;
    };
    for info in flock.iter().filter(|info| !previous.contains(&info.id)) {
        let _ = daemon.delete(info.id).await;
    }
    true
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::process::Command;
    use std::time::Duration;

    use shep_client::RequestError;
    use shep_client::shep_core::protocol::{ProcessInfo, RpcError, RpcErrorCode};
    use shep_client::shep_core::status::ProcStatus;
    use tokio::time::Instant;

    use super::*;

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
        async fn delete(&self, _id: u32) -> Result<(), Error> {
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

    // ---- the cutover ----------------------------------------------------

    /// The pid of the first pre-existing instance; a second counts up from
    /// it.
    const ORIGINAL_PID: u32 = 100;
    /// The id of the first instance the cutover's own `Start` spawns.
    ///
    /// A second newcomer counts DOWN from it, so no id in these fixtures is
    /// ever also a pid. An assertion aimed at the wrong one then fails
    /// rather than coinciding.
    const NEWCOMER_ID: u32 = 99;
    /// The pid of the first newcomer; a second counts up from it.
    const NEWCOMER_PID: u32 = 200;
    /// The id of the instance a repair `Start` necessarily spawns.
    const REPAIR_ID: u32 = 500;
    /// The pid of that instance.
    const REPAIR_PID: u32 = 700;

    /// What a newcomer looks like at some moment after the `Start`.
    #[derive(Debug, Clone, Copy)]
    enum Newcomer {
        /// No row at all. This is what the first poll or two really see: a
        /// `Start` is an acceptance, and the shepherd spawns afterwards.
        Absent,
        /// A row, at this status, pid and restart count.
        Up {
            status: ProcStatus,
            pid: u32,
            restarts: u32,
        },
    }

    /// The newcomer's state at `elapsed`, from a step function written as
    /// `(from, state)` pairs in ascending order.
    fn newcomer_at(script: &[(Duration, Newcomer)], elapsed: Duration) -> Newcomer {
        script
            .iter()
            .rev()
            .find(|(from, _)| *from <= elapsed)
            .map_or(Newcomer::Absent, |(_, state)| *state)
    }

    /// A [`Daemon`] that answers `describe` the way a real shepherd does
    /// through a cutover, and records every `start` and `delete`.
    ///
    /// The originals are `Online` throughout, which is not a convenience:
    /// `Request::Start` on a registered name ADDS an instance beside them,
    /// and a real shepherd keeps the old one serving until something
    /// removes it. A double that turned the flock over instantly would be
    /// the same fiction that let the engine plan's worst blocker survive
    /// unit testing.
    ///
    /// The newcomers are a function of elapsed time since the `Start`, so a
    /// fixture says what the release DOES rather than counting polls: under
    /// `start_paused` the clock moves only when the code under test sleeps,
    /// so `attempt`'s 100ms poll and its ten-second dwell land on the
    /// script's own thresholds deterministically.
    struct CutOverDouble {
        /// The ids the shepherd already has for this sheep.
        originals: Vec<u32>,
        /// What each newcomer looks like as time passes since the `Start`.
        script: Vec<(Duration, Newcomer)>,
        /// Which `start` call, counting from zero, the shepherd refuses.
        refuses: Option<usize>,
        /// Every `start` the shepherd accepted, in order.
        starts: RefCell<Vec<AppConfig>>,
        /// Every id passed to `delete`, in order.
        deletes: RefCell<Vec<u32>>,
        /// How many `start` calls have arrived, refused ones included.
        attempts: Cell<usize>,
        /// When the cutover's own `Start` was accepted.
        accepted_at: Cell<Option<Instant>>,
        /// How many instances the repair `Start`s have spawned.
        repairs: Cell<u32>,
    }

    impl CutOverDouble {
        fn new(
            originals: &[u32],
            script: Vec<(Duration, Newcomer)>,
            refuses: Option<usize>,
        ) -> Self {
            Self {
                originals: originals.to_vec(),
                script,
                refuses,
                starts: RefCell::new(Vec::new()),
                deletes: RefCell::new(Vec::new()),
                attempts: Cell::new(0),
                accepted_at: Cell::new(None),
                repairs: Cell::new(0),
            }
        }

        /// Every app the shepherd accepted a `Start` for, in order.
        fn started(&self) -> Vec<AppConfig> {
            self.starts.borrow().clone()
        }

        /// Every id the cutover deleted, in order.
        fn deleted(&self) -> Vec<u32> {
            self.deletes.borrow().clone()
        }

        fn is_deleted(&self, id: u32) -> bool {
            self.deletes.borrow().contains(&id)
        }

        fn row(&self, id: u32, status: ProcStatus, pid: u32, restarts: u32) -> ProcessInfo {
            let _ = self;
            ProcessInfo::builder(id, "bpm", status)
                .pid(Some(pid))
                .restarts(restarts)
                .build()
        }
    }

    impl Daemon for CutOverDouble {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            unimplemented!()
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            unimplemented!()
        }
        async fn describe(&self, _sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            let mut flock = Vec::new();

            for (offset, id) in self.originals.iter().enumerate() {
                let offset = u32::try_from(offset).expect("a handful of instances");
                if !self.is_deleted(*id) {
                    flock.push(self.row(*id, ProcStatus::Online, ORIGINAL_PID + offset, 0));
                }
            }

            if let Some(accepted_at) = self.accepted_at.get()
                && let Newcomer::Up {
                    status,
                    pid,
                    restarts,
                } = newcomer_at(&self.script, Instant::now() - accepted_at)
            {
                for offset in 0..self.originals.len() {
                    let offset = u32::try_from(offset).expect("a handful of instances");
                    let id = NEWCOMER_ID - offset;
                    if !self.is_deleted(id) {
                        flock.push(self.row(id, status, pid + offset, restarts));
                    }
                }
            }

            for offset in 0..self.repairs.get() {
                let id = REPAIR_ID + offset;
                if !self.is_deleted(id) {
                    flock.push(self.row(id, ProcStatus::Online, REPAIR_PID + offset, 0));
                }
            }

            Ok(flock)
        }
        async fn start(&self, apps: Vec<AppConfig>) -> Result<(), Error> {
            let attempt = self.attempts.get();
            self.attempts.set(attempt + 1);
            if self.refuses == Some(attempt) {
                return Err(Error::Request(RequestError::Rpc(RpcError {
                    code: RpcErrorCode::Internal,
                    message: "bpm cannot be started".to_owned(),
                })));
            }

            self.starts.borrow_mut().extend(apps);
            if attempt == 0 {
                self.accepted_at.set(Some(Instant::now()));
            } else {
                self.repairs.set(self.repairs.get() + 1);
            }
            Ok(())
        }
        async fn delete(&self, id: u32) -> Result<(), Error> {
            self.deletes.borrow_mut().push(id);
            Ok(())
        }
        async fn reload(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
        async fn restart(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
        async fn save_roll(&self) -> Result<PathBuf, Error> {
            unimplemented!()
        }
    }

    /// The tempdirs a fixture has to keep alive. `Prepared` names paths
    /// inside both, and a `TempDir` deletes its directory when it drops.
    struct Dirs {
        _home: tempfile::TempDir,
        _checkout: tempfile::TempDir,
    }

    /// A prepared tree for `bpm`, plus a double scripted with `script`.
    async fn cutover_fixture_of(
        originals: &[u32],
        script: Vec<(Duration, Newcomer)>,
        refuses: Option<usize>,
    ) -> (CutOverDouble, Prepared, Dirs) {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        let roll = RollOf(&[("bpm", checkout.path())]);
        let prepared = prepare(&roll, home.path(), "bpm").await.expect("prepares");

        (
            CutOverDouble::new(originals, script, refuses),
            prepared,
            Dirs {
                _home: home,
                _checkout: checkout,
            },
        )
    }

    /// The ordinary shape: no row for the first few polls, then `Starting`,
    /// then `Online`, and it stays that way through the dwell.
    ///
    /// Staged rather than instant on purpose. `attempt` polls every 100ms,
    /// so a newcomer that only appears at 250ms is adopted on the fourth
    /// poll, which is what a shepherd that has accepted a `Start` and not
    /// yet spawned looks like.
    fn comes_up() -> Vec<(Duration, Newcomer)> {
        vec![
            (Duration::ZERO, Newcomer::Absent),
            (
                Duration::from_millis(250),
                Newcomer::Up {
                    status: ProcStatus::Starting,
                    pid: NEWCOMER_PID,
                    restarts: 0,
                },
            ),
            (
                Duration::from_millis(450),
                Newcomer::Up {
                    status: ProcStatus::Online,
                    pid: NEWCOMER_PID,
                    restarts: 0,
                },
            ),
        ]
    }

    /// One original, and a newcomer that comes up and stays.
    async fn cutover_fixture() -> (CutOverDouble, Prepared, Dirs) {
        cutover_fixture_of(&[7], comes_up(), None).await
    }

    /// Several originals, each replaced by a newcomer that comes up and
    /// stays.
    async fn cutover_fixture_with_instances(originals: &[u32]) -> (CutOverDouble, Prepared, Dirs) {
        cutover_fixture_of(originals, comes_up(), None).await
    }

    /// A newcomer that reports `Online` at once and is gone by the dwell.
    ///
    /// The exact shape finding F1 describes: shep marks a FRESH spawn
    /// `Online` when `listen_timeout` elapses whatever the readiness probe
    /// said, so a release that is already dead reports `Online` on time.
    async fn cutover_fixture_online_then_gone() -> (CutOverDouble, Prepared, Dirs) {
        cutover_fixture_of(
            &[7],
            vec![
                (
                    Duration::ZERO,
                    Newcomer::Up {
                        status: ProcStatus::Online,
                        pid: NEWCOMER_PID,
                        restarts: 0,
                    },
                ),
                (Duration::from_secs(5), Newcomer::Absent),
            ],
            None,
        )
        .await
    }

    /// A newcomer present at every poll, under a new pid by the dwell.
    ///
    /// `Online` rather than `Starting`, because that is what a crash loop
    /// really looks like: each spawn reaches `Online` on its
    /// `listen_timeout` and then dies, so status alone never says anything
    /// is wrong and only the pid does.
    async fn cutover_fixture_crash_looping() -> (CutOverDouble, Prepared, Dirs) {
        cutover_fixture_of(
            &[7],
            vec![
                (Duration::ZERO, Newcomer::Absent),
                (
                    Duration::from_millis(250),
                    Newcomer::Up {
                        status: ProcStatus::Online,
                        pid: NEWCOMER_PID,
                        restarts: 0,
                    },
                ),
                (
                    Duration::from_secs(5),
                    Newcomer::Up {
                        status: ProcStatus::Online,
                        pid: NEWCOMER_PID + 50,
                        restarts: 1,
                    },
                ),
            ],
            None,
        )
        .await
    }

    /// A newcomer that comes up and is `Errored` by the dwell: the
    /// port-collision shape, where the original still holds the port.
    fn dies_during_dwell() -> Vec<(Duration, Newcomer)> {
        vec![
            (Duration::ZERO, Newcomer::Absent),
            (
                Duration::from_millis(250),
                Newcomer::Up {
                    status: ProcStatus::Online,
                    pid: NEWCOMER_PID,
                    restarts: 0,
                },
            ),
            (
                Duration::from_secs(5),
                Newcomer::Up {
                    status: ProcStatus::Errored,
                    pid: NEWCOMER_PID,
                    restarts: 0,
                },
            ),
        ]
    }

    /// The port-collision shape, with the repair `Start` accepted.
    async fn cutover_fixture_dies_during_dwell() -> (CutOverDouble, Prepared, Dirs) {
        cutover_fixture_of(&[7], dies_during_dwell(), None).await
    }

    /// The same, with every `start` after the cutover's own refused.
    ///
    /// The SECOND call, not the first: the cutover's own `Start` has to be
    /// accepted or the run never reaches the repair this exists to check.
    async fn cutover_fixture_dies_and_refuses_repair() -> (CutOverDouble, Prepared, Dirs) {
        cutover_fixture_of(&[7], dies_during_dwell(), Some(1)).await
    }

    /// A newcomer that never appears at all, so the phase-one budget is
    /// what ends the wait.
    ///
    /// Absent rather than stuck in `Starting`: `is_alive` accepts
    /// `Starting`, deliberately, because a probeless app sits there for its
    /// whole `listen_timeout` and that is not a failure. What the budget
    /// bounds is a newcomer APPEARING.
    async fn cutover_fixture_never_appears() -> (CutOverDouble, Prepared, Dirs) {
        cutover_fixture_of(&[7], vec![(Duration::ZERO, Newcomer::Absent)], None).await
    }

    /// A shepherd that refuses the cutover's `Start` outright.
    async fn cutover_fixture_refusing_start() -> (CutOverDouble, Prepared, Dirs) {
        cutover_fixture_of(&[7], comes_up(), Some(0)).await
    }

    /// fails if the new registration's cwd is anything but the `current`
    /// symlink, set EXPLICITLY. A Flockfile app's DEFAULTED cwd is resolved
    /// at registration, so registering from inside a release pins the sheep
    /// to that release and every later swap moves a symlink the app no
    /// longer reaches. Measured on a real shepherd: the reload after a swap
    /// re-ran the OLD release's script. This is the single most
    /// load-bearing line in the crate and it fails silently, one release
    /// later, when it is wrong.
    #[tokio::test(start_paused = true)]
    async fn the_new_registration_names_current_explicitly() {
        let (daemon, prepared, _dirs) = cutover_fixture().await;
        let current = prepared.tree.current();
        cut_over(&daemon, prepared).await.expect("cuts over");

        let started = daemon.started();
        assert_eq!(started.len(), 1, "exactly one Start");
        assert_eq!(
            started[0].cwd.as_deref(),
            current.to_str(),
            "cwd must be the current symlink itself, not a release and not None"
        );
    }

    /// fails if the OLD instances are deleted by name rather than by id, or
    /// if the newcomer is deleted instead. A name selector would take the
    /// new instance down with them, because `Start` added it BESIDE the old
    /// under the same name, and an id read after the `Start` could name the
    /// release that was just deployed.
    #[tokio::test(start_paused = true)]
    async fn only_the_old_instances_are_deleted_and_by_id() {
        let (daemon, prepared, _dirs) = cutover_fixture().await;
        cut_over(&daemon, prepared).await.expect("cuts over");
        assert_eq!(daemon.deleted(), vec![7], "the pre-existing instance's id");
        assert!(
            !daemon.deleted().contains(&NEWCOMER_ID),
            "99 is the newcomer"
        );
    }

    /// fails if a scaled sheep loses only one of the instances it was
    /// running. `Start` adds one newcomer per configured instance beside
    /// every existing one, so a cutover that deleted a single id would
    /// leave the rest serving the pre-adoption checkout indefinitely.
    #[tokio::test(start_paused = true)]
    async fn every_replaced_instance_is_deleted() {
        let (daemon, prepared, _dirs) = cutover_fixture_with_instances(&[7, 8]).await;
        cut_over(&daemon, prepared).await.expect("cuts over");
        assert_eq!(daemon.deleted(), vec![7, 8]);
    }

    /// fails if a new instance that never comes up is left registered. The
    /// likeliest cause is the one the design names: both instances are
    /// alive at once, so without SO_REUSEPORT the new one cannot bind the
    /// port and dies. Leaving it behind gives the operator a permanently
    /// errored second instance of their app and an old one still serving,
    /// with nothing saying which is which.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_never_comes_up_is_deleted_and_the_old_one_kept() {
        let (daemon, prepared, _dirs) = cutover_fixture_dies_during_dwell().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        assert!(
            daemon.deleted().contains(&NEWCOMER_ID),
            "the newcomer is removed"
        );
        assert!(!daemon.deleted().contains(&7), "the old instance is kept");
        let shown = err.to_string();
        assert!(shown.contains("SO_REUSEPORT"), "{shown}");
    }

    /// fails if a release that shep called `Online` without the probe ever
    /// passing is accepted. This is the whole of finding F1 and it is the
    /// engine plan's round-3 blocker on a different path: shep marks a
    /// FRESH SPAWN Online when `listen_timeout` elapses whatever the probe
    /// said, and aborts only a RELOAD replacement on a readiness timeout.
    /// So `is_new(info) && Online` establishes nothing about a Start, and
    /// measured against a real shepherd it returned Done in 15.6ms on a
    /// dead-on-arrival release, after which the healthy original is deleted
    /// by id and `state.deployed` written.
    ///
    /// The fixture is the exact shape shep produces: the newcomer reports
    /// Online, on time, and is gone by the dwell.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_went_online_without_serving_is_still_rejected() {
        let (daemon, prepared, _dirs) = cutover_fixture_online_then_gone().await;
        cut_over(&daemon, prepared)
            .await
            .expect_err("the dwell catches it");
    }

    /// fails if a newcomer that crash-loops through the dwell is accepted.
    /// An app whose release cannot run is restarted by shep, so it is
    /// present at every poll and present at the dwell, under a DIFFERENT
    /// pid each time. Pid identity across the dwell is what catches it, and
    /// `restarts` moving is the second signal.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_crash_loops_through_the_dwell_is_rejected() {
        let (daemon, prepared, _dirs) = cutover_fixture_crash_looping().await;
        cut_over(&daemon, prepared)
            .await
            .expect_err("the pids moved");
    }

    /// fails if an abandoned cutover leaves shep's persisted roll naming
    /// the release that was just rejected. `FlockRegistry` is name-keyed
    /// and records on every accepted Start, and deleting the newcomer does
    /// NOT undo it, because the surviving original keeps the name alive so
    /// the roll's prune never drops it. Verified end to end: after the
    /// delete the roll named the new release while the live pid executed in
    /// the old one, and `shep kill` plus a restart plus `shep muster`
    /// brought the sheep back from the abandoned release.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_cutover_puts_the_original_config_back_in_the_roll() {
        let (daemon, prepared, _dirs) = cutover_fixture_dies_during_dwell().await;
        let original = prepared.previous_config.clone();

        cut_over(&daemon, prepared).await.expect_err("gives up");

        let last = daemon.started().last().cloned().expect("a repair Start");
        assert_eq!(
            last.cwd, original.cwd,
            "the roll is re-recorded at the old cwd"
        );
        assert_eq!(
            daemon.deleted().len(),
            2,
            "the newcomer, and the instance the repair Start spawned"
        );
    }

    /// fails if a repair that could not be made is glossed over.
    /// [`Error::CutOver`] used to say the original was serving
    /// "unchanged", which is true of the process and false of the persisted
    /// flock: a reboot would silently resurrect the rejected release. When
    /// the repair fails the operator has to be told that specifically,
    /// because it is the half they cannot see in `shep flock`.
    #[tokio::test(start_paused = true)]
    async fn a_failed_roll_repair_is_named_not_glossed() {
        let (daemon, prepared, _dirs) = cutover_fixture_dies_and_refuses_repair().await;
        let err = cut_over(&daemon, prepared).await.expect_err("gives up");
        let shown = err.to_string();
        assert!(shown.contains("muster"), "{shown}");
        assert!(
            shown.contains("reboot") || shown.contains("restart"),
            "{shown}"
        );
    }

    /// fails if a Start the shepherd refused is treated as one it accepted.
    /// Nothing was spawned, so there is nothing to delete, and issuing a
    /// Delete against an id that was never created would either do nothing
    /// or, worse, match something else.
    #[tokio::test(start_paused = true)]
    async fn a_refused_start_deletes_nothing() {
        let (daemon, prepared, _dirs) = cutover_fixture_refusing_start().await;
        cut_over(&daemon, prepared).await.expect_err("refused");
        assert!(daemon.deleted().is_empty());
    }

    /// fails if the record is advanced before the newcomer verified.
    /// `deploy.toml` naming a release nothing has served is the same defect
    /// the engine plan spent five rounds removing from the deploy path, and
    /// it must not come back through this one.
    #[tokio::test(start_paused = true)]
    async fn the_record_advances_only_after_the_newcomer_is_online() {
        let (daemon, prepared, _dirs) = cutover_fixture_never_appears().await;
        let path = prepared.tree.state_file();
        cut_over(&daemon, prepared).await.expect_err("gives up");
        assert_eq!(State::read(&path).expect("reads").deployed, None);
    }
}
