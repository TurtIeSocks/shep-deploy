//! The poll loop: what `watch = "auto"` means.
//!
//! Everything else in this crate runs when an operator types a command.
//! This runs unattended, forever, so two properties matter more here than
//! anywhere else: one target's failure must not stop the others, and
//! whatever a tick prints must be true of what it actually did.
//!
//! # What a tick asks the shepherd for
//!
//! Nothing, until it has something to deploy. Targets are read from the
//! filesystem - one directory per target under `<shep_home>/deploy`, each
//! holding its own record - and a target whose branch has not moved is
//! answered by `git` alone. The shepherd is asked only by a deploy that is
//! going ahead.
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

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use tokio::time::sleep;

use crate::config::DogConfig;
use crate::daemon::Daemon;
use crate::deploy::{self, Outcome};
use crate::error::Error;
use crate::paths::{self, Tree};
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
async fn tick<D: Daemon>(
    daemon: &D,
    shep_home: &Path,
    config: DogConfig,
) -> Vec<(String, Result<Outcome, Error>)> {
    let names = match paths::targets(shep_home) {
        Ok(names) => names,
        Err(err) => return vec![(shep_home.join("deploy").display().to_string(), Err(err))],
    };

    let mut results = Vec::new();
    for name in names {
        let tree = Tree::for_sheep(shep_home, &name);
        let mut state = match State::read(&tree.state_file()) {
            Ok(state) => state,
            Err(err) => {
                results.push((name, Err(err)));
                continue;
            }
        };
        if !due(&state) {
            continue;
        }
        let outcome = deploy::deploy(daemon, &tree, &mut state, config.retention).await;
        results.push((name, outcome));
    }
    results
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
fn worth_saying(previous: &mut BTreeMap<String, Repeat>, sheep: &str, line: &str) -> bool {
    if let Some(seen) = previous.get_mut(sheep)
        && seen.line == line
    {
        seen.muted += 1;
        if seen.muted < RESAY {
            return false;
        }
    }

    previous.insert(
        sheep.to_owned(),
        Repeat {
            line: line.to_owned(),
            muted: 0,
        },
    );
    true
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
/// It can also be deferred, which is a separate matter and not this
/// module's to fix: `git` runs through blocking `std::process::Command` on
/// a current-thread runtime, so nothing else is polled while a fetch is in
/// flight, and a fetch against a host that is not answering is not a
/// bounded wait.
pub async fn run<D: Daemon>(daemon: &D, shep_home: &Path, config: DogConfig) -> Result<(), Error> {
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
    config: DogConfig,
    out: &mut O,
    err: &mut E,
) -> Result<(), Error> {
    let mut previous: BTreeMap<String, Repeat> = BTreeMap::new();
    loop {
        let results = tick(daemon, shep_home, config).await;

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
            if !worth_saying(&mut previous, &sheep, said.text()) {
                continue;
            }
            let _ = match &said {
                Said::Note(text) => writeln!(out, "{text}"),
                Said::Complaint(text) => writeln!(err, "{text}"),
            };
        }

        sleep(config.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::time::Duration;
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use shep_client::shep_core::config::AppConfig;
    use shep_client::shep_core::protocol::ProcessInfo;
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

    /// Runs a git subcommand for fixture setup, panicking if it fails.
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// The config every test that is not about the interval runs on.
    const fn config() -> DogConfig {
        DogConfig {
            interval: Duration::from_secs(30),
            retention: 5,
        }
    }

    /// A target's record, for the one function that reads nothing else.
    fn target(watch: Watch) -> State {
        State {
            remote: "https://example.com/x".to_owned(),
            branch: "main".to_owned(),
            deployed: Some("a1b2c3".to_owned()),
            failed: None,
            verify: Verify::default(),
            watch,
            origin_cwd: None,
            origin_script: None,
            checkout: PathBuf::from("/srv/x"),
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
        let origin = tempfile::tempdir().expect("tempdir");
        git(origin.path(), &["init", "-q", "-b", "main"]);
        git(origin.path(), &["config", "user.email", "test@example.com"]);
        git(origin.path(), &["config", "user.name", "test"]);
        fs::write(origin.path().join("Flockfile.toml"), flockfile(sheep)).expect("Flockfile");
        git(origin.path(), &["add", "."]);
        git(origin.path(), &["commit", "-q", "-m", "first"]);

        let tree = Tree::for_sheep(home, sheep);
        fs::create_dir_all(tree.git()).expect("create the git dir");
        git(&tree.git(), &["init", "-q", "--bare"]);
        let remote = origin.path().to_str().expect("utf-8 path").to_owned();
        crate::git::fetch(&tree.git(), &remote).expect("fetch");

        let first = crate::git::remote_head(&tree.git(), "main").expect("head");
        crate::git::worktree_add(&tree.git(), &tree.release(&first), &first).expect("worktree");
        swap::point_at(&tree.current(), &tree.release(&first)).expect("swap");

        let state = State {
            remote,
            branch: "main".to_owned(),
            deployed: Some(first),
            failed: None,
            verify: Verify::Probed,
            watch,
            origin_cwd: None,
            origin_script: None,
            checkout: origin.path().to_owned(),
        };
        state.write(&tree.state_file()).expect("write deploy.toml");

        // The commit the tick has to notice. Without it every deploy is
        // `UpToDate` and the tests below would pass on a loop that does
        // nothing at all.
        fs::write(origin.path().join("second.txt"), "x").expect("write");
        git(origin.path(), &["add", "."]);
        git(origin.path(), &["commit", "-q", "-m", "second"]);

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
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            unimplemented!()
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            unimplemented!()
        }
        async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            Ok(vec![
                ProcessInfo::builder(0, sheep, ProcStatus::Online)
                    .pid(Some(FIRST_PID + self.reloads.get() * 100))
                    .build(),
            ])
        }
        async fn start(&self, _apps: Vec<AppConfig>) -> Result<(), Error> {
            unimplemented!()
        }
        async fn delete(&self, _id: u32) -> Result<(), Error> {
            unimplemented!()
        }
        async fn reload(&self, _sheep: &str) -> Result<(), Error> {
            self.reloads.set(self.reloads.get() + 1);
            Ok(())
        }
        async fn restart(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
        async fn save_roll(&self) -> Result<PathBuf, Error> {
            unimplemented!()
        }
    }

    /// A shepherd that has gone quiet, counting the times it was asked.
    ///
    /// One `describe` per tick, exactly: a deploy captures the generation
    /// before its reload, that capture is the first thing it asks for, and
    /// this answers it with an error - so the deploy puts `current` back
    /// and gives up before any second question. Nothing is recorded and
    /// nothing moves, so the next tick does the identical thing, which is
    /// what makes the count a tick count.
    ///
    /// A count far above what the interval allows means the loop stopped
    /// sleeping. That is a busy loop with no await point in it, and under a
    /// paused clock it would hang the test rather than fail it, because the
    /// runtime never parks and so never advances time to the timeout. The
    /// panic turns that hang into a red test with a reason on it.
    struct Counting {
        describes: Cell<u32>,
    }

    impl Counting {
        const fn new() -> Self {
            Self {
                describes: Cell::new(0),
            }
        }

        fn ticks(&self) -> u32 {
            self.describes.get()
        }
    }

    impl Daemon for Counting {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            unimplemented!()
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            unimplemented!()
        }
        async fn describe(&self, _sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            self.describes.set(self.describes.get() + 1);
            assert!(
                self.describes.get() <= 60,
                "the loop ticked far more than the interval allows: it is not sleeping"
            );
            Err(Error::Protocol("the shepherd stopped answering".to_owned()))
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
            unimplemented!()
        }
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

        assert!(tick(&Ready::new(), home.path(), config()).await.is_empty());

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
        write_target(home.path(), "broken", Watch::Auto, Some("old"));
        let _origin = write_target_ready(home.path(), "fine", Watch::Auto);

        let results = tick(&Ready::new(), home.path(), config()).await;

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

    /// fails if a tick with no targets at all is an error. That is every
    /// freshly adopted dog, and it must idle quietly rather than logging a
    /// failure every thirty seconds forever.
    #[tokio::test]
    async fn a_dog_with_no_targets_ticks_quietly() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(tick(&Ready::new(), home.path(), config()).await.is_empty());
    }

    /// fails if a deploy directory that cannot be listed is passed over in
    /// silence. Every target is under it, so the dog has stopped deploying
    /// everything - and idling quietly is what a dog with nothing to do
    /// looks like too. One row, named for the directory rather than for a
    /// sheep, because there is no sheep to name.
    #[tokio::test]
    async fn a_deploy_directory_that_cannot_be_listed_is_reported() {
        let home = tempfile::tempdir().expect("tempdir");
        let root = home.path().join("deploy");
        fs::create_dir_all(&root).expect("create the deploy dir");
        // Listable again on drop or not, this test never reads it back.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).expect("chmod");

        let results = tick(&Ready::new(), home.path(), config()).await;

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

        let results = tick(&Ready::new(), home.path(), config()).await;

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
        let _origin = write_target_ready(home.path(), "quiet", Watch::Auto);
        let counter = Counting::new();
        let began = tokio::time::Instant::now();

        // Three ticks' worth, then stop.
        let _ = tokio::time::timeout(
            Duration::from_secs(305),
            run(
                &counter,
                home.path(),
                DogConfig {
                    interval: Duration::from_secs(150),
                    retention: 5,
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
        let _origin = write_target_ready(home.path(), "quiet", Watch::Auto);
        let counter = Counting::new();

        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            run(
                &counter,
                home.path(),
                DogConfig {
                    interval: Duration::from_secs(600),
                    retention: 5,
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

        assert!(worth_saying(
            &mut previous,
            "web",
            "web: the remote is gone"
        ));
        assert!(!worth_saying(
            &mut previous,
            "web",
            "web: the remote is gone"
        ));
        assert!(!worth_saying(
            &mut previous,
            "web",
            "web: the remote is gone"
        ));
        // A different target is a different line.
        assert!(worth_saying(
            &mut previous,
            "koji",
            "koji: the remote is gone"
        ));
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
            assert!(worth_saying(&mut previous, "web", line), "{line}");
            assert!(!worth_saying(&mut previous, "web", line), "{line}");
        }
    }

    /// fails if a condition that cleared stays muted, so that its coming
    /// back is never mentioned. A line that differs is new information, and
    /// a target that has started working again is the clearest case of it.
    #[test]
    fn a_line_that_changes_is_said_again() {
        let mut previous = BTreeMap::new();
        let broken = "web: the remote is gone";

        assert!(worth_saying(&mut previous, "web", broken));
        assert!(!worth_saying(&mut previous, "web", broken));
        assert!(worth_saying(&mut previous, "web", "web deployed abc1234"));
        assert!(worth_saying(&mut previous, "web", broken));
    }

    /// fails if a muted line is muted forever. A dog runs for months, and a
    /// condition mentioned once in a log that has since rotated is a
    /// condition nobody can find. Said again every `RESAY` ticks, which is
    /// an hour at the default interval.
    #[test]
    fn a_muted_line_is_said_again_eventually() {
        let mut previous = BTreeMap::new();
        let line = "web: the remote is gone";

        assert!(worth_saying(&mut previous, "web", line));
        let said = (0..RESAY * 2)
            .filter(|_| worth_saying(&mut previous, "web", line))
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
        let head = head_of(origin.path());
        let (mut out, mut err) = (Vec::new(), Vec::new());

        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            run_with(
                &Ready::new(),
                home.path(),
                DogConfig {
                    interval: Duration::from_secs(600),
                    retention: 5,
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
        write_target(home.path(), "broken", Watch::Auto, Some("old"));
        let (mut out, mut err) = (Vec::new(), Vec::new());

        // Three ticks' worth, as the interval test above.
        let _ = tokio::time::timeout(
            Duration::from_secs(305),
            run_with(
                &Ready::new(),
                home.path(),
                DogConfig {
                    interval: Duration::from_secs(150),
                    retention: 5,
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
}
