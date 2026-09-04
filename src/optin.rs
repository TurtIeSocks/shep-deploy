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
use crate::git;
use crate::lock;
use crate::paths::Tree;
use crate::roll;
use crate::shared;
use crate::state::{State, Verify, Watch};
use crate::swap;
use crate::verify::{self, DWELL, Generation, POLL, is_alive};

/// Everything opt-in built, ready for a cutover to register and swap into
/// place.
///
/// Carries the tree's exclusive hold as well, so the lock [`prepare`] took
/// is still held while [`cut_over`] registers, deletes and writes the
/// record. Released between the two, a second `setup` in that window read a
/// record with no `deployed` and told the operator to remove a tree a live
/// cutover was using.
#[derive(Debug)]
pub struct Prepared {
    /// The tree's exclusive hold, kept for the cutover. See the type doc.
    hold: lock::Deploying,
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
/// [`crate::deploy::stage_release`] (the worktree, the cache link, the shared
/// files and both Flockfiles),
/// [`build::run`] or [`swap::point_at`] return.
pub async fn prepare<D: Daemon>(
    daemon: &D,
    shep_home: &Path,
    sheep: &str,
    config: &DogConfig,
) -> Result<Prepared, Error> {
    let tree = Tree::for_sheep(shep_home, sheep);

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
    // The shepherd already runs the sheep from this tree's `current`: a
    // cutover landed and its record was not written, or the record was lost
    // afterwards. Either way the tree is live, and the advice the record
    // branch below gives an unrecorded tree, remove it and run setup again,
    // would take a running service's cwd with it. The record is repaired
    // from what `current` names instead, and the sheep is then what it is:
    // a deploy target, refused here like any other.
    if shared::same_path(&checkout, &tree.current())
        && let Some(release) = swap::resolve(&tree.current())?
        && let Some(sha) = release.file_name().and_then(|name| name.to_str())
        && crate::state::is_sha(sha)
        && release.is_dir()
    {
        // The tree exists in this case, so the hold litters nothing, and the
        // repair below writes the record under it.
        let _deploying = lock::hold(&tree)?;
        let _record = lock::hold_record(&tree)?;
        let next = format!(
            "Deploy it with `shep deploy {sheep}`, or change how it is watched with `shep \
             deploy {sheep} --watch auto|manual`."
        );
        let record = match State::read(&tree.state_file()) {
            Ok(mut state @ State { deployed: None, .. }) => {
                state.deployed = Some(sha.to_owned());
                state.write(&tree.state_file())?;
                format!(
                    "Its record named no deployed release and has been brought up to date \
                     with that. {next}"
                )
            }
            Ok(state) if state.deployed.as_deref() == Some(sha) => {
                format!("Its record agrees. {next}")
            }
            Ok(State {
                deployed: Some(other),
                ..
            }) => format!(
                "Its record names {} instead, which is what a deploy whose record could not \
                 be written leaves; the next deploy corrects it. {next}",
                &other[..7.min(other.len())]
            ),
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                "Its record is missing, and this dog cannot rebuild the origin fields removal \
                 needs: write a deploy.toml by hand from another target's as a model, with \
                 `deployed` set to that sha, before deploying."
                    .to_owned()
            }
            Err(_) => "Its record cannot be read: repair it before deploying, and do NOT \
                       remove the tree, the service runs from inside it."
                .to_owned(),
        };
        return Err(Error::Config(format!(
            "{sheep} is already a deploy target: the shepherd runs it from {}, which names {}. \
             {record}",
            tree.current().display(),
            &sha[..7]
        )));
    }

