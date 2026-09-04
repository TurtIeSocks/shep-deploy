//! The poll loop: what `watch = "auto"` means.
//!
//! Everything else in this crate runs when an operator types a command.
//! This runs unattended, forever, so two properties matter more here than
//! anywhere else: one target's failure must not stop the others, and
//! whatever a tick prints must be true of what it actually did.
//!
//! # What a tick asks the shepherd for
//!
//! One `set_smit` per target, every tick, and nothing more until it has
//! something to deploy. Targets are read from the filesystem - one
//! directory per target under `<shep_home>/deploy`, each holding its own
//! record - and a target whose branch has not moved is answered by `git`
//! alone. The smit is republished each tick because the daemon holds it in
//! memory only for as long as this dog's connection lasts; see [`tick`].
//! Past that, the shepherd is asked only by a deploy that is going ahead.
//!
//! In particular the roll is never read. [`crate::roll::registered`] is how
//! `survey` learns what shep has registered, and it goes through
//! `SaveRoll`, which makes the shepherd re-query every live process AND
//! rewrite `flock.json` whether anything changed or not. Once per `survey`
//! is a fair price; once every thirty seconds forever is a disk write per
//! tick for a fact no tick needs.
//!
//! # Serial, and no mid-deploy abort
//!
//! One tick deploys its targets one at a time, and a tick never begins
//! while the previous one is still running, because this is a plain
//! `loop { tick().await; sleep(interval).await; }`. A push landing during a
//! build is therefore picked up on the NEXT tick rather than aborting the
//! one in flight. The design asks for the abort; the cost of deferring it
//! is one build of latency before a hotfix lands, not a wrong outcome.
//!
//! The upside is that [`crate::deploy`]'s own race guard stays a guard.
//! It refuses when `current` moved while a deploy was preparing, and that
//! refusal is there for a second OPERATOR invocation - it never has to hold
//! the loop off itself.

use std::any::Any;
use std::collections::BTreeMap;
use std::future::Future;
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::time::sleep;

use crate::config::DogConfig;
use crate::daemon::Daemon;
use crate::deploy::{self, Outcome};
use crate::error::Error;
use crate::paths::{self, Tree};
use crate::smit;
use crate::state::{State, Watch};

/// Whether the poll loop should deploy this target on its own.
///
/// The only thing `manual` changes. Releases, shared linking, the atomic
/// swap, probe verification, auto-rollback and retention all apply to a
/// manual target identically; the trigger is the whole of the difference.
/// Two cases it serves: somebody who wants the convenience without a deploy
/// on every commit, and pausing a target during an incident without
/// rehoming the dog, which is the same switch and matters more when
/// something is already going wrong.
const fn due(state: &State) -> bool {
    matches!(state.watch, Watch::Auto)
}

/// One pass over every target, deploying each `auto` one that has moved.
///
/// Answers with one row per target it attempted, in name order, rather than
/// stopping at the first failure. A dog that gave up because one of five
/// remotes was unreachable would stop deploying the other four, and nothing
/// would restart it with a reason anybody could read.
///
/// Through [`deploy::unattended`] rather than [`deploy::deploy`], which is
/// the difference between a loop and an operator: a sha an earlier attempt
/// failed on is left alone until the branch moves, rather than rebuilt and
/// rolled back every interval for as long as nobody pushes.
///
/// Targets are re-read from disk every tick rather than cached, so a target
/// created, retargeted with `git checkout stable`, or switched to `manual`
/// while the dog runs is picked up on the next pass without a restart.
///
/// Two failures are reported as rows rather than passed over. A record that
/// cannot be read is a target nothing can be done with, and
/// [`paths::targets`] only lists directories that HAVE one, so it is a
/// broken record rather than an ordinary absence. A deploy directory that
/// cannot be listed at all takes every target with it, and its row is named
/// for the directory because there is no sheep left to name; silence there
/// is indistinguishable from a dog with nothing to do.
async fn tick<D: Daemon>(daemon: &D, shep_home: &Path, config: &DogConfig) -> Tick {
    let found = match paths::targets(shep_home) {
        Ok(found) => found,
        Err(err) => {
            return Tick {
                targets: None,
                results: vec![(shep_home.join("deploy").display().to_string(), Err(err))],
            };
        }
    };

    let mut results = Vec::new();
    let targets = Some(found.named.len() + found.unnamed.len());
    // A target the dog cannot name is a row, not a skip: nothing polls it
    // and nothing restores it, and the operator has to be the one to rename
    // it.
    for dir in found.unnamed {
        // The key is the path's `Debug` form: escaped, so it prints as one
        // line, and exact, so two such directories never share a mute entry
        // (`display` is lossy and folds distinct non-UTF-8 names together).
        // `report` puts the key at the front of the line, so the message
        // does not name the directory again.
        results.push((
            format!("{dir:?}"),
            Err(Error::Config(
                "holds a deploy.toml, and its name cannot be a sheep's (not valid UTF-8, or \
                 carrying a character that would rewrite a log line), so it is neither polled \
                 nor restored. `shep flock` lists every sheep and `shep describe <sheep>` its \
                 cwd: check none runs from inside it, then rename the directory"
                    .to_owned(),
            )),
        ));
    }
    for name in found.named {
        let tree = Tree::for_sheep(shep_home, &name);
        let mut state = match State::read(&tree.state_file()) {
            Ok(state) => state,
            Err(err) => {
                results.push((name, Err(err)));
                continue;
            }
        };
        // Above the `due` check on purpose: a manual target's smit is
        // exactly how an operator sees that it is paused, so skipping it
        // would hide the state the smit exists to show.
        //
        // Republished every tick rather than on change, because the daemon
        // holds smits in memory and drops them whenever this dog stops. A
        // publish-on-change dog would show nothing after a daemon restart
        // until its next deploy, which for a healthy target could be weeks.
        //
        // A failure does not stop the deploy: this is cosmetic, and a daemon
        // that refuses it is one this dog can still deploy through. It is a
        // row of its own rather than a bare `eprintln!`, so it reaches the
        // stream the caller chose and, more to the point, the mute: a daemon
        // that refuses every smit would otherwise say so once per target per
        // tick, forever.
        // Keyed with a `/`, which no sheep name can carry, so the row and the
        // target never share a mute entry.
        if let Err(err) = smit::publish(daemon, &name, &state).await {
            results.push((format!("{name}/smit"), Err(err)));
        }
        if !due(&state) {
            continue;
        }
        // Guarded, because this loop's one promise is that a target's
        // failure does not stop the others, and a panic is the one failure
        // `unattended`'s `Result` cannot carry. Nothing in the crate panics
        // on purpose outside tests; this is for the day something does, in
        // a loop meant to run for months.
        let deploy = Box::pin(deploy::unattended(daemon, &tree, &mut state, config));
        let outcome = match CatchUnwind(deploy).await {
            Ok(outcome) => outcome,
            Err(payload) => Err(Error::Panicked {
                sheep: name.clone(),
                what: panic_message(payload.as_ref()),
            }),
        };
        results.push((name, outcome));
    }
    Tick { targets, results }
}

