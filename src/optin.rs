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
use crate::config::DogConfig;
use crate::daemon::Daemon;
use crate::deploy::RELOAD_DEADLINE_SLACK;
use crate::error::Error;
use crate::flockfile;
use crate::git;
use crate::lock;
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
/// The record it writes says `watch = "manual"`, whatever the sheep ends
/// up with: nothing has been served from the tree yet, and [`cut_over`] is
/// what promotes it. A cutover abandoned partway therefore leaves a target
/// the poll loop passes over rather than one it refuses every interval.
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
    config: &DogConfig,
) -> Result<Prepared, Error> {
    let tree = Tree::for_sheep(shep_home, sheep);
    if tree.state_file().is_file() {
        // Two very different situations share this one condition, and
        // telling them apart is not a nicety. A target that was cut over
        // has served from its tree and `shep deploy` is exactly right for
        // it. A target whose cutover was ABANDONED has served nothing, and
        // `shep deploy` on it builds, swaps and reloads the sheep at its
        // own checkout, sees a full pid turnover because the reload really
        // did replace that instance, and reports success for a release
        // nothing ever ran. `deployed` is what separates them.
        //
        // A record that cannot be READ is neither, and guessing has no
        // cautious side: the two branches give opposite advice and one of
        // them says to remove the tree. A cut-over sheep's cwd is inside
        // that tree, so following it on a live target destroys a running
        // service's working directory, which is the one thing here that
        // cannot be undone. So this refuses instead.
        let state = State::read(&tree.state_file()).map_err(|source| {
            Error::Config(format!(
                "{sheep} has a deploy tree at {} and its record at {} cannot be read. \
                 Refusing rather than guessing. That record is the only thing that says \
                 whether {sheep} was ever cut over, and the two answers need opposite \
                 handling. Do NOT remove the tree until you know which it is: if {sheep} WAS \
                 cut over, its working directory is inside it and removing it takes a running \
                 service's cwd with it. Restore or repair that file first - `shep describe \
                 {sheep}` shows whether the sheep is running from inside this tree. Reading it \
                 failed with: {source}",
                tree.root().display(),
                tree.state_file().display()
            ))
        })?;
        return Err(Error::Config(if state.deployed.is_some() {
            format!(
                "{sheep} is already a deploy target: its tree is at {}. Deploy it with \
                 `shep deploy {sheep}`, or change how it is watched with \
                 `shep deploy {sheep} --watch auto|manual`.",
                tree.releases().display()
            )
        } else {
            format!(
                "{sheep} has a deploy tree at {} but was never cut over to it: its record names \
                 no deployed release, so nothing has ever been served from that tree. An \
                 abandoned first cutover leaves exactly this. Do NOT run `shep deploy {sheep}` \
                 against it - that would reload the sheep at its own checkout and report success \
                 for a release nothing served. Remove {} and run `shep-deploy setup {sheep}` \
                 again once the cause of the first failure is fixed.",
                tree.releases().display(),
                tree.root().display()
            )
        }));
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
        failed: None,
        verify: Verify::default(),
        // `manual` until the cutover lands, and `cut_over` is what promotes
        // it. A tree nothing has served from is not a deploy target - the
        // deploy path refuses one outright - so writing `auto` here would
        // hand the poll loop a target it can only refuse, once every
        // interval, for as long as the abandoned tree sits there.
        watch: Watch::Manual,
        // The two fields the whole restore depends on, captured once, here,
        // and never touched again.
        origin_cwd: Some(checkout.clone()),
        origin_script: Some(previous_config.script.clone()),
        checkout,
    };

    // Same hold a deploy takes, for the same reason: everything below writes
    // to this tree, and a poll tick can be doing the same at the same moment.
    // After the directory exists, because that is what the lock file lives in.
    let _deploying = lock::hold(&tree)?;

    std::fs::create_dir_all(tree.releases()).map_err(|source| Error::Io {
        path: tree.releases(),
        source,
    })?;
    git::init_bare(&tree.git())?;
    git::fetch(&tree.git(), &state.remote, config.git_timeout)?;
    let sha = git::remote_head(&tree.git(), &state.branch)?;

    let release = tree.release(&sha);
    // Shared with `deploy::attempt` rather than a bare `worktree_add`, and
    // that is what makes this function's own retry story true. `git worktree
    // add` refuses a path that already exists ("fatal: `<path>` already
    // exists") and refuses one it still has registered after the directory
    // was removed ("missing but already registered worktree"). So every run
    // that died anywhere from here onward left a release directory that made
    // the next run fail on git rather than resume, which is the opposite of
    // what the doc above promises.
    crate::deploy::checkout_release(&tree, &sha)?;
    shared::link_cache(&release, &tree.cache_target())?;
    // Held, not recomputed. This is the only record of which files came from
    // the operator's own checkout rather than from the repository, and
    // `flockfile` needs it to know whether an override is theirs.
    let shared_paths = shared::to_link(&state.checkout)?;
    shared::link_into(&release, &state.checkout, &shared_paths)?;

    let app = flockfile::app_config(&release, sheep, &shared_paths)?;
    let spec = flockfile::build_spec(&release, &shared_paths)?;
    build::run(
        sheep,
        &release,
        &spec,
        app.user.as_deref(),
        &config.passthrough,
        &tree.cache_target(),
        config.build_timeout,
    )
    .await?;

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
/// the machine really is exactly as it was. The [`Daemon::describe`] before
/// the `Start` is in the same position, and its error is passed through for
/// the same reason.
///
/// Two steps run only once the newcomer has been accepted, and they are
/// not alike.
///
/// [`Error::Stranded`] is a [`Daemon::delete`] that failed. The cutover
/// landed and the new release is serving, but an instance it replaced is
/// still registered, and that does NOT settle: a later deploy reloads every
/// instance of the name and respawns each from its own spec, so the
/// leftover comes back on the pre-adoption config every time. The ids are
/// collected rather than the first failure returned, so the error can name
/// every one of them.
///
/// [`State::write`] failing is the mild one, and it is passed through. The
/// release is serving and only the record is missing, which the next
/// `shep-deploy deploy` writes.
///
/// A cutover that lands with nothing stranded promotes the record's `watch`
/// from the `manual` [`prepare`] wrote to `auto`, which is what makes the
/// target the poll loop's. A stranded one does not: instances it could not
/// delete are still registered, and every later deploy reloads the name and
/// respawns each of them from its own pre-adoption spec, so a loop polling
/// it would bring them back on a schedule. Remove the ids the error names,
/// then `shep deploy <sheep> --watch auto`.
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
    //
    // One read, not two. The ids to delete and the pids to compare against
    // have to describe the same instant: asking twice would let an instance
    // restart between them, so the generation would be frozen against a set
    // of pids the id list never named. That is the whole argument
    // `Generation::of_infos` was added for, and it applies to its own
    // caller first.
    let before = daemon.describe(&sheep).await?;
    let rows: Vec<&ProcessInfo> = before.iter().collect();
    let previous: Vec<u32> = rows.iter().map(|info| info.id).collect();
    let generation = Generation::of_infos(&rows);

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
            //
            // Collected rather than `?`d. A delete that failed partway
            // leaves instances that a later deploy does not clean up but
            // RELOADS, respawning each from its own pre-adoption spec, so
            // the ids have to reach the operator. Stopping at the first
            // failure would hide the rest of them.
            let mut stranded = Vec::new();
            for id in previous {
                if daemon.delete(id).await.is_err() {
                    stranded.push(id);
                }
            }

            // Written either way: the release is serving, so the record
            // naming it is true whatever the cleanup did.
            state.deployed = Some(sha.clone());
            // The promotion out of `manual`, and only on the clean path.
            // The tree has now been served from, which is what makes it a
            // deploy target at all. A cutover that left instances behind
            // has not finished: every later deploy reloads the name and
            // respawns each leftover from its own pre-adoption spec, so a
            // loop polling this target would bring them back on a
            // schedule. The operator removes the ids the error names and
            // then asks for `--watch auto`.
            if stranded.is_empty() {
                state.watch = Watch::Auto;
            }
            state.write(&tree.state_file())?;

            if stranded.is_empty() {
                Ok(sha)
            } else {
                Err(Error::Stranded {
                    sheep,
                    sha,
                    ids: stranded,
                })
            }
        }
        // Nothing was spawned and nothing was recorded, so there is nothing
        // to clean up and nothing to say beyond what the shepherd said.
        CutOver::NotStarted(source) => Err(source),
        CutOver::NotVerified(why) => {
            let undone = undo_start(daemon, &sheep, &previous, previous_config).await;
            Err(Error::CutOver {
                sheep,
                why,
                removed: undone.removed,
                repaired: undone.recorded,
                tree: tree.root().to_owned(),
                source: None,
            })
        }
        // Reported the same way rather than returned bare. This arm is
        // reached because a request failed, which means the shepherd went
        // quiet, which is exactly when `undo_start`'s own opening
        // `describe` fails too. So it is the arm most likely to leave a
        // poisoned roll and the least able to repair one, and a bare
        // transport error would tell the operator nothing about that.
        CutOver::Failed(source) => {
            let undone = undo_start(daemon, &sheep, &previous, previous_config).await;
            Err(Error::CutOver {
                sheep,
                why: SHEPHERD_QUIET.to_owned(),
                removed: undone.removed,
                repaired: undone.recorded,
                tree: tree.root().to_owned(),
                source: Some(Box::new(source)),
            })
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

/// What a first cutover most often runs into, said once.
///
/// Appended to every `why` where it could be the cause, and deliberately not
/// to the one where it cannot: a `Start` the shepherd accepted that then
/// produced no row at all never got as far as binding anything.
const PORT_COLLISION: &str = "The first cutover is the one deploy that runs two instances at \
     once, so an app that does not bind with SO_REUSEPORT cannot take its own port while the \
     original still holds it. Every deploy after the first replaces the instance rather than \
     joining it, and does not meet this. If the app cannot set SO_REUSEPORT, `shep stop` it, \
     remove the tree named above, and run setup again: with the port free the newcomer binds, \
     and the cutover is the one deploy allowed to be down for a moment anyway.";

/// The `why` for a cutover that ended because the shepherd stopped
/// answering, rather than because the release failed.
const SHEPHERD_QUIET: &str = "The shepherd stopped answering while the new instance was being \
     watched, so nothing was established about it either way.";

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
            return CutOver::NotVerified(format!(
                "The new instance failed before it finished starting. {PORT_COLLISION}"
            ));
        }
        if Instant::now() >= deadline {
            return CutOver::NotVerified(format!(
                "No new instance appeared within {}s, although the shepherd accepted the start.",
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
            "The new instance did not stay up for {}s after starting. {PORT_COLLISION}",
            DWELL.as_secs()
        ));
    }
    // NOT belt and braces, and an earlier comment here saying so was wrong
    // in the direction that matters. `Generation::is_new` compares pids and
    // nothing else, and the generation was captured before the `Start`, so
    // an ORIGINAL that crashed and respawned before the real newcomer
    // appeared reads as a newcomer: phase one adopts it, and the dwell
    // finds that same pid still alive and not errored. The restart count is
    // the only thing left that rejects it. Without this check the cutover
    // returns `Done` having verified nothing, and then deletes the healthy
    // original by id.
    //
    // Not an exotic shape either. The newcomer is fighting the original for
    // its port, which is the premise of this whole task, and the original
    // is the side that can lose. Pinned by
    // `an_original_that_respawns_is_not_mistaken_for_the_newcomer`.
    if survivors.iter().any(|info| info.restarts > 0) {
        return CutOver::NotVerified(format!(
            "The instance this cutover adopted had already restarted {}s later, so what is \
             running is not the process the start spawned. {PORT_COLLISION}",
            DWELL.as_secs()
        ));
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
/// Answers both halves separately, because the error text needs both and
/// they fail independently: a newcomer left registered is visible in
/// `shep flock`, and a roll left poisoned is not.
async fn undo_start<D: Daemon>(
    daemon: &D,
    sheep: &str,
    previous: &[u32],
    original: AppConfig,
) -> Undone {
    let Ok(flock) = daemon.describe(sheep).await else {
        // Nothing is known about what needs removing, so nothing is
        // claimed. The error text sends the operator to `shep describe`.
        return Undone {
            removed: false,
            recorded: false,
        };
    };
    // Individual failures are recorded rather than returned: this path is
    // already failing, and an operator needs the reason they got here
    // rather than a second error about the cleanup.
    let mut removed = drain(daemon, &flock, previous).await;

    if daemon.start(vec![original]).await.is_err() {
        return Undone {
            removed,
            recorded: false,
        };
    }
    let Ok(flock) = daemon.describe(sheep).await else {
        return Undone {
            removed,
            recorded: false,
        };
    };
    removed &= drain(daemon, &flock, previous).await;

    Undone {
        removed,
        recorded: true,
    }
}

/// What [`undo_start`] managed to put back.
///
/// Two bools rather than one, because the `Start` did two things and either
/// repair can fail on its own. Folding them together would make the error
/// text claim one when only the other held.
struct Undone {
    /// Whether every instance this cutover added was removed.
    removed: bool,
    /// Whether the shepherd's persisted roll was re-recorded.
    recorded: bool,
}

/// Deletes every row in `flock` whose id is not in `previous`, answering
/// whether all of them went.
async fn drain<D: Daemon>(daemon: &D, flock: &[ProcessInfo], previous: &[u32]) -> bool {
    let mut all = true;
    for info in flock.iter().filter(|info| !previous.contains(&info.id)) {
        if daemon.delete(info.id).await.is_err() {
            all = false;
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use crate::fixtures;

    /// The config every test that is not about a config value runs on.
    fn test_config() -> crate::config::DogConfig {
        crate::config::DogConfig {
            interval: std::time::Duration::from_secs(30),
            retention: 5,
            git_timeout: std::time::Duration::from_secs(60),
            build_timeout: std::time::Duration::from_secs(60),
            passthrough: Vec::new(),
        }
    }

    // For `Error::source` on the variant the quiet-shepherd path returns.
    use core::error::Error as _;
    use std::cell::{Cell, RefCell};
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
        async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), Error> {
            unimplemented!()
        }
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
        fixtures::run_git(dir.path(), &["init", "-q", "-b", "main"]);
        fixtures::run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        fixtures::run_git(dir.path(), &["config", "user.name", "test"]);
        fixtures::run_git(
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
        fixtures::run_git(dir.path(), &["add", "."]);
        fixtures::run_git(dir.path(), &["commit", "-q", "-m", "initial"]);
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
            failed: None,
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
        let entries = [("bpm", checkout.path())];
        let daemon = RollOf(&entries);

        let err = prepare(&daemon, home.path(), "bpm", &test_config())
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
        let entries = [("bpm", checkout.path())];
        let daemon = RollOf(&entries);

        let prepared = prepare(&daemon, home.path(), "bpm", &test_config())
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
        fixtures::run_git(checkout.path(), &["checkout", "-q", "-b", "stable"]);
        let entries = [("bpm", checkout.path())];
        let daemon = RollOf(&entries);

        let prepared = prepare(&daemon, home.path(), "bpm", &test_config())
            .await
            .expect("prepares");
        assert_eq!(prepared.state.branch, "stable");
    }

    /// fails if a run that died before writing its record cannot simply be
    /// run again, which is what this function's own doc promises twice.
    ///
    /// The promise was false from `worktree_add` onward. `prepare` had no
    /// existence guard and no cleanup on any failure path, so the release
    /// directory a dead run left behind made the next run fail with a raw
    /// `fatal: '<path>' already exists` from git. Verified 2026-08-28.
    ///
    /// Modelled here at the narrowest window the doc calls out by name, a
    /// kill between `swap::point_at` and `state.write`: the release and
    /// `current` exist, `deploy.toml` does not, so the "already a deploy
    /// target" refusal at the top correctly does not fire and the run gets
    /// as far as the checkout before failing.
    #[tokio::test]
    async fn a_prepare_that_died_before_writing_its_record_runs_again() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        let entries = [("bpm", checkout.path())];
        let daemon = RollOf(&entries);

        let first = prepare(&daemon, home.path(), "bpm", &test_config())
            .await
            .expect("prepares");
        std::fs::remove_file(first.tree.state_file()).expect("the record a kill never wrote");

        let again = prepare(&daemon, home.path(), "bpm", &test_config())
            .await
            .expect("a tree with no record must be resumable, as the doc says");
        assert_eq!(again.sha, first.sha);
        assert!(
            again.tree.state_file().is_file(),
            "the second run must leave the record the first one never wrote"
        );
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
        let entries = [("bpm", checkout.path())];
        let daemon = RollOf(&entries);

        let prepared = prepare(&daemon, home.path(), "bpm", &test_config())
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
        let head = fixtures::head_of(checkout.path());
        fixtures::run_git(checkout.path(), &["checkout", "-q", &head]);
        let entries = [("bpm", checkout.path())];
        let daemon = RollOf(&entries);

        let err = prepare(&daemon, home.path(), "bpm", &test_config())
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

    /// What one row looks like at some moment after the `Start`.
    ///
    /// Written for the newcomers and used for the originals too, because
    /// the case that matters most is an original that stops looking like
    /// itself: see `an_original_that_respawns_is_not_mistaken_for_the_newcomer`.
    #[derive(Debug, Clone, Copy)]
    enum Shape {
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

    /// A row's shape at `elapsed`, from a step function written as
    /// `(from, shape)` pairs in ascending order.
    fn shape_at(script: &[(Duration, Shape)], elapsed: Duration) -> Shape {
        script
            .iter()
            .rev()
            .find(|(from, _)| *from <= elapsed)
            .map_or(Shape::Absent, |(_, shape)| *shape)
    }

    /// The ordinary original: `Online` at its own pid, never restarted,
    /// for as long as the cutover runs.
    fn originals_stay_online() -> Vec<(Duration, Shape)> {
        vec![(
            Duration::ZERO,
            Shape::Up {
                status: ProcStatus::Online,
                pid: ORIGINAL_PID,
                restarts: 0,
            },
        )]
    }

    /// An original that crashes and is respawned a second into the cutover:
    /// same instance id, a new pid, `restarts` at one.
    ///
    /// The shape a port fight really produces on the losing side, and the
    /// one that reads as a newcomer to a pid-only comparison.
    fn originals_respawn_mid_cutover() -> Vec<(Duration, Shape)> {
        vec![
            (
                Duration::ZERO,
                Shape::Up {
                    status: ProcStatus::Online,
                    pid: ORIGINAL_PID,
                    restarts: 0,
                },
            ),
            (
                Duration::from_secs(1),
                Shape::Up {
                    status: ProcStatus::Online,
                    pid: ORIGINAL_PID + 50,
                    restarts: 1,
                },
            ),
        ]
    }

    /// A [`Daemon`] that answers `describe` the way a real shepherd does
    /// through a cutover, and records every `start` and `delete`.
    ///
    /// The originals stay `Online` unless a fixture says otherwise, which is
    /// not a convenience: `Request::Start` on a registered name ADDS an
    /// instance beside them, and a real shepherd keeps the old one serving
    /// until something removes it. A double that turned the flock over
    /// instantly would be the same fiction that let the engine plan's worst
    /// blocker survive unit testing.
    ///
    /// Both scripts are a function of elapsed time since the `Start`, so a
    /// fixture says what the flock DOES rather than counting polls: under
    /// `start_paused` the clock moves only when the code under test sleeps,
    /// so `attempt`'s 100ms poll and its ten-second dwell land on the
    /// script's own thresholds deterministically.
    struct CutOverDouble {
        /// The ids the shepherd already has for this sheep.
        originals: Vec<u32>,
        /// What every original looks like as time passes since the `Start`,
        /// and before it.
        original_script: Vec<(Duration, Shape)>,
        /// What each newcomer looks like as time passes since the `Start`.
        script: Vec<(Duration, Shape)>,
        /// Which `start` call, counting from zero, the shepherd refuses.
        refuses: Option<usize>,
        /// Which `delete` call, counting from zero, the shepherd starts
        /// refusing at. Every one from there on is refused.
        refuses_deletes_from: Option<usize>,
        /// How many `delete` calls have arrived, refused ones included.
        deletes_seen: Cell<usize>,
        /// How long after the `Start` every `describe` starts failing.
        mute_after: Option<Duration>,
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
        fn new(originals: &[u32], script: Vec<(Duration, Shape)>, refuses: Option<usize>) -> Self {
            Self {
                originals: originals.to_vec(),
                original_script: originals_stay_online(),
                script,
                refuses,
                refuses_deletes_from: None,
                deletes_seen: Cell::new(0),
                mute_after: None,
                starts: RefCell::new(Vec::new()),
                deletes: RefCell::new(Vec::new()),
                attempts: Cell::new(0),
                accepted_at: Cell::new(None),
                repairs: Cell::new(0),
            }
        }

        /// The same double, with every `describe` failing from `after`
        /// onwards - a shepherd that stops answering mid-cutover.
        fn going_quiet_after(mut self, after: Duration) -> Self {
            self.mute_after = Some(after);
            self
        }

        /// The same double, with every `delete` refused.
        fn refusing_deletes(self) -> Self {
            self.refusing_deletes_from(0)
        }

        /// The same double, refusing every `delete` from the `from`th on.
        ///
        /// `undo_start` drains twice - once for what the cutover's own
        /// `Start` spawned, once for what the repair `Start` did - and only
        /// the second failing is a shape no other fixture reaches.
        fn refusing_deletes_from(mut self, from: usize) -> Self {
            self.refuses_deletes_from = Some(from);
            self
        }

        /// The same double, with the originals doing something other than
        /// sitting still.
        fn while_the_originals(mut self, script: Vec<(Duration, Shape)>) -> Self {
            self.original_script = script;
            self
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
            // Before the `Start` the originals are read at zero, which is
            // where every script has them sitting still.
            let elapsed = self
                .accepted_at
                .get()
                .map_or(Duration::ZERO, |accepted_at| Instant::now() - accepted_at);

            if self.accepted_at.get().is_some()
                && self.mute_after.is_some_and(|after| elapsed >= after)
            {
                return Err(Error::Request(RequestError::Rpc(RpcError {
                    code: RpcErrorCode::Internal,
                    message: "the shepherd is not answering".to_owned(),
                })));
            }

            if let Shape::Up {
                status,
                pid,
                restarts,
            } = shape_at(&self.original_script, elapsed)
            {
                for (offset, id) in self.originals.iter().enumerate() {
                    let offset = u32::try_from(offset).expect("a handful of instances");
                    if !self.is_deleted(*id) {
                        flock.push(self.row(*id, status, pid + offset, restarts));
                    }
                }
            }

            if self.accepted_at.get().is_some()
                && let Shape::Up {
                    status,
                    pid,
                    restarts,
                } = shape_at(&self.script, elapsed)
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
            let seen = self.deletes_seen.get();
            self.deletes_seen.set(seen + 1);
            if self.refuses_deletes_from.is_some_and(|from| seen >= from) {
                return Err(Error::Request(RequestError::Rpc(RpcError {
                    code: RpcErrorCode::Internal,
                    message: format!("instance {id} cannot be deleted"),
                })));
            }
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
        async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), Error> {
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
        script: Vec<(Duration, Shape)>,
        refuses: Option<usize>,
    ) -> (CutOverDouble, Prepared, Dirs) {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        let entries = [("bpm", checkout.path())];
        let roll = RollOf(&entries);
        let prepared = prepare(&roll, home.path(), "bpm", &test_config())
            .await
            .expect("prepares");

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
    fn comes_up() -> Vec<(Duration, Shape)> {
        vec![
            (Duration::ZERO, Shape::Absent),
            (
                Duration::from_millis(250),
                Shape::Up {
                    status: ProcStatus::Starting,
                    pid: NEWCOMER_PID,
                    restarts: 0,
                },
            ),
            (
                Duration::from_millis(450),
                Shape::Up {
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
                    Shape::Up {
                        status: ProcStatus::Online,
                        pid: NEWCOMER_PID,
                        restarts: 0,
                    },
                ),
                (Duration::from_secs(5), Shape::Absent),
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
                (Duration::ZERO, Shape::Absent),
                (
                    Duration::from_millis(250),
                    Shape::Up {
                        status: ProcStatus::Online,
                        pid: NEWCOMER_PID,
                        restarts: 0,
                    },
                ),
                (
                    Duration::from_secs(5),
                    Shape::Up {
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
    fn dies_during_dwell() -> Vec<(Duration, Shape)> {
        vec![
            (Duration::ZERO, Shape::Absent),
            (
                Duration::from_millis(250),
                Shape::Up {
                    status: ProcStatus::Online,
                    pid: NEWCOMER_PID,
                    restarts: 0,
                },
            ),
            (
                Duration::from_secs(5),
                Shape::Up {
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
        cutover_fixture_of(&[7], vec![(Duration::ZERO, Shape::Absent)], None).await
    }

    /// A newcomer that comes up and stays, with every `delete` refused.
    async fn cutover_fixture_comes_up_but_refuses_deletes() -> (CutOverDouble, Prepared, Dirs) {
        let (daemon, prepared, dirs) = cutover_fixture_of(&[7, 8], comes_up(), None).await;
        (daemon.refusing_deletes(), prepared, dirs)
    }

    /// A shepherd that accepts the `Start` and then stops answering.
    ///
    /// Muted from the instant the `Start` lands, which is the worst
    /// version: the roll has been poisoned and every request that could
    /// read or repair it fails.
    async fn cutover_fixture_shepherd_goes_quiet() -> (CutOverDouble, Prepared, Dirs) {
        let (daemon, prepared, dirs) = cutover_fixture_of(&[7], comes_up(), None).await;
        (daemon.going_quiet_after(Duration::ZERO), prepared, dirs)
    }

    /// The port-collision shape, with only the SECOND `delete` refused:
    /// the one that removes the instance the repair `Start` spawned.
    ///
    /// The roll is recorded, so `repaired` is true, and an instance is
    /// still left registered, so `removed` is false. No other fixture
    /// reaches that combination, and it is the one where a cwd-based hint
    /// would point at the wrong instance: the repair `Start` carries the
    /// ORIGINAL config, so what it spawned has the operator's own checkout
    /// as its cwd, exactly like the survivor.
    async fn cutover_fixture_dies_and_refuses_the_second_delete() -> (CutOverDouble, Prepared, Dirs)
    {
        let (daemon, prepared, dirs) = cutover_fixture_of(&[7], dies_during_dwell(), None).await;
        (daemon.refusing_deletes_from(1), prepared, dirs)
    }

    /// The port-collision shape, with every `delete` refused as well.
    async fn cutover_fixture_dies_and_refuses_deletes() -> (CutOverDouble, Prepared, Dirs) {
        let (daemon, prepared, dirs) = cutover_fixture_of(&[7], dies_during_dwell(), None).await;
        (daemon.refusing_deletes(), prepared, dirs)
    }

    /// An original that crashes and respawns a second into the cutover,
    /// with no real newcomer ever arriving.
    ///
    /// The two halves are both needed. The respawn gives the original a pid
    /// the pre-`Start` generation never had, which is all `is_new` looks
    /// at; the absent newcomer is what leaves it as the only candidate.
    async fn cutover_fixture_original_respawns() -> (CutOverDouble, Prepared, Dirs) {
        let (daemon, prepared, dirs) =
            cutover_fixture_of(&[7], vec![(Duration::ZERO, Shape::Absent)], None).await;
        (
            daemon.while_the_originals(originals_respawn_mid_cutover()),
            prepared,
            dirs,
        )
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

    /// fails if a new instance that came up and then died is left
    /// registered. The
    /// likeliest cause is the one the design names: both instances are
    /// alive at once, so without SO_REUSEPORT the new one cannot bind the
    /// port and dies. Leaving it behind gives the operator a permanently
    /// errored second instance of their app and an old one still serving,
    /// with nothing saying which is which.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_dies_during_the_dwell_is_deleted_and_the_old_one_kept() {
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

        let err = cut_over(&daemon, prepared)
            .await
            .expect_err("the dwell catches it");

        // Which failure, not merely that there was one: the fixture would
        // also error if the fake stopped answering, and that would pass a
        // bare `expect_err` while establishing nothing about the dwell.
        assert!(matches!(err, Error::CutOver { .. }), "{err}");
        assert!(err.to_string().contains("did not stay up"), "{err}");
    }

    /// fails if a newcomer that crash-loops through the dwell is accepted.
    /// An app whose release cannot run is restarted by shep, so it is
    /// present at every poll and present at the dwell, under a DIFFERENT
    /// pid each time. Pid identity across the dwell is what catches it, and
    /// `restarts` moving is the second signal.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_crash_loops_through_the_dwell_is_rejected() {
        let (daemon, prepared, _dirs) = cutover_fixture_crash_looping().await;

        let err = cut_over(&daemon, prepared)
            .await
            .expect_err("the pids moved");

        // The pid check, named: a crash-looped newcomer is present at the
        // same count under a different pid, so this is the branch that has
        // to fire rather than the restart count beside it.
        assert!(matches!(err, Error::CutOver { .. }), "{err}");
        assert!(err.to_string().contains("did not stay up"), "{err}");
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

        let err = cut_over(&daemon, prepared).await.expect_err("refused");

        // The shepherd's own refusal, unchanged. An `Error::CutOver` here
        // would be this crate claiming a cutover was abandoned when none
        // was ever begun.
        assert!(matches!(err, Error::Request(_)), "{err}");
        assert!(daemon.deleted().is_empty());
    }

    /// fails if a record that cannot be READ is treated as evidence of
    /// anything. The two branches below give opposite advice and one of
    /// them says to remove the tree, so a guess has no cautious side: a
    /// live, cut-over sheep's working directory is `<tree>/current`, and an
    /// operator who follows that instruction on one deletes a running
    /// service's cwd. Permissions, a hand-edit typo and corruption all
    /// reach here, and none of them is an abandoned cutover.
    ///
    /// It is the only irreversible thing this crate can tell somebody to
    /// do, which is why it refuses rather than picking the branch that
    /// looked safer in the direction it was thought about.
    #[tokio::test]
    async fn a_record_that_cannot_be_read_refuses_rather_than_guessing() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        write_target(home.path(), "bpm", Watch::Auto, Some("old"));
        let tree = Tree::for_sheep(home.path(), "bpm");
        std::fs::write(tree.state_file(), "this is not toml = = =").expect("corrupt the record");
        let entries = [("bpm", checkout.path())];
        let daemon = RollOf(&entries);

        let err = prepare(&daemon, home.path(), "bpm", &test_config())
            .await
            .expect_err("refuses");

        let shown = err.to_string();
        assert!(shown.contains("cannot be read"), "{shown}");
        assert!(
            shown.contains("Do NOT remove the tree"),
            "the irreversible instruction is withheld: {shown}"
        );
        assert!(
            !shown.contains("was never cut over"),
            "it must not claim the cutover was abandoned: {shown}"
        );
    }

    /// fails if a target whose cutover was ABANDONED is pointed at
    /// `shep deploy`. Both situations trip the same `deploy.toml` check,
    /// and they need opposite advice. A target that was cut over serves
    /// from its tree, so `shep deploy` is exactly right. A target whose
    /// cutover was abandoned has served nothing from it, and its record
    /// names no release, so `deploy` does not short-circuit: it builds,
    /// points `current` at the same release, and reloads the sheep BY NAME
    /// at the operator's own checkout. shep replaces that instance from its
    /// own spec, `verify::wait` sees a full pid turnover, and the deploy
    /// prints success for a release nothing ever served. That is the exact
    /// class the engine plan spent five rounds removing, reachable through
    /// one sentence of advice.
    #[tokio::test]
    async fn an_abandoned_target_is_not_pointed_at_shep_deploy() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        write_target(home.path(), "bpm", Watch::Auto, None);
        let entries = [("bpm", checkout.path())];
        let daemon = RollOf(&entries);

        let err = prepare(&daemon, home.path(), "bpm", &test_config())
            .await
            .expect_err("refuses");

        let shown = err.to_string();
        assert!(shown.contains("never cut over"), "{shown}");
        assert!(shown.contains("Do NOT run `shep deploy bpm`"), "{shown}");
        assert!(
            shown.contains("shep-deploy setup bpm"),
            "it says how to try again: {shown}"
        );
    }

    /// fails if a delete the shepherd refused is passed over in silence,
    /// or if only the first failure is reported. The cutover landed and the
    /// new release is serving, so it would be easy to call this cosmetic,
    /// and it is not: what is left behind is an instance the next deploy
    /// does not clean up but RELOADS, respawning it from its own
    /// pre-adoption spec, so it serves the operator's checkout code while
    /// the supervisor keeps it alive. That is verbatim the consequence the
    /// "leave the original running" design was reversed over. The ids have
    /// to reach the operator, all of them, with the command that removes
    /// them.
    ///
    /// The record is still written, because the release really is serving.
    #[tokio::test(start_paused = true)]
    async fn a_refused_delete_names_every_instance_it_left_behind() {
        let (daemon, prepared, _dirs) = cutover_fixture_comes_up_but_refuses_deletes().await;
        let path = prepared.tree.state_file();
        let sha = prepared.sha.clone();

        let err = cut_over(&daemon, prepared).await.expect_err("names them");

        let shown = err.to_string();
        assert!(shown.contains("shep delete 7"), "{shown}");
        assert!(
            shown.contains("shep delete 8"),
            "not just the first one: {shown}"
        );
        assert_eq!(
            State::read(&path).expect("reads").deployed,
            Some(sha),
            "the release is serving, so the record names it"
        );
    }

    /// fails if a cutover ended by a shepherd that went quiet reports the
    /// transport error and nothing else. This is the arm most likely to
    /// leave the roll poisoned and the least able to repair it: it is
    /// reached BECAUSE a request failed, and `undo_start` opens with a
    /// `describe` of its own, so the repair almost always fails too. The
    /// benign case, a release that did not come up with a healthy shepherd,
    /// gets careful `repaired` reporting; this one used to get a bare
    /// socket error saying nothing about the record left behind. The
    /// shepherd's own failure still has to reach the operator, so it is
    /// carried as a source rather than dropped.
    #[tokio::test(start_paused = true)]
    async fn a_shepherd_that_goes_quiet_still_reports_the_repair_it_could_not_make() {
        let (daemon, prepared, _dirs) = cutover_fixture_shepherd_goes_quiet().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        assert!(
            matches!(
                err,
                Error::CutOver {
                    repaired: false,
                    ..
                }
            ),
            "{err}"
        );
        assert!(err.source().is_some(), "the shepherd's own error is kept");
        let shown = err.to_string();
        assert!(shown.contains("stopped answering"), "{shown}");
        assert!(
            shown.contains("muster"),
            "the roll paragraph fires: {shown}"
        );
    }

    /// fails if an abandoned cutover leaves the operator thinking the
    /// target is deployable. `deploy` does not short-circuit on a record
    /// naming no release, so `shep deploy` here reloads the sheep at its
    /// own checkout, sees a real pid turnover, and prints success for a
    /// release nothing served. The failure that produces the false green is
    /// the operator following ordinary advice, so this message has to say
    /// which command NOT to run, not merely omit it.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_cutover_says_the_target_is_not_deployable_yet() {
        let (daemon, prepared, _dirs) = cutover_fixture_dies_during_dwell().await;
        let tree = prepared.tree.root().to_owned();

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        let shown = err.to_string();
        assert!(shown.contains("is NOT a deploy target"), "{shown}");
        assert!(shown.contains("Do NOT run `shep deploy bpm`"), "{shown}");
        assert!(shown.contains("shep-deploy setup bpm"), "{shown}");
        // Resolved, not `$SHEP_HOME/deploy/bpm`. That variable is usually
        // unset in an operator's own shell, where the fallback is `~/.shep`,
        // so the unexpanded form makes `rm -rf` target `/deploy/bpm` and do
        // nothing at all. The remedy is the whole point of this message.
        assert!(
            shown.contains(&format!("remove {}", tree.display())),
            "{shown}"
        );
        assert!(!shown.contains("$SHEP_HOME"), "{shown}");
    }

    /// fails if a cutover that never spawned anything blames the port. The
    /// SO_REUSEPORT paragraph is the likeliest cause of a newcomer that
    /// came up and died, and it is impossible for one the shepherd accepted
    /// and never produced a row for: nothing ever got as far as binding.
    /// An operator sent after a port that was never contended is an
    /// operator who stops reading at the first plausible sentence.
    #[tokio::test(start_paused = true)]
    async fn a_cutover_that_spawned_nothing_does_not_blame_the_port() {
        let (daemon, prepared, _dirs) = cutover_fixture_never_appears().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        let shown = err.to_string();
        assert!(!shown.contains("SO_REUSEPORT"), "{shown}");
        assert!(shown.contains("No new instance appeared"), "{shown}");
    }

    /// fails if a cutover that never started anything says it removed
    /// something. With this fixture the shepherd accepts the `Start` and
    /// produces no row at all, so the first drain finds nothing and
    /// succeeds vacuously - and the sentence after `why` used to read "The
    /// instance it added has been removed" directly beneath "No new
    /// instance appeared". Two adjacent sentences contradicting each other
    /// is how an operator decides a message is boilerplate and stops
    /// reading it, which matters because the paragraph after them is the
    /// one that keeps them off `shep deploy`.
    #[tokio::test(start_paused = true)]
    async fn a_cutover_that_added_nothing_does_not_claim_to_have_removed_it() {
        let (daemon, prepared, _dirs) = cutover_fixture_never_appears().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        let shown = err.to_string();
        assert!(shown.contains("No new instance appeared"), "{shown}");
        assert!(
            !shown.contains("instance it added has been removed"),
            "it cannot have removed what it never added: {shown}"
        );
        assert!(
            shown.contains("Nothing this cutover started is left registered"),
            "{shown}"
        );
    }

    /// fails if a repair instance that could not be deleted is described as
    /// the one under the deploy tree. `undo_start` drains twice and only
    /// the first drain removes something started from the deploy tree: the
    /// repair `Start` re-registers the ORIGINAL config, so the instance it
    /// spawns has the operator's own checkout as its cwd, identical to the
    /// survivor's. A hint keyed on cwd is then wrong, and wrong in the
    /// direction that has the operator hunting for a row that does not
    /// exist while a duplicate of their app keeps running.
    ///
    /// `removed: false` with `repaired: true` is also a combination no
    /// other fixture produces.
    #[tokio::test(start_paused = true)]
    async fn a_repair_instance_left_behind_is_not_described_by_its_cwd() {
        let (daemon, prepared, _dirs) = cutover_fixture_dies_and_refuses_the_second_delete().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        assert!(
            matches!(
                err,
                Error::CutOver {
                    removed: false,
                    repaired: true,
                    ..
                }
            ),
            "{err}"
        );
        let shown = err.to_string();
        assert!(
            !shown.contains("cwd under the deploy tree"),
            "the hint would point at the wrong instance: {shown}"
        );
        assert!(shown.contains("shep delete <id>"), "{shown}");
    }

    /// fails if an abandoned cutover claims to have removed an instance it
    /// could not remove. `undo_start` swallows each failed delete so the
    /// operator gets the reason they are here rather than a second error
    /// about the cleanup, which makes this message the only place a failed
    /// one is ever mentioned. An operator told the newcomer is gone has no
    /// reason to look for it, and what is left behind is a second instance
    /// of their app running the release that was just rejected.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_could_not_be_removed_is_named_not_claimed_gone() {
        let (daemon, prepared, _dirs) = cutover_fixture_dies_and_refuses_deletes().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        let shown = err.to_string();
        assert!(shown.contains("could NOT be removed"), "{shown}");
        assert!(shown.contains("shep describe bpm"), "{shown}");
    }

    /// fails if the ORIGINAL, respawned, is mistaken for the release this
    /// cutover just started. `Generation::is_new` compares pids and the
    /// generation is captured before the `Start`, so an original that
    /// crashes and comes back under a new pid looks exactly like a
    /// newcomer: phase one adopts it, and the dwell finds the same pid
    /// still alive. Only the restart count is left to reject it, which is
    /// why that check is load-bearing rather than belt and braces.
    ///
    /// What is at stake is the whole of this task. Accept the respawned
    /// original and the cutover reports `Done` having verified nothing,
    /// then deletes that healthy original by id, leaving the sheep with no
    /// instance at all. And the shape is the premise of the task rather
    /// than a corner: the newcomer is fighting the original for its port.
    #[tokio::test(start_paused = true)]
    async fn an_original_that_respawns_is_not_mistaken_for_the_newcomer() {
        let (daemon, prepared, _dirs) = cutover_fixture_original_respawns().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        assert!(matches!(err, Error::CutOver { .. }), "{err}");
        // The restart count, specifically. The pid check cannot fire here:
        // the row the dwell sees is the one phase one adopted.
        assert!(err.to_string().contains("already restarted"), "{err}");
        assert!(
            !daemon.deleted().contains(&7),
            "the healthy original is kept: {:?}",
            daemon.deleted()
        );
    }

    /// fails if the record is advanced before the newcomer verified.
    /// `deploy.toml` naming a release nothing has served is the same defect
    /// the engine plan spent five rounds removing from the deploy path, and
    /// it must not come back through this one.
    #[tokio::test(start_paused = true)]
    async fn the_record_advances_only_after_the_newcomer_is_online() {
        let (daemon, prepared, _dirs) = cutover_fixture_never_appears().await;
        let path = prepared.tree.state_file();

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        assert!(matches!(err, Error::CutOver { .. }), "{err}");
        assert_eq!(State::read(&path).expect("reads").deployed, None);
    }

    /// fails if a tree the cutover has not landed on is left watched. The
    /// poll loop deploys every `auto` target it finds, and a deploy against
    /// such a tree is refused - so a target abandoned between `prepare` and
    /// `cut_over` would be refused once every interval, forever, for a
    /// target the loop should be ignoring outright. `prepare` writes
    /// `manual` and the cutover promotes it, which is the same rule the
    /// docs already state: a sheep is not a deploy target until a cutover
    /// lands.
    #[tokio::test(start_paused = true)]
    async fn a_prepared_target_is_not_watched_until_the_cutover_lands() {
        let (daemon, prepared, _dirs) = cutover_fixture().await;
        let path = prepared.tree.state_file();

        assert_eq!(
            State::read(&path).expect("reads").watch,
            Watch::Manual,
            "prepared, and nothing has served from the tree yet"
        );

        cut_over(&daemon, prepared).await.expect("cuts over");

        assert_eq!(
            State::read(&path).expect("reads").watch,
            Watch::Auto,
            "the cutover landed, so the poll loop may have it"
        );
    }

    /// fails if a cutover that was abandoned leaves the target watched.
    /// This is the tree nobody ever served from, and the loop must pass
    /// over it rather than meeting `deploy`'s refusal every interval.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_cutover_leaves_the_target_unwatched() {
        let (daemon, prepared, _dirs) = cutover_fixture_never_appears().await;
        let path = prepared.tree.state_file();

        cut_over(&daemon, prepared).await.expect_err("gives up");

        assert_eq!(State::read(&path).expect("reads").watch, Watch::Manual);
    }

    /// fails if a cutover that landed and could not tidy up starts the poll
    /// loop off on its own. The release IS serving, so the record names it -
    /// but instances the cutover could not delete are still registered, and
    /// every later deploy reloads the name and respawns each of them from
    /// its own pre-adoption spec. Unattended, that is the leftover coming
    /// back on a schedule. The operator removes the ids the error names and
    /// then asks for `--watch auto`.
    #[tokio::test(start_paused = true)]
    async fn a_stranded_cutover_does_not_start_the_loop_off_on_its_own() {
        let (daemon, prepared, _dirs) = cutover_fixture_comes_up_but_refuses_deletes().await;
        let path = prepared.tree.state_file();

        let err = cut_over(&daemon, prepared).await.expect_err("is stranded");

        assert!(matches!(err, Error::Stranded { .. }), "{err}");
        let state = State::read(&path).expect("reads");
        assert!(state.deployed.is_some(), "the release is serving");
        assert_eq!(state.watch, Watch::Manual);
    }

    /// fails if a cutover that DID land stops recording what it landed, or
    /// answers with a sha that is not the one under `current`. The negative
    /// is pinned next door and the positive was not, so both lines writing
    /// the record could be deleted with the suite still green - and the
    /// state they leave, a cut-over sheep whose record names no release, is
    /// what makes `shep deploy` report success for a release nothing served.
    #[tokio::test(start_paused = true)]
    async fn the_record_and_the_answer_name_the_release_that_was_cut_over() {
        let (daemon, prepared, _dirs) = cutover_fixture().await;
        let path = prepared.tree.state_file();
        let expected = prepared.sha.clone();

        let sha = cut_over(&daemon, prepared).await.expect("cuts over");

        assert_eq!(sha, expected, "the sha it answers with");
        assert_eq!(State::read(&path).expect("reads").deployed, Some(expected));
    }
}