    // A cwd anywhere else inside this sheep's own deploy tree is what an
    // abandoned cutover leaves in the shepherd's record once the tree's own
    // record is gone, and capturing it as the origin would make removal put
    // the sheep back INTO the tree. Refused by name rather than left to
    // `current_branch`, which happened to refuse it only because releases
    // are detached worktrees.
    if shared::resolved(&checkout).starts_with(shared::resolved(tree.root())) {
        return Err(Error::Config(format!(
            "{sheep} is registered with its working directory inside its own deploy tree ({}), \
             so there is no operator checkout to take it over from. That is what an abandoned \
             cutover leaves in the shepherd's record: re-register {sheep} from its own \
             Flockfile in the checkout it should deploy from, then run setup again",
            checkout.display()
        )));
    }

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
        // The whole definition, so removal puts back `env`, `instances`,
        // the probes and everything else the operator had, not the two
        // fields above alone.
        origin: Some(previous_config.clone()),
    };
    // Checked here, before the fetch, the worktree and the build, rather
    // than found out by the record write at the very end. Neither value was
    // typed by the operator: `remote` is what `git remote get-url origin`
    // answered in the checkout and `checkout` is the cwd shep registered.
    state.validate(Path::new("deploy.toml")).map_err(|err| {
        Error::Config(format!(
            "{sheep} cannot be recorded as it is registered ({err}). `remote` is the \
                 checkout's `origin`, `branch` is the checkout's HEAD, and `checkout` is the cwd \
                 shep has for {sheep}; fix whichever is named and run setup again"
        ))
    })?;

    // Same hold a deploy takes, for the same reason: everything below writes
    // to this tree, and a poll tick can be doing the same at the same moment.
    // After every refusal that needs no tree, so a `setup` that fails on a
    // detached HEAD or a missing `origin` leaves nothing behind: `hold` makes
    // the tree's root itself. Carried out in `Prepared` so the cutover runs
    // under it too.
    let hold = lock::hold(&tree)?;

    std::fs::create_dir_all(tree.releases()).map_err(Error::at(tree.releases()))?;
    git::init_bare(&tree.git())?;
    // Off the runtime's thread, like every git call: see `shared::off_thread`.
    let (git_dir, remote, budget) = (tree.git(), state.remote.clone(), config.git_timeout);
    shared::off_thread(move || git::fetch(&git_dir, &remote, budget)).await?;
    let sha = git::remote_head(&tree.git(), &state.branch)?;

    let release = tree.release(&sha);
    // Shared with `deploy::attempt`, and that is what makes this function's
    // own retry story true: `stage_release` goes through `checkout_release`,
    // which reuses a finished checkout and replaces a partial one. `git
    // worktree add` alone refuses a path that already exists ("fatal:
    // `<path>` already exists") and refuses one it still has registered
    // after the directory was removed ("missing but already registered
    // worktree"), so every run that died anywhere from here onward left a
    // release directory that made the next run fail on git rather than
    // resume, which is the opposite of what the doc above promises.
    let (app, spec) = {
        let (tree, checkout, sha) = (tree.clone(), state.checkout.clone(), sha.clone());
        shared::off_thread(move || crate::deploy::stage_release(&tree, &checkout, &sha)).await?
    };
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
        hold,
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
        hold,
        tree,
        mut state,
        sha,
        app,
        previous_config,
    } = prepared;
    // Held to the end of this function, and named so it is not dropped here.
    let _hold = hold;
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
    // The interpreter the sheep already runs under, when the Flockfile names
    // none. `shep start` fills an unset `interpreter` from `shep.toml`'s
    // `[interpreters]` by the script's extension, and a fresh home maps
    // `js`, `py`, `rb`, `sh` and more out of the box; the shepherd itself
    // never does, and runs a script with no interpreter directly. This dog
    // cannot read that map, so a Flockfile that relies on it would have
    // been registered here with none and the replacement exec'd `server.js`
    // as a program. The registered value IS the map's answer for this
    // sheep, so it is carried. An explicit value, `"none"` included,
    // outranks the map for the CLI and is left alone here for the same
    // reason. Read out of shep-cli's `apply_interpreters` on 2026-09-04.
    if registering.interpreter.is_none() {
        registering.interpreter = previous_config.interpreter.clone();
    }

    match attempt(daemon, &sheep, registering, &generation, &previous).await {
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
            let outcome = if stranded.is_empty() {
                Ok(sha)
            } else {
                Err(Error::Stranded {
                    sheep: sheep.clone(),
                    sha,
                    ids: stranded,
                })
            };
            // The ids outrank the write. A record that could not be written
            // costs one more `shep deploy`; an id that never reached the
            // operator costs an instance respawned on the old config on every
            // deploy from now on, so the write's failure is printed rather
            // than allowed to replace the error that names them.
            let written =
                lock::hold_record(&tree).and_then(|_record| state.write(&tree.state_file()));
            if let Err(err) = written {
                if outcome.is_ok() {
                    return Err(err);
                }
                eprintln!("shep-deploy: {sheep}: could not record the cutover: {err}");
            }
            outcome
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

/// Whether a newcomer's row says it is dead.
///
/// The same answer with or without a pid: a row the shepherd has accepted
/// and not yet spawned has no pid and says `Starting`, which [`is_alive`]
/// accepts, and every other status without a pid is a process on its way
/// out or already gone. One predicate rather than two hand-kept complements
/// of `ProcStatus`, so a status added upstream is judged one way.
fn died(info: &ProcessInfo) -> bool {
    !is_alive(info)
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
/// `crate::deploy`'s per-instance product: a cutover's `Start` spawns its
/// instances together and drains nothing, where a reload replaces every
/// instance one at a time and has to wait out each drain.
fn cutover_budget(app: &AppConfig) -> Duration {
    // Saturating and capped, for the reason `crate::deploy`'s `budget` gives:
    // `listen_timeout` is the repository's number, not the operator's.
    verify::bounded(
        app.listen_timeout
            .as_duration()
            .saturating_add(RELOAD_DEADLINE_SLACK),
    )
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
    previous: &[u32],
) -> CutOver {
    let patience = cutover_budget(&app);
    // How many the `Start` spawns. A scaled sheep spawns them together, and
    // freezing the generation on the first to appear verified a subset and
    // then deleted every original: the half-strength defect
    // `Generation::has_turned_over` guards against on the deploy path.
    let wanted = usize::try_from(app.instances.max(1)).unwrap_or(1);

    if let Err(source) = daemon.start(vec![app]).await {
        return CutOver::NotStarted(source);
    }
    let started_at = Instant::now();
    let deadline = started_at + patience;

    // Phase one: every newcomer exists and none has already failed.
    //
    // A newcomer is a row whose id the flock did not have before the
    // `Start`. By id rather than by pid, because an instance that died hard
    // has no pid and a pid-only reading made it invisible: the loop then
    // burned the whole budget and reported that nothing appeared, which is
    // the one message that deliberately withholds the port-collision hint,
    // for a port collision. An original that crashed and respawned keeps its
    // id and is not a newcomer, whatever its pid.
    let arrived = loop {
        let flock = match daemon.describe(sheep).await {
            Ok(flock) => flock,
            Err(source) => return CutOver::Failed(source),
        };

        let newcomers: Vec<&ProcessInfo> = flock
            .iter()
            .filter(|info| !previous.contains(&info.id))
            .collect();

        // A row with a pid is judged by `is_alive`. A row without one is
        // judged only by a status that says it died (`Errored`,
        // `WaitingRestart`): a row the shepherd has accepted and not yet
        // spawned has no pid either, and whatever status it carries in that
        // instant is not a verdict.
        if newcomers.iter().any(|info| died(info)) {
            return CutOver::NotVerified(format!(
                "The new instance failed before it finished starting. {PORT_COLLISION}"
            ));
        }
        // Up, with a process: the dwell below compares pids.
        let up: Vec<&ProcessInfo> = newcomers
            .iter()
            .copied()
            .filter(|info| before.is_new(info))
            .collect();
        if up.len() >= wanted {
            break Generation::of_infos(&up);
        }
        if Instant::now() >= deadline {
            return CutOver::NotVerified(if up.is_empty() {
                format!(
                    "No new instance appeared within {}s, although the shepherd accepted the \
                     start.",
                    started_at.elapsed().as_secs()
                )
            } else {
                format!(
                    "Only {} of the {wanted} instances the start asked for appeared within \
                     {}s. {PORT_COLLISION}",
                    up.len(),
                    started_at.elapsed().as_secs()
                )
            });
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
    let seen: Vec<u32> = flock.iter().map(|info| info.id).collect();
    let mut removed = drain(daemon, &flock, previous).await;

    let patience = cutover_budget(&original);
    if daemon.start(vec![original]).await.is_err() {
        return Undone {
            removed,
            recorded: false,
        };
    }

    // The repair `Start` spawned an instance, and a `Start` is an acceptance:
    // the shepherd spawns afterwards. Waited for, the way phase one of
    // `attempt` waits, because describing at once found nothing, reported
    // everything as removed, and left a second instance of the app coming up
    // on the original config to fight the survivor for its port.
    let deadline = Instant::now() + patience;
    loop {
        let Ok(flock) = daemon.describe(sheep).await else {
            // The roll IS re-recorded: the start was accepted. What is not
            // known is whether the instance it spawned is gone, and it is
            // reported as not, which is the answer that sends the operator
            // to look.
            return Undone {
                removed: false,
                recorded: true,
            };
        };
        // A row that is new to both the pre-cutover flock AND the listing the
        // first drain worked from. A newcomer that drain could not delete is
        // in the second and must not pass for the repair instance, or the
        // wait ends before that instance appears and it is left running.
        if flock
            .iter()
            .any(|info| !previous.contains(&info.id) && !seen.contains(&info.id))
        {
            removed &= drain(daemon, &flock, previous).await;
            break;
        }
        if Instant::now() >= deadline {
            removed = false;
            break;
        }
        sleep(POLL).await;
    }

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
///
/// "Not in `previous`" is "not seen before the cutover's `Start`", which is
/// a wider net than "added by this cutover": an instance an operator started
/// by hand inside the window goes with it. That window is a few seconds
/// under a lock the operator's own commands respect, so the wider net is
/// accepted rather than narrowed by guessing which row was whose.
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
        async fn save_roll(&self) -> Result<PathBuf, Error> {
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
            // Written inside the first checkout rather than into a tempdir
            // that has to be kept alive past this call and was therefore
            // never removed, one per `prepare`. The checkout is a test's own
            // tempdir and goes with it; an untracked file in it is not
            // ignored, so nothing links it into a release.
            let (_, cwd) = self.0.first().expect("a roll names at least one sheep");
            let path = cwd.join(".flock-roll.json");
            std::fs::write(&path, format!("{{\"apps\":[{}]}}", apps.join(",")))
                .expect("write roll");
            Ok(path)
        }

        crate::fixtures::daemon_methods!(unimplemented;
            dog_config, list_flock, describe, start, delete, reload, restart, set_smit,
        );
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
        checkout_declaring("[[app]]\nname = \"bpm\"\nscript = \"./run.sh\"\n")
    }

    /// As [`checkout_with_commit`], with the Flockfile's text given.
    fn checkout_declaring(flockfile: &str) -> tempfile::TempDir {
        let dir = fixtures::checkout(&[
            ("Flockfile.toml", flockfile),
            ("run.sh", "#!/bin/sh\necho hi\n"),
        ]);
        // Itself, so `git::remote_url` has an answer. Nothing fetches from it.
        fixtures::run_git(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                dir.path().to_str().expect("utf-8 tempdir path"),
            ],
        );
        dir
    }

    /// Writes `<home>/deploy/<sheep>/deploy.toml` through `State::write`,
    /// standing in for a sheep that is already a deploy target.
    fn write_target(home: &Path, sheep: &str, watch: Watch, sha: Option<&str>) {
        let tree = Tree::for_sheep(home, sheep);
        std::fs::create_dir_all(tree.state_file().parent().expect("has a parent"))
            .expect("create target dir");
        let state = State {
            deployed: sha.map(str::to_owned),
            watch,
            ..fixtures::state()
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
        write_target(
            home.path(),
            "bpm",
            Watch::Auto,
            Some("0123456789abcdef0123456789abcdef01234567"),
        );
        let entries = [("bpm", checkout.path())];
        let daemon = RollOf(&entries);

        let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
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

        let prepared = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
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

        let prepared = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
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

        let first = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
            .await
            .expect("prepares");
        let (first_sha, state_file) = (first.sha.clone(), first.tree.state_file());
        // The run died, so its hold on the tree died with it: `Prepared`
        // carries the lock, and a second run against a held tree is refused
        // for the reason `crate::lock` gives, which is not the case here.
        drop(first);
        std::fs::remove_file(&state_file).expect("the record a kill never wrote");

        let again = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
            .await
            .expect("a tree with no record must be resumable, as the doc says");
        assert_eq!(again.sha, first_sha);
        assert!(
            state_file.is_file(),
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

        let prepared = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
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

        let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
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
        /// At most this many newcomers appear, whatever the `Start` asked
        /// for: a shepherd that spawned one of two and no more.
        newcomers_at_most: Option<u32>,
        /// How many instances the repair `Start`s have spawned.
        repairs: Cell<u32>,
        /// When the first repair `Start` was accepted; its instance appears
        /// [`SPAWN_LAG`] after, as a real shepherd's would.
        repaired_at: Cell<Option<Instant>>,
    }

    /// How long a shepherd takes to spawn after accepting a `Start`, as the
    /// double plays it.
    const SPAWN_LAG: Duration = Duration::from_millis(250);

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
                newcomers_at_most: None,
                repairs: Cell::new(0),
                repaired_at: Cell::new(None),
            }
        }

        /// The same double, spawning at most `n` newcomers however many the
        /// `Start` asked for.
        fn spawning_at_most(mut self, n: u32) -> Self {
            self.newcomers_at_most = Some(n);
            self
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
                    daemon_version: None,
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
                // As many as the accepted `Start` asked for, which is what a
                // shepherd spawns; the originals' count is a different
                // number and using it hid the multi-instance wait entirely.
                let asked = self
                    .starts
                    .borrow()
                    .first()
                    .map_or(1, |app| app.instances.max(1));
                let spawned = self.newcomers_at_most.map_or(asked, |cap| asked.min(cap));
                for offset in 0..spawned {
                    let id = NEWCOMER_ID - offset;
                    if !self.is_deleted(id) {
                        flock.push(self.row(id, status, pid + offset, restarts));
                    }
                }
            }

            // Not at once. `undo_start` used to describe right after the
            // repair `Start` and find nothing, and a double that showed the
            // row instantly hid that.
            let repair_spawned = self
                .repaired_at
                .get()
                .is_some_and(|at| Instant::now() - at >= SPAWN_LAG);
            if repair_spawned {
                for offset in 0..self.repairs.get() {
                    let id = REPAIR_ID + offset;
                    if !self.is_deleted(id) {
                        flock.push(self.row(id, ProcStatus::Online, REPAIR_PID + offset, 0));
                    }
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
                    daemon_version: None,
                })));
            }

            self.starts.borrow_mut().extend(apps);
            if attempt == 0 {
                self.accepted_at.set(Some(Instant::now()));
            } else {
                self.repairs.set(self.repairs.get() + 1);
                if self.repaired_at.get().is_none() {
                    self.repaired_at.set(Some(Instant::now()));
                }
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
                    daemon_version: None,
                })));
            }
            self.deletes.borrow_mut().push(id);
            Ok(())
        }

        crate::fixtures::daemon_methods!(unimplemented;
            dog_config, list_flock, reload, restart, save_roll, set_smit,
        );
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
        cutover_fixture_from(checkout_with_commit(), originals, script, refuses).await
    }

    /// As [`cutover_fixture_of`], from a checkout the test built itself.
    async fn cutover_fixture_from(
        checkout: tempfile::TempDir,
        originals: &[u32],
        script: Vec<(Duration, Shape)>,
        refuses: Option<usize>,
    ) -> (CutOverDouble, Prepared, Dirs) {
        let home = tempfile::tempdir().expect("tempdir");
        let entries = [("bpm", checkout.path())];
        let roll = RollOf(&entries);
        let prepared = prepare(&roll, home.path(), "bpm", &fixtures::dog_config())
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

    /// fails if a Flockfile that names no `interpreter` is registered with
    /// none. `shep start` filled the sheep's from `shep.toml`'s
    /// `[interpreters]` by extension, the shepherd itself never does, and
    /// this dog cannot read the map: registered bare, the replacement would
    /// exec `server.js` as a program. An explicit value, `"none"` included,
    /// is the Flockfile's own and stays.
    #[tokio::test(start_paused = true)]
    async fn the_new_registration_keeps_the_interpreter_the_sheep_ran_under() {
        let (daemon, mut prepared, _dirs) = cutover_fixture().await;
        prepared.previous_config.interpreter = Some("node".to_owned());
        prepared.app.interpreter = None;
        cut_over(&daemon, prepared).await.expect("cuts over");
        assert_eq!(daemon.started()[0].interpreter.as_deref(), Some("node"));

        let (daemon, mut prepared, _dirs) = cutover_fixture().await;
        prepared.previous_config.interpreter = Some("node".to_owned());
        prepared.app.interpreter = Some("none".to_owned());
        cut_over(&daemon, prepared).await.expect("cuts over");
        assert_eq!(daemon.started()[0].interpreter.as_deref(), Some("none"));
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

    /// fails if a sheep running two instances loses only one of them at
    /// the cutover. Every original is deleted by id once the newcomer is
    /// verified, so a cutover that deleted a single id would leave the rest
    /// serving the pre-adoption checkout indefinitely. The Flockfile here
    /// asks for one instance; the scaled cutover has its own tests.
    #[tokio::test(start_paused = true)]
    async fn every_replaced_instance_is_deleted() {
        let (daemon, prepared, _dirs) = cutover_fixture_with_instances(&[7, 8]).await;
        cut_over(&daemon, prepared).await.expect("cuts over");
        assert_eq!(daemon.deleted(), vec![7, 8]);
    }

    /// fails if a pidless newcomer is judged by anything but its status.
    /// A row the shepherd has accepted and not yet spawned has no pid and
    /// says `Starting`; one that died hard has no pid and says `Errored`.
    /// The first is waited for and the second ends the cutover.
    #[test]
    fn a_pidless_newcomer_is_dead_only_when_its_status_says_so() {
        let row = |status| ProcessInfo::builder(99, "bpm", status).build();
        for alive in [ProcStatus::Starting, ProcStatus::Online] {
            assert!(
                !died(&row(alive)),
                "{alive:?} without a pid is not yet a verdict"
            );
        }
        for dead in [
            ProcStatus::Errored,
            ProcStatus::WaitingRestart,
            ProcStatus::Stopped,
            ProcStatus::Stopping,
        ] {
            assert!(died(&row(dead)), "{dead:?} without a pid is dead");
        }
        let with_pid = ProcessInfo::builder(99, "bpm", ProcStatus::Stopping)
            .pid(Some(4242))
            .build();
        assert!(
            died(&with_pid),
            "with a pid the status decides the same way"
        );
    }

    /// fails if the record stops carrying the app as the shepherd had it
    /// before adoption. Removal restores from this; `cwd` and `script` alone
    /// left the deployed repository's `env`, instances and probes in place.
    #[tokio::test]
    async fn the_record_carries_the_whole_pre_adoption_definition() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        let entries = [("bpm", checkout.path())];
        let daemon = RollOf(&entries);

        let prepared = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
            .await
            .expect("prepares");

        let origin = prepared.state.origin.as_ref().expect("recorded");
        assert_eq!(origin, &prepared.previous_config);
        let on_disk = State::read(&prepared.tree.state_file()).expect("reads");
        assert_eq!(on_disk.origin.as_ref(), Some(&prepared.previous_config));
    }

    /// fails if a sheep the shepherd already runs from `current` is treated
    /// as one to take over. That is what a cutover that landed and could not
    /// write its record leaves, and the old refusal told the operator to
    /// remove the tree the service runs from. The record is repaired from
    /// what `current` names and the sheep is refused as the target it is.
    #[tokio::test]
    async fn a_sheep_already_running_from_current_has_its_record_repaired() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "bpm");
        let release = tree.release(fixtures::SHA);
        std::fs::create_dir_all(&release).expect("a release directory");
        swap::point_at(&tree.current(), &release).expect("current");
        write_target(home.path(), "bpm", Watch::Manual, None);
        let current = tree.current();
        let entries = [("bpm", current.as_path())];
        let daemon = RollOf(&entries);

        let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
            .await
            .expect_err("already a target");

        let shown = err.to_string();
        assert!(shown.contains("already a deploy target"), "{shown}");
        assert!(shown.contains("brought up to date"), "{shown}");
        assert!(
            !shown.contains("Remove"),
            "must never say to remove the tree: {shown}"
        );
        assert_eq!(
            State::read(&tree.state_file())
                .expect("reads")
                .deployed
                .as_deref(),
            Some(fixtures::SHA),
            "the record now names what current names"
        );
    }

    /// fails if a second spelling of the tree's `current` is not recognised
    /// as the tree's `current`. The daemon hands back the cwd as it was
    /// registered, and it can spell `$SHEP_HOME` through a symlink the dog
    /// does not use. A literal comparison then skipped this branch, found
    /// the record with no `deployed`, and told the operator to remove the
    /// tree a running service was serving from.
    #[tokio::test]
    async fn a_sheep_running_from_current_spelled_through_a_link_is_still_recognised() {
        let root = tempfile::tempdir().expect("tempdir");
        let home = root.path().join("home");
        std::fs::create_dir_all(&home).expect("home");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&home, &link).expect("a second spelling");
        let tree = Tree::for_sheep(&home, "bpm");
        let release = tree.release(fixtures::SHA);
        std::fs::create_dir_all(&release).expect("a release directory");
        swap::point_at(&tree.current(), &release).expect("current");
        write_target(&home, "bpm", Watch::Manual, None);
        let spelled_through_link = link.join("deploy/bpm/current");
        let entries = [("bpm", spelled_through_link.as_path())];
        let daemon = RollOf(&entries);

        let err = prepare(&daemon, &home, "bpm", &fixtures::dog_config())
            .await
            .expect_err("already a target");

        let shown = err.to_string();
        assert!(shown.contains("already a deploy target"), "{shown}");
        assert!(
            !shown.contains("Remove"),
            "must never say to remove the tree: {shown}"
        );
    }

    /// fails if the same sheep with NO record at all is told anything but
    /// that the record is missing. Nothing can rebuild the origin fields, so
    /// the operator writes the file; the tree still must not be removed.
    #[tokio::test]
    async fn a_sheep_running_from_current_with_no_record_is_told_to_write_one() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "bpm");
        let release = tree.release(fixtures::SHA);
        std::fs::create_dir_all(&release).expect("a release directory");
        swap::point_at(&tree.current(), &release).expect("current");
        let current = tree.current();
        let entries = [("bpm", current.as_path())];
        let daemon = RollOf(&entries);

        let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
            .await
            .expect_err("already a target");

        let shown = err.to_string();
        assert!(shown.contains("record is missing"), "{shown}");
        assert!(!shown.contains("Remove"), "{shown}");
    }

    /// fails if a scaled sheep's cutover settles for fewer newcomers than
    /// the `Start` asked for. With `instances = 2` and a shepherd that
    /// spawned one, the cutover used to freeze its generation on that one,
    /// verify it, and delete both originals: the sheep came back at half
    /// strength with the deploy reported landed.
    #[tokio::test(start_paused = true)]
    async fn a_scaled_cutover_waits_for_every_instance_the_start_asked_for() {
        let checkout =
            checkout_declaring("[[app]]\nname = \"bpm\"\nscript = \"./run.sh\"\ninstances = 2\n");
        let (daemon, prepared, _dirs) =
            cutover_fixture_from(checkout, &[7, 8], comes_up(), None).await;
        let daemon = daemon.spawning_at_most(1);

        let err = cut_over(&daemon, prepared)
            .await
            .expect_err("one of two is not a cutover");

        let shown = err.to_string();
        assert!(shown.contains("Only 1 of the 2"), "{shown}");
        assert!(
            !daemon.deleted().contains(&7) && !daemon.deleted().contains(&8),
            "the originals are kept: {:?}",
            daemon.deleted()
        );
    }

    /// fails if a scaled sheep whose newcomers all appear is not cut over.
    /// The other half of the test above: two asked for, two spawned, both
    /// originals replaced.
    #[tokio::test(start_paused = true)]
    async fn a_scaled_cutover_lands_when_every_instance_appears() {
        let checkout =
            checkout_declaring("[[app]]\nname = \"bpm\"\nscript = \"./run.sh\"\ninstances = 2\n");
        let (daemon, prepared, _dirs) =
            cutover_fixture_from(checkout, &[7, 8], comes_up(), None).await;

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
        write_target(
            home.path(),
            "bpm",
            Watch::Auto,
            Some("0123456789abcdef0123456789abcdef01234567"),
        );
        let tree = Tree::for_sheep(home.path(), "bpm");
        std::fs::write(tree.state_file(), "this is not toml = = =").expect("corrupt the record");
        let entries = [("bpm", checkout.path())];
        let daemon = RollOf(&entries);

        let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
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

        let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
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
    /// cutover just started. A newcomer used to be any row whose pid the
    /// pre-`Start` generation never had, so an original that crashed and
    /// came back under a new pid looked exactly like one: phase one adopted
    /// it, and the dwell found the same pid still alive. Newcomers are now
    /// told by id, which a respawn keeps, and the restart count on the dwell
    /// is the second wall rather than the only one.
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
        // By id, now: the original keeps its id across the respawn, so it is
        // never a newcomer whatever its pid, and what the cutover reports is
        // that no newcomer appeared at all. The restart check on the dwell
        // stays as a second wall behind this one.
        assert!(
            err.to_string().contains("No new instance appeared"),
            "{err}"
        );
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