/// What one tick found and did.
struct Tick {
    /// How many targets were on disk, due or not, or `None` when the
    /// directory could not be listed at all.
    ///
    /// Kept apart from the results because a manual target produces no
    /// result and is still a target: a tick with results and no targets is
    /// impossible, and a tick with targets and no results is the ordinary
    /// quiet one. `None` is kept apart from `Some(0)` because "gone" and
    /// "unreadable" want different rows.
    targets: Option<usize>,
    /// One row per target that had something to say, plus a row per smit
    /// that could not be published.
    results: Vec<(String, Result<Outcome, Error>)>,
}

/// A future that turns a panic in the one it wraps into an `Err`, so the
/// loop around it keeps going.
///
/// Written here rather than taken from a crate because it is a dozen lines:
/// each poll is run under [`catch_unwind`], and a payload that comes out is
/// the future's answer. The inner future is pinned on the heap by the
/// caller, which is what makes it `Unpin` and lets it be polled through a
/// plain closure.
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.get_mut().0;
        match catch_unwind(AssertUnwindSafe(|| inner.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

/// The message a panic carried, or a note that it carried none.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic with no message".to_owned()
    }
}

/// What one target's outcome is worth saying, and which log it belongs in.
///
/// Split from the writing so that both halves can be checked: what a tick
/// says is the only observable this module has, and a loop whose prints
/// were all deleted passed the suite that came before this type existed.
#[derive(Debug, PartialEq, Eq)]
enum Said {
    /// Something that happened, for stdout.
    Note(String),
    /// Something that did not, for stderr.
    Complaint(String),
}

impl Said {
    /// The line itself, without the stream it goes to.
    fn text(&self) -> &str {
        match self {
            Self::Note(text) | Self::Complaint(text) => text,
        }
    }
}

/// What to say about one target's outcome, or `None` for silence.
///
/// `UpToDate` says nothing at all, deliberately: it is the answer to almost
/// every tick of almost every target, and a dog that logged a line per
/// target per thirty seconds would bury the deploys nobody wants to miss
/// under its own heartbeat.
///
/// A rollback is a complaint even though it arrives as an `Ok`. The machine
/// is healthy, which is why it is not an error - and the deploy did not
/// land, which is why it does not belong in the log an operator reads to
/// see what shipped.
fn report(sheep: &str, outcome: &Result<Outcome, Error>) -> Option<Said> {
    // Every key is printed as the line's label, and one kind of key is a
    // directory name that exists precisely because it carries something a
    // log line cannot.
    let sheep = &crate::shared::printable(sheep);
    match outcome {
        Ok(Outcome::UpToDate) => None,
        Ok(Outcome::Deployed { sha }) => Some(Said::Note(format!("{sheep} deployed {sha}"))),
        Ok(Outcome::RolledBack { to, why }) => Some(Said::Complaint(format!(
            "{sheep} rolled back to {to}: {why}"
        ))),
        Err(err) => Some(Said::Complaint(format!("{sheep}: {err}"))),
    }
}

/// How many ticks a target's repeated line is muted for before it is said
/// again.
///
/// Counted in ticks rather than in time, so a loop that polls less often
/// says it less often - 120 is an hour at the default interval. It is not
/// zero, because the alternative is a condition that is mentioned once and
/// then never again: a dog running for months would have its only
/// explanation in a log that has since rotated, and a target removed and
/// recreated would have its first refusal swallowed by the entry left over
/// from its predecessor.
const RESAY: u32 = 120;

/// One target's last line, and how many ticks it has been muted for.
struct Repeat {
    /// The line as it was said.
    line: String,
    /// Ticks since it was last said.
    muted: u32,
}

/// Whether `line` is worth saying, given what was last said about this
/// target.
///
/// A line that differs from the last one is always worth saying, and a
/// repeat of the same line is not, until [`RESAY`] ticks have gone by.
///
/// The criterion is the line rather than a list of error variants, and that
/// is the whole of the fix it replaces. "The refusals that cannot clear on
/// their own" is not a set anybody can enumerate correctly - a tree the
/// cutover never landed on, a `deploy.toml` that will not parse, a deploy
/// directory that cannot be listed, a remote that no longer resolves and a
/// build that fails on a committed typo are all permanent until somebody
/// acts - and an enumeration that misses one prints 2,880 identical lines a
/// day. A condition that really can clear on its own says something
/// different the moment it does, which re-arms the target without anything
/// having to know which conditions those are.
///
/// Answers how many repeats were swallowed since the line was last said,
/// so a line said again after [`RESAY`] ticks can carry the count: an
/// operator reading it an hour later can then tell "still broken" from
/// "broke again, many times". `None` is silence; `Some(0)` is a line that
/// is new.
fn worth_saying(previous: &mut BTreeMap<String, Repeat>, sheep: &str, line: &str) -> Option<u32> {
    let mut swallowed = 0;
    if let Some(seen) = previous.get_mut(sheep)
        && seen.line == line
    {
        seen.muted += 1;
        if seen.muted < RESAY {
            return None;
        }
        swallowed = seen.muted;
    }

    previous.insert(
        sheep.to_owned(),
        Repeat {
            line: line.to_owned(),
            muted: 0,
        },
    );
    Some(swallowed)
}

/// Polls forever, deploying what has moved.
///
/// The first tick runs at once and the sleep comes after it, so a restarted
/// dog is doing its job immediately rather than looking broken for an
/// interval first.
///
/// What it says per target is [`report`]'s, and how often it repeats itself
/// is [`worth_saying`]'s.
///
/// # Errors
/// Never returns `Ok`, and in practice never returns at all: a target's
/// failure is reported and the loop goes on to the next one, because a dog
/// that exited on the first unreachable remote would stop deploying
/// everything else and nothing would restart it with a reason anybody could
/// read. The `Result` is the signature the caller wants, not a promise that
/// something in here can fail outward.
///
/// # Cancellation
/// Stopping mid-deploy is not interrupted at a step of this loop's
/// choosing. A `tokio::select!` around this future cancels it at its next
/// await point, which can be inside a deploy, and that is acceptable: every
/// step is either before the swap, where nothing has been disturbed, or
/// inside the reload, where the shepherd already has its instructions and
/// finishes on its own. What a cancellation can lose is the record write
/// after a verified deploy, which leaves `deploy.toml` naming the previous
/// release and the next tick deploying the same sha again. That costs one
/// rebuild and repairs itself, which is the right shape of failure for a
/// signal that means "stop now".
///
/// It is not deferred by a git call any more. Every git call and the
/// artifact copy run on tokio's blocking pool (`shared::off_thread`), so a
/// stop arriving during a five-minute fetch is answered at once: this
/// future is dropped, `main` gives the pool a second to wind down, and the
/// fetch's own git process finishes or times out on its own after the dog
/// has gone. A fetch against a host that is not answering is still bounded
/// by `DogConfig::git_timeout`, so it fails that one target like any other
/// error rather than holding the loop.
pub async fn run<D: Daemon>(daemon: &D, shep_home: &Path, config: &DogConfig) -> Result<(), Error> {
    run_with(
        daemon,
        shep_home,
        config,
        &mut io::stdout(),
        &mut io::stderr(),
    )
    .await
}

/// [`run`], writing to somewhere a test can read.
///
/// The whole of what this loop does that anybody sees is what it writes, so
/// the two streams are arguments rather than macros: with `println!` baked
/// in, deleting every line of output left the suite green.
///
/// # Errors
/// As [`run`]. A write that fails is NOT one of them: a dog whose log pipe
/// has gone should carry on deploying rather than die of it, so a failed
/// write is passed over, exactly as `println!` would have carried on after
/// a full disk.
async fn run_with<D: Daemon, O: Write, E: Write>(
    daemon: &D,
    shep_home: &Path,
    config: &DogConfig,
    out: &mut O,
    err: &mut E,
) -> Result<(), Error> {
    let mut previous: BTreeMap<String, Repeat> = BTreeMap::new();
    let deploy_dir = shep_home.join("deploy").display().to_string();
    loop {
        let Tick {
            targets,
            mut results,
        } = tick(daemon, shep_home, config).await;

        // A deploy directory that is gone answers as an empty list, which is
        // also what a shepherd with no targets yet answers, so the loop went
        // silent the moment a directory was unmounted or removed from under
        // it. Said on every tick with nothing to poll, and left to the mute
        // to say once and then hourly: that covers a dog restarted after the
        // directory went, which a "had targets last tick" flag did not. A
        // listing that failed carries its own row and is not this case.
        if targets == Some(0) {
            results.push((
                deploy_dir.clone(),
                Err(Error::Config(format!(
                    "no deploy targets under {deploy_dir}: nothing to poll until one is set up \
                     with `shep-deploy setup <sheep>`"
                ))),
            ));
        }

        // A target that is gone stops muting anything. Otherwise a target
        // deleted and recreated has its first line swallowed by the entry
        // its predecessor left behind.
        previous.retain(|name, _| results.iter().any(|(seen, _)| seen == name));

        for (sheep, outcome) in results {
            let Some(said) = report(&sheep, &outcome) else {
                // Nothing to say means nothing to repeat: a target that is
                // up to date has put whatever it was complaining about
                // behind it.
                previous.remove(&sheep);
                continue;
            };
            let Some(swallowed) = worth_saying(&mut previous, &sheep, said.text()) else {
                continue;
            };
            let repeats = if swallowed == 0 {
                String::new()
            } else {
                format!(" (repeated {swallowed} times since it was last said)")
            };
            let _ = match &said {
                Said::Note(text) => writeln!(out, "{text}{repeats}"),
                Said::Complaint(text) => writeln!(err, "{text}{repeats}"),
            };
        }

        sleep(config.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use crate::fixtures;

    use super::*;

    /// fails if a target the dog cannot name is skipped, keyed by a lossy
    /// name, or printed with the character that made it unnameable. The
    /// key is the path's `Debug` form so two such directories never share a
    /// mute entry; the line an operator reads has the newline replaced.
    #[tokio::test]
    async fn an_unnameable_target_is_a_row_that_cannot_forge_a_line() {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = home.path().join("deploy").join("bad\nname");
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("deploy.toml"), "").expect("record");

        let results = tick(&Ready::new(), home.path(), &fixtures::dog_config())
            .await
            .results;

        assert_eq!(results.len(), 1, "{results:?}");
        let (key, outcome) = &results[0];
        assert_eq!(key, &format!("{dir:?}"));
        let said = report(key, outcome).expect("a complaint");
        assert!(!said.text().contains('\n'), "{:?}", said.text());
        assert!(
            said.text().contains("rename the directory"),
            "{}",
            said.text()
        );
    }

    /// fails if a panic inside a deploy stops being caught. The loop's one
    /// promise is that a target's failure does not stop the others, and a
    /// panic is the failure `Result` cannot carry.
    #[tokio::test]
    async fn a_panic_becomes_an_err_rather_than_ending_the_loop() {
        let caught = CatchUnwind(Box::pin(async {
            tokio::task::yield_now().await;
            panic!("the deploy fell over");
        }))
        .await;
        let payload = caught.expect_err("the panic must be caught");
        assert_eq!(panic_message(payload.as_ref()), "the deploy fell over");

        let fine = CatchUnwind(Box::pin(async { 7 })).await;
        assert_eq!(fine.ok(), Some(7));
    }

    /// fails if a panic's message is lost on the way to the row. A `String`
    /// payload is what `panic!` with a format string gives, a `&str` is what
    /// a literal gives, and anything else has to say it had no message
    /// rather than print nothing.
    #[test]
    fn a_panic_message_is_read_from_either_payload_shape() {
        assert_eq!(panic_message(&"literal"), "literal");
        assert_eq!(panic_message(&String::from("formatted")), "formatted");
        assert_eq!(panic_message(&42_u8), "a panic with no message");
    }

    use core::time::Duration;
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use shep_client::RequestError;
    use shep_client::shep_core::config::AppConfig;
    use shep_client::shep_core::protocol::{ProcessInfo, RpcError, RpcErrorCode};
    use shep_client::shep_core::status::ProcStatus;
    use tempfile::TempDir;

    use crate::state::Verify;
    use crate::swap;

    /// The fixture app, named per sheep so `flockfile::app_config` finds
    /// it, and probed because `Verify::Probed` refuses a sheep whose
    /// readiness nothing gates.
    fn flockfile(sheep: &str) -> String {
        format!(
            "[[app]]\nname = '{sheep}'\nscript = './run.sh'\n\n\
             [app.readiness_probe]\nkind = 'exec'\ntarget = 'true'\n"
        )
    }

    /// A target's record, for the one function that reads nothing else.
    fn target(watch: Watch) -> State {
        State {
            deployed: Some(fixtures::OTHER_SHA.to_owned()),
            watch,
            ..fixtures::state()
        }
    }

    /// Writes `<home>/deploy/<sheep>/deploy.toml` and nothing else: a
    /// target with a record and no repository under it at all.
    fn write_target(home: &Path, sheep: &str, watch: Watch, sha: Option<&str>) {
        let tree = Tree::for_sheep(home, sheep);
        fs::create_dir_all(tree.root()).expect("create the tree");
        let mut state = target(watch);
        state.deployed = sha.map(str::to_owned);
        state.write(&tree.state_file()).expect("write deploy.toml");
    }

    /// A target that a tick can really deploy: an origin repository one
    /// commit ahead, a bare clone, a release already live under `current`,
    /// and a record naming it.
    ///
    /// The returned [`TempDir`] is the origin, and dropping it deletes the
    /// repository the deploy fetches from, so tests hold it.
    fn write_target_ready(home: &Path, sheep: &str, watch: Watch) -> TempDir {
        let origin = fixtures::checkout(&[("Flockfile.toml", &flockfile(sheep))]);

        let tree = Tree::for_sheep(home, sheep);
        fs::create_dir_all(tree.git()).expect("create the git dir");
        fixtures::run_git(&tree.git(), &["init", "-q", "--bare"]);
        let remote = origin.path().to_str().expect("utf-8 path").to_owned();
        crate::git::fetch(&tree.git(), &remote, fixtures::TEST_BUDGET).expect("fetch");

        let first = crate::git::remote_head(&tree.git(), "main").expect("head");
        crate::git::worktree_add(&tree.git(), &tree.release(&first), &first).expect("worktree");
        swap::point_at(&tree.current(), &tree.release(&first)).expect("swap");

        let state = State {
            remote,
            deployed: Some(first),
            verify: Verify::Probed,
            watch,
            checkout: origin.path().to_owned(),
            ..fixtures::state()
        };
        state.write(&tree.state_file()).expect("write deploy.toml");

        // The commit the tick has to notice. Without it every deploy is
        // `UpToDate` and the tests below would pass on a loop that does
        // nothing at all.
        fs::write(origin.path().join("second.txt"), "x").expect("write");
        fixtures::run_git(origin.path(), &["add", "."]);
        fixtures::run_git(origin.path(), &["commit", "-q", "-m", "second"]);

        origin
    }

    /// The pid serving before any reload. Any other value would do; this
    /// one is only recognisable in a failure message.
    const FIRST_PID: u32 = 12835;

    /// A shepherd whose reload really replaces the process, so a deploy
    /// against a sound tree verifies and lands.
    ///
    /// The pid moves on the RELOAD rather than on every `describe`.
    /// A double whose pid moved on its own would report a turnover to any
    /// caller that looked twice, which is how a deploy that never reloaded
    /// anything could pass for one that did.
    struct Ready {
        reloads: Cell<u32>,
    }

    impl Ready {
        const fn new() -> Self {
            Self {
                reloads: Cell::new(0),
            }
        }
    }

    impl Daemon for Ready {
        async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            Ok(vec![
                ProcessInfo::builder(0, sheep, ProcStatus::Online)
                    .pid(Some(FIRST_PID + self.reloads.get() * 100))
                    .build(),
            ])
        }
        async fn reload(&self, _sheep: &str) -> Result<(), Error> {
            self.reloads.set(self.reloads.get() + 1);
            Ok(())
        }
        async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), Error> {
            Ok(())
        }

        crate::fixtures::daemon_methods!(unimplemented;
            dog_config, list_flock, start, delete, restart, save_roll,
        );
    }

    /// A shepherd that has gone quiet, counting the times it was asked, on
    /// a branch that moves while it is down.
    ///
    /// One `describe` per tick, exactly. A deploy captures the generation
    /// before its reload, that capture is the first thing it asks for, and
    /// this answers it with an error, so the deploy puts `current` back and
    /// gives up before asking anything else.
    ///
    /// The commit is what makes the next tick do the same thing again
    /// rather than hold: an attempt that failed is recorded against its
    /// sha, and the branch moving is what clears it. So this is a target
    /// somebody keeps pushing to while the shepherd is down, which is the
    /// one shape that really does deploy on every tick - and the reason the
    /// count is a tick count.
    ///
    /// A count far above what the interval allows means the loop stopped
    /// sleeping. That is a busy loop with no await point in it, and under a
    /// paused clock it would hang the test rather than fail it, because the
    /// runtime never parks and so never advances time to the timeout. The
    /// panic turns that hang into a red test with a reason on it.
    struct Counting {
        describes: Cell<u32>,
        origin: PathBuf,
    }

    impl Counting {
        fn new(origin: &Path) -> Self {
            Self {
                describes: Cell::new(0),
                origin: origin.to_owned(),
            }
        }

        fn ticks(&self) -> u32 {
            self.describes.get()
        }
    }

    impl Daemon for Counting {
        async fn describe(&self, _sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            let asked = self.describes.get() + 1;
            self.describes.set(asked);
            assert!(
                asked <= 60,
                "the loop ticked far more than the interval allows: it is not sleeping"
            );
            fs::write(self.origin.join(format!("{asked}.txt")), "x").expect("write");
            fixtures::run_git(&self.origin, &["add", "."]);
            fixtures::run_git(&self.origin, &["commit", "-q", "-m", "another"]);
            Err(Error::Protocol("the shepherd stopped answering".to_owned()))
        }
        async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), Error> {
            Ok(())
        }

        crate::fixtures::daemon_methods!(unimplemented;
            dog_config, list_flock, start, delete, reload, restart, save_roll,
        );
    }

    /// fails if a manual target is polled. This is the switch an operator
    /// reaches for during an incident, and a deploy firing at that moment
    /// is the exact opposite of what they asked for. `set_watch` already
    /// has a test that SETTING the mode does not deploy; this is the other
    /// half, that HOLDING it does not either.
    #[tokio::test]
    async fn a_manual_target_is_never_polled() {
        assert!(!due(&target(Watch::Manual)));
        assert!(due(&target(Watch::Auto)));
    }

    /// fails if a manual target is deployed by a tick that runs over it.
    /// The line above is the decision on its own; this is the decision
    /// being obeyed, and it is the one that would let a paused target
    /// deploy in the middle of an incident.
    #[tokio::test(start_paused = true)]
    async fn a_manual_target_is_left_alone_by_a_whole_tick() {
        let home = tempfile::tempdir().expect("tempdir");
        let _origin = write_target_ready(home.path(), "paused", Watch::Manual);
        let before = State::read(&Tree::for_sheep(home.path(), "paused").state_file())
            .expect("reads")
            .deployed;

        assert!(
            tick(&Ready::new(), home.path(), &fixtures::dog_config())
                .await
                .results
                .is_empty()
        );

        assert_eq!(
            State::read(&Tree::for_sheep(home.path(), "paused").state_file())
                .expect("reads")
                .deployed,
            before,
            "still on the release it was on"
        );
    }

    /// fails if one target's failure stops the tick. A dog that gives up
    /// because one of five remotes was unreachable stops deploying the
    /// other four, and nothing restarts it with a reason anybody can read.
    #[tokio::test(start_paused = true)]
    async fn one_targets_failure_does_not_stop_the_others() {
        let home = tempfile::tempdir().expect("tempdir");
        // A record naming a release, with no repository under it at all:
        // the fetch is what fails, which is the shape of a remote nobody
        // can reach.
        write_target(home.path(), "broken", Watch::Auto, Some(fixtures::SHA));
        let _origin = write_target_ready(home.path(), "fine", Watch::Auto);

        let results = tick(&Ready::new(), home.path(), &fixtures::dog_config())
            .await
            .results;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "broken");
        assert!(results[0].1.is_err(), "broken failed");
        assert_eq!(results[1].0, "fine");
        assert!(
            matches!(results[1].1, Ok(Outcome::Deployed { .. })),
            "fine still ran: {:?}",
            results[1].1
        );
    }

    /// fails if a tick rebuilds a sha the last attempt failed on. This is
    /// the difference between the loop's entry into the deploy sequence and
    /// an operator's: unheld, one bad commit costs two reloads of a live
    /// app and a full rebuild every interval, indefinitely, and nothing
    /// about that self-corrects until somebody pushes.
    #[tokio::test(start_paused = true)]
    async fn a_tick_holds_a_sha_that_already_failed() {
        let home = tempfile::tempdir().expect("tempdir");
        let origin = write_target_ready(home.path(), "held", Watch::Auto);
        let tree = Tree::for_sheep(home.path(), "held");
        let mut state = State::read(&tree.state_file()).expect("reads");
        state.failed = Some(fixtures::head_of(origin.path()));
        state.write(&tree.state_file()).expect("writes");

        let daemon = Ready::new();
        let results = tick(&daemon, home.path(), &fixtures::dog_config())
            .await
            .results;

        assert_eq!(results.len(), 1);
        assert!(
            matches!(results[0].1, Err(Error::Held { .. })),
            "{:?}",
            results[0].1
        );
        assert_eq!(daemon.reloads.get(), 0, "nothing was reloaded");
    }

    /// fails if a tick with no targets at all is an error. That is every
    /// freshly adopted dog, and it must idle quietly rather than logging a
    /// failure every thirty seconds forever.
    #[tokio::test]
    async fn a_dog_with_no_targets_ticks_quietly() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(
            tick(&Ready::new(), home.path(), &fixtures::dog_config())
                .await
                .results
                .is_empty()
        );
    }

    /// fails if a deploy directory that cannot be listed is passed over in
    /// silence. Every target is under it, so the dog has stopped deploying
    /// everything - and idling quietly is what a dog with nothing to do
    /// looks like too. One row, named for the directory rather than for a
    /// sheep, because there is no sheep to name.
    ///
    /// Relies on mode bits, which root ignores: run as root (a container,
    /// say) the listing succeeds and this test fails for a reason that has
    /// nothing to do with the loop. CI runs unprivileged.
    #[tokio::test]
    async fn a_deploy_directory_that_cannot_be_listed_is_reported() {
        let home = tempfile::tempdir().expect("tempdir");
        let root = home.path().join("deploy");
        fs::create_dir_all(&root).expect("create the deploy dir");
        // Listable again on drop or not, this test never reads it back.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).expect("chmod");

        let results = tick(&Ready::new(), home.path(), &fixtures::dog_config())
            .await
            .results;

        let listable = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, root.display().to_string());
        assert!(results[0].1.is_err());
        listable.expect("chmod back");
    }

    /// fails if a record that cannot be read is skipped in silence. A
    /// `deploy.toml` that will not parse is a target the dog can do nothing
    /// with and nobody will hear about, and `paths::targets` only lists
    /// directories that have one - so this is a broken record rather than
    /// an ordinary absence.
    #[tokio::test]
    async fn a_record_that_cannot_be_read_is_reported_rather_than_skipped() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "garbled");
        fs::create_dir_all(tree.root()).expect("create the tree");
        fs::write(tree.state_file(), "this is not toml").expect("write deploy.toml");

        let results = tick(&Ready::new(), home.path(), &fixtures::dog_config())
            .await
            .results;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "garbled");
        assert!(results[0].1.is_err());
    }

    /// fails if the loop stops sleeping the configured interval between
    /// ticks. A loop that polls continuously fetches continuously, which
    /// hammers every remote it watches and reads as a hung dog.
    ///
    /// `start_paused` so a five-minute interval costs no wall clock: the
    /// same idiom the deploy tests use to run out a real verification
    /// budget instantly. The target is a real one rather than an empty
    /// home, because a tick over an empty home asks the shepherd nothing
    /// and there would be nothing to count.
    #[tokio::test(start_paused = true)]
    async fn ticks_are_spaced_by_the_configured_interval() {
        let home = tempfile::tempdir().expect("tempdir");
        let origin = write_target_ready(home.path(), "quiet", Watch::Auto);
        let counter = Counting::new(origin.path());
        let began = tokio::time::Instant::now();

        // Three ticks' worth, then stop.
        let _ = tokio::time::timeout(
            Duration::from_secs(305),
            run(
                &counter,
                home.path(),
                &DogConfig {
                    interval: Duration::from_secs(150),
                    ..fixtures::dog_config()
                },
            ),
        )
        .await;

        assert_eq!(counter.ticks(), 3, "one at t=0, then every 150s");
        assert!(began.elapsed() >= Duration::from_secs(300));
    }

    /// fails if the first tick waits for the interval before running. A dog
    /// that polls nothing for the first thirty seconds after every restart
    /// looks broken for thirty seconds after every restart, and a longer
    /// interval makes it worse in proportion.
    #[tokio::test(start_paused = true)]
    async fn the_first_tick_happens_at_once() {
        let home = tempfile::tempdir().expect("tempdir");
        let origin = write_target_ready(home.path(), "quiet", Watch::Auto);
        let counter = Counting::new(origin.path());

        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            run(
                &counter,
                home.path(),
                &DogConfig {
                    interval: Duration::from_secs(600),
                    ..fixtures::dog_config()
                },
            ),
        )
        .await;

        assert_eq!(counter.ticks(), 1);
    }

    /// fails if a target that keeps failing the same way keeps saying so.
    /// At thirty seconds, a line an interval is 2,880 identical lines a day
    /// with everything the dog really has to say buried in between them.
    #[test]
    fn a_line_that_repeats_is_said_once() {
        let mut previous = BTreeMap::new();

        assert!(worth_saying(&mut previous, "web", "web: the remote is gone").is_some());
        assert!(worth_saying(&mut previous, "web", "web: the remote is gone").is_none());
        assert!(worth_saying(&mut previous, "web", "web: the remote is gone").is_none());
        // A different target is a different line.
        assert!(worth_saying(&mut previous, "koji", "koji: the remote is gone").is_some());
    }

    /// fails if the mute is keyed on anything narrower than the line. The
    /// criterion it replaced was one error variant, and every other
    /// permanent condition - an unparseable record, a deploy directory that
    /// cannot be listed, a remote that no longer resolves, a build that
    /// fails on a committed typo - went on saying so every interval. There
    /// is no correct list of those, which is the argument for the line.
    #[test]
    fn every_repeated_line_is_muted_not_one_chosen_kind() {
        let mut previous = BTreeMap::new();
        let lines = [
            "web: bad deploy configuration: /srv/deploy/web/deploy.toml: expected an equals",
            "web: `git fetch origin` exited with status 128: could not resolve host",
            "web: the build command exited with status 1",
        ];

        for line in lines {
            assert!(worth_saying(&mut previous, "web", line).is_some(), "{line}");
            assert!(worth_saying(&mut previous, "web", line).is_none(), "{line}");
        }
    }

    /// fails if a condition that cleared stays muted, so that its coming
    /// back is never mentioned. A line that differs is new information, and
    /// a target that has started working again is the clearest case of it.
    #[test]
    fn a_line_that_changes_is_said_again() {
        let mut previous = BTreeMap::new();
        let broken = "web: the remote is gone";

        assert!(worth_saying(&mut previous, "web", broken).is_some());
        assert!(worth_saying(&mut previous, "web", broken).is_none());
        assert!(worth_saying(&mut previous, "web", "web deployed abc1234").is_some());
        assert!(worth_saying(&mut previous, "web", broken).is_some());
    }

    /// fails if a muted line is muted forever. A dog runs for months, and a
    /// condition mentioned once in a log that has since rotated is a
    /// condition nobody can find. Said again every `RESAY` ticks, which is
    /// an hour at the default interval.
    #[test]
    fn a_muted_line_is_said_again_eventually() {
        let mut previous = BTreeMap::new();
        let line = "web: the remote is gone";

        assert!(worth_saying(&mut previous, "web", line).is_some());
        let said = (0..RESAY * 2)
            .filter(|_| worth_saying(&mut previous, "web", line).is_some())
            .count();

        assert_eq!(said, 2, "once every RESAY ticks, not once ever");
    }

    /// fails if what a tick did stops reaching the log, or reaches the
    /// wrong one. This is the whole of what an operator sees of the poll
    /// loop: nothing else about it is observable at all.
    #[test]
    fn each_outcome_says_what_it_did_and_where() {
        assert_eq!(report("web", &Ok(Outcome::UpToDate)), None, "the heartbeat");
        assert_eq!(
            report(
                "web",
                &Ok(Outcome::Deployed {
                    sha: "a1b2c3".to_owned()
                })
            ),
            Some(Said::Note("web deployed a1b2c3".to_owned()))
        );
        // A rollback arrives as an `Ok` because the machine is healthy, and
        // is still a complaint because the deploy did not land.
        assert_eq!(
            report(
                "web",
                &Ok(Outcome::RolledBack {
                    to: "old".to_owned(),
                    why: "it did not come up".to_owned()
                })
            ),
            Some(Said::Complaint(
                "web rolled back to old: it did not come up".to_owned()
            ))
        );
        let err = report("web", &Err(Error::Build { status: Some(1) }));
        let Some(Said::Complaint(text)) = err else {
            panic!("a failure is a complaint: {err:?}");
        };
        assert!(text.starts_with("web: "), "{text}");
    }

    /// fails if a deploy the loop made never reaches the log. The whole
    /// output block could be deleted and every test above still passes:
    /// `report` and `worth_saying` are pure, and nothing else pins that
    /// their verdict is what actually gets written, or that it goes to
    /// stdout where an operator looks for what shipped.
    #[tokio::test(start_paused = true)]
    async fn a_deploy_is_written_to_the_log_it_belongs_in() {
        let home = tempfile::tempdir().expect("tempdir");
        let origin = write_target_ready(home.path(), "fine", Watch::Auto);
        let head = fixtures::head_of(origin.path());
        let (mut out, mut err) = (Vec::new(), Vec::new());

        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            run_with(
                &Ready::new(),
                home.path(),
                &DogConfig {
                    interval: Duration::from_secs(600),
                    ..fixtures::dog_config()
                },
                &mut out,
                &mut err,
            ),
        )
        .await;

        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            format!("fine deployed {head}\n")
        );
        assert!(err.is_empty(), "nothing failed");
    }

    /// fails if a failure never reaches the error log, or reaches it once
    /// per tick. Both halves matter and neither is pinned by the pure
    /// tests: this is the only thing that says `worth_saying`'s verdict
    /// actually gates a write.
    #[tokio::test(start_paused = true)]
    async fn a_failure_is_written_once_however_many_ticks_repeat_it() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target(home.path(), "broken", Watch::Auto, Some(fixtures::SHA));
        let (mut out, mut err) = (Vec::new(), Vec::new());

        // Three ticks' worth, as the interval test above.
        let _ = tokio::time::timeout(
            Duration::from_secs(305),
            run_with(
                &Ready::new(),
                home.path(),
                &DogConfig {
                    interval: Duration::from_secs(150),
                    ..fixtures::dog_config()
                },
                &mut out,
                &mut err,
            ),
        )
        .await;

        let complained = String::from_utf8(err).expect("utf-8");
        assert_eq!(complained.lines().count(), 1, "{complained}");
        assert!(complained.starts_with("broken: "), "{complained}");
        assert!(out.is_empty(), "nothing deployed");
    }

    /// A [`Daemon`] whose `set_smit` records every `(sheep, text)` it was
    /// asked to paint, in call order, and which otherwise behaves as
    /// [`Ready`] does.
    struct SmitRecording {
        ready: Ready,
        smits: RefCell<Vec<(String, String)>>,
    }

    impl Default for SmitRecording {
        fn default() -> Self {
            Self {
                ready: Ready::new(),
                smits: RefCell::new(Vec::new()),
            }
        }
    }

    impl SmitRecording {
        fn smits(&self) -> Vec<(String, String)> {
            self.smits.borrow().clone()
        }
    }

    impl Daemon for SmitRecording {
        async fn dog_config(&self, name: &str) -> Result<String, Error> {
            self.ready.dog_config(name).await
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            self.ready.list_flock().await
        }
        async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            self.ready.describe(sheep).await
        }
        async fn start(&self, apps: Vec<AppConfig>) -> Result<(), Error> {
            self.ready.start(apps).await
        }
        async fn delete(&self, id: u32) -> Result<(), Error> {
            self.ready.delete(id).await
        }
        async fn reload(&self, sheep: &str) -> Result<(), Error> {
            self.ready.reload(sheep).await
        }
        async fn restart(&self, sheep: &str) -> Result<(), Error> {
            self.ready.restart(sheep).await
        }
        async fn save_roll(&self) -> Result<PathBuf, Error> {
            self.ready.save_roll().await
        }
        async fn set_smit(&self, sheep: &str, text: &str) -> Result<(), Error> {
            self.smits
                .borrow_mut()
                .push((sheep.to_owned(), text.to_owned()));
            Ok(())
        }
    }

    /// A [`Daemon`] whose `set_smit` always answers an [`RpcError`], and
    /// which otherwise deploys cleanly - the shepherd refusing a smit is
    /// not the same thing as a shepherd that cannot be reached.
    struct RefusingSmits;

    impl Daemon for RefusingSmits {
        async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            Ok(vec![
                ProcessInfo::builder(0, sheep, ProcStatus::Online)
                    .pid(Some(FIRST_PID))
                    .build(),
            ])
        }
        async fn reload(&self, _sheep: &str) -> Result<(), Error> {
            Ok(())
        }
        async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), Error> {
            Err(Error::Request(RequestError::Rpc(RpcError {
                code: RpcErrorCode::Internal,
                message: "smits are not accepted right now".to_owned(),
                daemon_version: None,
            })))
        }

        crate::fixtures::daemon_methods!(unimplemented;
            dog_config, list_flock, start, delete, restart, save_roll,
        );
    }

    /// fails if the smit stops being republished every tick. The daemon
    /// holds smits in memory and drops them when the dog stops, so a dog
    /// that published once and then only on change would show nothing at
    /// all after a daemon restart until the next deploy, which for a
    /// healthy target could be weeks.
    #[tokio::test]
    async fn every_tick_republishes_every_targets_smit() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target(
            home.path(),
            "bpm",
            Watch::Auto,
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"),
        );
        write_target(
            home.path(),
            "ctm",
            Watch::Manual,
            Some("f6e5d4c3b2a1f6e5d4c3b2a1f6e5d4c3b2a1f6e5"),
        );
        let daemon = SmitRecording::default();

        tick(&daemon, home.path(), &fixtures::dog_config()).await;
        tick(&daemon, home.path(), &fixtures::dog_config()).await;

        assert_eq!(
            daemon.smits(),
            vec![
                ("bpm".to_owned(), "▲ main@a1b2c3".to_owned()),
                ("ctm".to_owned(), "⏸ main@f6e5d4".to_owned()),
                ("bpm".to_owned(), "▲ main@a1b2c3".to_owned()),
                ("ctm".to_owned(), "⏸ main@f6e5d4".to_owned()),
            ]
        );
    }

    /// fails if a MANUAL target loses its smit. `due` skips manual targets
    /// for deploying, and the smit is exactly how an operator sees that a
    /// target is paused, so skipping the smit too would hide the state the
    /// smit exists to show.
    #[tokio::test]
    async fn a_manual_target_still_gets_a_smit() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target(
            home.path(),
            "ctm",
            Watch::Manual,
            Some("f6e5d4c3b2a1f6e5d4c3b2a1f6e5d4c3b2a1f6e5"),
        );
        let daemon = SmitRecording::default();
        tick(&daemon, home.path(), &fixtures::dog_config()).await;
        assert_eq!(daemon.smits().len(), 1);
    }

    /// fails if a smit that could not be published takes the tick down with
    /// it. A daemon that refuses one is a daemon this dog can still deploy
    /// through, and a cosmetic call must never cost a deploy.
    #[tokio::test]
    async fn a_refused_smit_does_not_stop_the_tick() {
        let home = tempfile::tempdir().expect("tempdir");
        // Held, unlike the brief's own listing of this test: an unheld
        // `TempDir` drops (and deletes) the origin the instant this line
        // ends, which fails the fetch for a reason that has nothing to do
        // with the smit refusal this test exists to pin - see every other
        // `write_target_ready` caller in this file for the pattern.
        let _origin = write_target_ready(home.path(), "fine", Watch::Auto);
        let results = tick(&RefusingSmits, home.path(), &fixtures::dog_config())
            .await
            .results;
        let row = |name: &str| {
            results
                .iter()
                .find(|(sheep, _)| sheep == name)
                .unwrap_or_else(|| panic!("no row for {name}: {results:?}"))
        };
        assert!(row("fine").1.is_ok(), "the deploy still ran");
        // And the refusal is a row of its own, so it reaches the mute rather
        // than being printed once per target per tick forever.
        assert!(
            row("fine/smit").1.is_err(),
            "the refusal is reported as its own row"
        );
    }

    /// fails if a target whose branch name is longer than a smit allows
    /// takes the deploy down with it, or simply publishes nothing. The
    /// text is shortened by `smit::publish` before it ever reaches the
    /// daemon, so this reaches the daemon at all rather than erroring out
    /// of `set_smit` locally.
    #[tokio::test]
    async fn a_too_long_branch_name_does_not_stop_the_tick() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target(
            home.path(),
            "long",
            Watch::Auto,
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"),
        );
        let tree = Tree::for_sheep(home.path(), "long");
        let mut state = State::read(&tree.state_file()).expect("reads");
        state.branch = "x".repeat(100);
        state.write(&tree.state_file()).expect("writes");

        let daemon = SmitRecording::default();
        let results = tick(&daemon, home.path(), &fixtures::dog_config())
            .await
            .results;

        assert!(
            !daemon.smits().is_empty(),
            "a shortened smit was still sent"
        );
        let sent = &daemon.smits()[0].1;
        assert!(sent.chars().count() <= 48, "{sent}");
        // The target has no real repository, so the deploy attempt itself
        // fails - what this test pins is that the smit publish ahead of it
        // did not take the whole tick down too.
        assert!(results[0].1.is_err(), "the fetch is what fails here");
    }
}
