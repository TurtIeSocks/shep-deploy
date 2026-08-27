//! The tier that drives a REAL shepherd.
//!
//! Every module under `src/` is unit-tested against a fake [`Daemon`] or a
//! throwaway git fixture. None of that proves the binary can actually talk
//! to a shepherd - reload a real sheep, read a real `describe`, or leave a
//! real release serving when a build fails. That is what this file is for.
//!
//! Gated behind the `integration` feature, and needing `$SHEP_BIN` pointed at
//! a built `shep`:
//!
//! ```text
//! SHEP_BIN="$(command -v shep)" cargo test -p shep-deploy --features integration
//! ```
//!
//! `shep-deploy` is a binary crate with no library, so this file cannot call
//! `crate::deploy::deploy` the way the unit tests in `src/deploy.rs` do -
//! everything here goes through the two real binaries, `shep` and
//! `shep-deploy` itself, exactly as an operator would run them.
//!
//! # $SHEP_HOME is a temporary directory in every test here
//!
//! Not because a developer's real `~/.shep` is production - it is not, on
//! this machine - but for the ordinary reason any test suite isolates its
//! fixtures: tests run in parallel, and two tests sharing one `$SHEP_HOME`
//! would collide on the same socket, the same `deploy/web` tree, and the
//! same flock entry. A test that depends on another test's leftover state is
//! not a test. Every [`Shepherd`] below owns its own `tempfile::tempdir` and
//! kills the daemon it booted when it drops.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// The `shep-deploy` binary under test, as cargo built it for this run.
const DEPLOY_BIN: &str = env!("CARGO_BIN_EXE_shep-deploy");

/// How long any "wait until the world catches up" poll gets before it gives
/// up and fails the test.
const PATIENCE: Duration = Duration::from_secs(60);

/// `src/main.rs`'s `ROLLED_BACK`, restated because a test binary cannot see
/// a binary crate's internals. A change to one without the other fails this
/// test, which is the point.
const ROLLED_BACK_EXIT: u8 = 12;

/// The `shep` binary under test.
///
/// # Panics
/// If `$SHEP_BIN` is unset or does not point at a file. Loudly, rather than
/// skipping: a tier that quietly does nothing is the failure mode this whole
/// file exists to avoid.
fn shep_bin() -> PathBuf {
    let raw = std::env::var("SHEP_BIN").expect(
        "the integration tier needs $SHEP_BIN pointing at a built shep binary, for example \
         SHEP_BIN=\"$(command -v shep)\"",
    );
    let path = PathBuf::from(raw);
    assert!(
        path.is_file(),
        "$SHEP_BIN does not name a file: {}",
        path.display()
    );
    path
}

/// One shepherd in its own temporary `$SHEP_HOME`, killed on drop.
struct Shepherd {
    home: tempfile::TempDir,
    shep: PathBuf,
}

impl Shepherd {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("a temporary $SHEP_HOME");
        // A unix socket path is bounded by the kernel: 104 bytes on macOS,
        // 108 on Linux. `$TMPDIR` is long on macOS, so this is close enough
        // to the limit to be worth saying out loud rather than discovering
        // as "the daemon process exited before it started answering".
        let socket = home.path().join("run/shep.sock");
        assert!(
            socket.as_os_str().len() < 100,
            "$TMPDIR is too deep for a unix socket here: {} is {} bytes and the kernel allows \
             about 104. Run with a shorter TMPDIR.",
            socket.display(),
            socket.as_os_str().len()
        );
        Self {
            home,
            shep: shep_bin(),
        }
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    /// Run one `shep` command against this home and hand back its output.
    ///
    /// `SHEP_HOME` goes in the environment as well as in `--home`, matching
    /// `shep-log-rotate`'s own integration tier: `shep adopt` vets a
    /// candidate binary by spawning it with this process's environment
    /// inherited, and a missing `SHEP_HOME` there would point that spawn at
    /// whatever `$SHEP_HOME` this test process itself has.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(&self.shep)
            .args(args)
            .arg("--home")
            .arg(self.home())
            .env("SHEP_HOME", self.home())
            .output()
            .expect("shep ran")
    }

    /// Run one `shep` command and require it to succeed.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "shep {args:?} failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Run `shep-deploy deploy <sheep>` against this home, exactly as the
    /// supervised dog or an operator's direct invocation would.
    fn deploy(&self, sheep: &str) -> Output {
        self.deploy_args(&["deploy", sheep])
    }

    /// Run `shep-deploy` against this home with whatever argv is given.
    ///
    /// `SHEP_HOME` and nothing else, which is the whole of a dog's
    /// environment - a verb that needed more than that would work here and
    /// fail under the shepherd.
    fn deploy_args(&self, args: &[&str]) -> Output {
        Command::new(DEPLOY_BIN)
            .args(args)
            .env("SHEP_HOME", self.home())
            .output()
            .expect("shep-deploy ran")
    }
}

impl Drop for Shepherd {
    fn drop(&mut self) {
        // Failures ignored: a test that already failed must report its own
        // reason, not this one.
        let _ = self.run(&["kill", "--style", "bare"]);
    }
}

/// Runs a git subcommand for fixture setup, panicking if it fails.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
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

/// The `web` app as both Flockfiles here declare it: script, and a
/// readiness probe that looks THROUGH the `current` symlink for a file a
/// release either ships or does not.
///
/// `with_cwd` is the one difference between the two copies, and it is not
/// cosmetic - see [`register_web`].
///
/// Every test in this file needs a probe, because `Verify::Probed` refuses
/// a target without one and refuses it by reading the RELEASE's Flockfile,
/// which is the only app definition the dog can see: `describe` reports
/// status and pid, never the config shep registered. The two copies can
/// therefore disagree, and this fixture keeps them honest by generating
/// both from here.
fn app_toml(home: &Path, with_cwd: bool, readiness: Readiness, extra: &str) -> String {
    let current = home.join("deploy/web/current");
    let cwd = if with_cwd {
        format!("cwd = {:?}\n", current.to_str().expect("utf-8 path"))
    } else {
        String::new()
    };
    let gate = match readiness {
        Readiness::Probe => format!(
            "\n[app.readiness_probe]\nkind = \"exec\"\ntarget = \"test -f \
             {marker}\"\ninterval = \"1s\"\ntimeout = \"2s\"\nfailure_threshold = 1\n",
            marker = current.join("ready-marker").display(),
        ),
        Readiness::Heuristic(listen) => format!("listen_timeout = \"{listen}s\"\n"),
    };
    format!("[[app]]\nname = \"web\"\nscript = \"./run.sh\"\n{cwd}{extra}{gate}")
}

/// How a test's app reports itself ready.
#[derive(Clone, Copy)]
enum Readiness {
    /// An exec probe through `current`, which is what the deploy sequence
    /// wants and what `verify = "probed"` requires.
    Probe,
    /// Nothing at all, which is what most real apps have. shep falls back
    /// to sleeping the whole of the `listen_timeout` carried here, per
    /// instance, and that is the case the verification window has to be
    /// derived for.
    Heuristic(u64),
}

/// The `listen_timeout` the slow probeless fixture app declares, in
/// seconds.
///
/// Longer than the ten-second window `Verify::Alive` used to be fixed at, on
/// purpose: shorter and the reload fits inside the old window, and the test
/// below cannot fail against the code it exists to pin.
const SLOW_LISTEN: u64 = 12;

/// A bare git origin with one committed app named `web`, whose script echoes
/// `version` to stdout and then sleeps, and which carries the marker file
/// the readiness probe looks for.
///
/// The version marker is how a test tells "the reload actually ran the new
/// release" apart from "the old process is still running" - both look
/// identical from `shep describe` alone, since `describe` only reports
/// status and pid, not which release's code is executing.
///
/// `ready-marker` is the other half. The probe is `test -f
/// <home>/deploy/web/current/ready-marker`, so it resolves through the
/// symlink the deploy swaps: a release that ships the marker can become
/// ready and one that does not never can. That is how a test makes a real
/// shepherd's `AwaitReady` fail on purpose, which is the only way to
/// exercise a rollback end to end.
fn origin_with_app(
    home: &Path,
    version: &str,
    readiness: Readiness,
    extra: &str,
) -> tempfile::TempDir {
    let origin = tempfile::tempdir().expect("tempdir");
    git(origin.path(), &["init", "-q", "-b", "main"]);
    git(origin.path(), &["config", "user.email", "test@example.com"]);
    git(origin.path(), &["config", "user.name", "test"]);
    fs::write(
        origin.path().join("Flockfile.toml"),
        app_toml(home, false, readiness, extra),
    )
    .expect("write Flockfile");
    fs::write(origin.path().join("ready-marker"), "").expect("write ready-marker");
    write_run_script(origin.path(), version);
    git(origin.path(), &["add", "."]);
    git(origin.path(), &["commit", "-q", "-m", version]);
    origin
}

/// The app config for the drain-window test: two instances, a short
/// readiness wait, and shep's default drain window stated outright so the
/// arithmetic in the test is readable next to it.
///
/// Two instances because shep swaps them strictly one at a time, so the
/// whole reload costs twice one swap - and [`crate::verify`]'s turnover
/// needs every old instance gone, drain included.
const DRAINING_APP: &str = "instances = 2\ngraceful_timeout = \"8s\"\n";

/// The `listen_timeout` that app declares. Short, so the test spends its
/// time in the drain rather than in readiness.
const DRAINING_LISTEN: u64 = 1;

/// Overwrites `dir`'s `run.sh` to echo `version` before sleeping.
fn write_run_script(dir: &Path, version: &str) {
    write_script(dir, &format!("#!/bin/sh\necho {version}\nsleep 300\n"));
}

/// As [`write_run_script`], but for an app that uses its whole drain
/// window: it ignores `SIGTERM` and keeps running until shep's
/// `graceful_timeout` expires and the `SIGKILL` lands.
///
/// The loop matters as much as the trap. `trap '' TERM` makes the shell
/// itself ignore the signal, but a `sleep 300` child that is signalled too
/// would end the script early and the drain with it; re-sleeping in a loop
/// means the only thing that can end this process is the kill.
fn write_stubborn_run_script(dir: &Path, version: &str) {
    write_script(
        dir,
        &format!("#!/bin/sh\ntrap '' TERM\necho {version}\nwhile :; do sleep 1; done\n"),
    );
}

/// Writes `body` as `dir/run.sh`, executable.
fn write_script(dir: &Path, body: &str) {
    let path = dir.join("run.sh");
    fs::write(&path, body).expect("write run.sh");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// A deploy tree for `sheep`, at `home/deploy/<sheep>`, matching
/// `crate::paths::Tree`'s own layout: `git` fetched from `origin`, one
/// release built for `origin`'s current head, and `current` pointed at it.
///
/// Returns the sha that release was built for.
fn build_tree(home: &Path, sheep: &str, origin: &Path) -> String {
    let root = home.join("deploy").join(sheep);
    let git_dir = root.join("git");
    fs::create_dir_all(&git_dir).expect("create git dir");
    git(&git_dir, &["init", "-q", "--bare"]);

    let remote = origin.to_str().expect("utf-8 origin path").to_owned();
    git(
        &git_dir,
        &["fetch", "--prune", &remote, "+refs/heads/*:refs/heads/*"],
    );
    let sha = head_of(origin);

    let release = root.join("releases").join(&sha);
    git(
        &git_dir,
        &["worktree", "add", release.to_str().unwrap(), &sha],
    );
    symlink(&release, root.join("current")).expect("symlink current");

    sha
}

/// Writes `deploy.toml` for `sheep`, tracking `origin`'s `main` branch,
/// already deployed at `sha`, and verifying the way `verify` says.
///
/// Field order and shape matches `crate::state::State`'s own TOML. `watch`
/// is left unset so it takes its documented default (`Auto`), the same
/// round trip `src/state.rs`'s own tests pin.
fn write_state(home: &Path, sheep: &str, origin: &Path, sha: &str, verify: &str) {
    let path = home.join("deploy").join(sheep).join("deploy.toml");
    fs::write(
        &path,
        format!(
            "remote = {remote:?}\nbranch = \"main\"\ndeployed = {sha:?}\nverify = \
             {verify:?}\ncheckout = {remote:?}\n",
            remote = origin.to_str().expect("utf-8 origin path"),
        ),
    )
    .expect("write deploy.toml");
}

/// Registers `web` with the shepherd, from a Flockfile OUTSIDE any release,
/// whose `cwd` is the `current` symlink.
///
/// **`cwd` is set explicitly, and to the symlink.** A Flockfile app whose
/// `cwd` is left to default takes its own directory - and shep resolves
/// that to a real path when it registers the app. Registering from
/// `<home>/deploy/web/current/Flockfile.toml` therefore pins the sheep to
/// the release that happened to be current at registration, and every later
/// deploy swaps a symlink the running app no longer reaches. Measured on a
/// real shepherd: the reload after a swap re-ran the OLD release's script.
/// An explicit `cwd` is stored verbatim, symlink and all, which is what the
/// design means by "the sheep's `cwd` is this path, permanently".
///
/// **The probe is registered, not deployed.** Nothing in this crate ever
/// re-registers the app, so the probe a reload actually uses is this one
/// and not whatever the new release's Flockfile says. A test that wants a
/// reload to fail therefore cannot change the probe; it changes what the
/// probe LOOKS at, which is why the probe points through `current` at a
/// file a release either ships or does not.
fn register_web(shepherd: &Shepherd, readiness: Readiness, extra: &str) {
    let path = shepherd.home().join("register.toml");
    fs::write(&path, app_toml(shepherd.home(), true, readiness, extra))
        .expect("write register.toml");
    shepherd.ok(&[
        "start",
        path.to_str().expect("utf-8 path"),
        "--style",
        "bare",
    ]);
}

/// The `listen_timeout` the pre-adoption checkout registration declares, in
/// seconds. Short: nothing in that phase is being measured, and the test
/// spends the wait before it can do anything else.
const CHECKOUT_LISTEN: u64 = 1;

/// Registers `web` the way an operator already runs it, before this dog has
/// touched anything: from their OWN clone of `origin`, out of a Flockfile
/// with **no `cwd` key at all**.
///
/// The missing `cwd` is the point rather than a shortcut. It is the
/// ordinary way an app is registered, and it is precisely the case the
/// cutover's explicit `cwd` exists for: shep resolves a defaulted `cwd` to
/// the Flockfile's own directory at REGISTRATION, so the sheep this starts
/// is pinned to this checkout and [`crate::optin::cut_over`] has to re-point
/// it at the `current` symlink itself.
///
/// The clone is separate from `origin` on purpose. This Flockfile is
/// probeless, because the probe the committed one carries looks through a
/// `current` symlink that does not exist yet and the sheep could never come
/// up; leaving that edit in `origin`'s working tree would then get committed
/// by the next `git commit -a` and ship a release the deploy path refuses
/// for having no probe.
///
/// Returns the clone, which the caller has to keep alive: it is the
/// registered sheep's working directory until the cutover moves it.
fn register_from_checkout(shepherd: &Shepherd, origin: &Path) -> tempfile::TempDir {
    let checkout = tempfile::tempdir().expect("tempdir");
    git(
        checkout.path(),
        &[
            "clone",
            "-q",
            origin.to_str().expect("utf-8 origin path"),
            ".",
        ],
    );
    let path = checkout.path().join("Flockfile.toml");
    fs::write(
        &path,
        app_toml(
            shepherd.home(),
            false,
            Readiness::Heuristic(CHECKOUT_LISTEN),
            "",
        ),
    )
    .expect("write the checkout Flockfile");
    shepherd.ok(&[
        "start",
        path.to_str().expect("utf-8 path"),
        "--style",
        "bare",
    ]);
    checkout
}

/// The last non-empty line of `path`, or `None` if it cannot be read yet.
///
/// The app's stdout log is how these tests tell which RELEASE is executing,
/// which `shep describe` cannot answer: it reports status and pid, not code.
/// The last line specifically, because the log accumulates across reloads -
/// including the short-lived instance of a release that failed to come up.
fn last_line(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .rfind(|line| !line.trim().is_empty())
        .map(str::to_owned)
}

/// Poll `ready` until it answers true, or fail with `what`.
fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

/// Whether `shep describe` reports `sheep` as `online`.
///
/// A pid alone is not enough for the probeless tests: shep hands out a pid
/// the moment it spawns and only calls the sheep `online` once its readiness
/// path is satisfied, which for those is a whole `listen_timeout` later.
fn described_online(shepherd: &Shepherd, sheep: &str) -> bool {
    shepherd
        .ok(&["describe", sheep, "--format", "json"])
        .contains("\"status\":\"online\"")
}

/// The pid `shep describe` reports for `sheep`, or `None` if it has none or
/// cannot be found at all.
fn described_pid(shepherd: &Shepherd, sheep: &str) -> Option<u32> {
    let listing = shepherd.ok(&["describe", sheep, "--format", "json"]);
    // No JSON dependency in this crate - a pid is the only field these tests
    // need, and it is unambiguous to find as text: `"pid":1234`.
    listing
        .split("\"pid\":")
        .nth(1)?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// How many instances `shep describe` reports under `sheep`.
///
/// Counted as `"id":` occurrences, the same shape [`described_pid`] reads a
/// pid with and for the same reason - no JSON dependency in this crate. One
/// per instance and nothing else in the listing carries that key: a lamb has
/// a pid and a name, never an id.
fn described_instances(shepherd: &Shepherd, sheep: &str) -> usize {
    shepherd
        .ok(&["describe", sheep, "--format", "json"])
        .matches("\"id\":")
        .count()
}

/// The stdout log shep is writing for `sheep` right now, as `describe`
/// itself names it.
///
/// Read from the listing rather than assembled from a slot number, because
/// the slot is not predictable across a cutover. Measured on a real
/// shepherd: `Start` on a registered name adds the newcomer BESIDE the
/// original, so the newcomer takes slot 1 and keeps it after the original is
/// deleted. A helper hard-coding `<sheep>-0-out.log` would go on reading the
/// log of the instance the cutover removed, whose last line is whatever it
/// was serving when it died.
///
/// A later reload does not move it - the instance id changes and the slot
/// does not - so one read before a deploy is good for the assertions after
/// it.
fn out_file(shepherd: &Shepherd, sheep: &str) -> PathBuf {
    let listing = shepherd.ok(&["describe", sheep, "--format", "json"]);
    let named = listing
        .split("\"out_file\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("describe names an out_file");
    PathBuf::from(named)
}

/// fails if a deploy of a fixture repo does not reach the running sheep at
/// all. This is the property no unit test can prove: every module below
/// `main` is exercised against a fake `Daemon`, and a fake can never be
/// wrong about whether `reload` actually replaces the process shep is
/// supervising.
#[test]
fn a_real_deploy_swaps_reloads_and_verifies() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(shepherd.home(), "v1", Readiness::Probe, "");
    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first, "probed");

    register_web(&shepherd, Readiness::Probe, "");
    wait_until("the first release to come online", || {
        described_pid(&shepherd, "web").is_some()
    });
    let out_log = shepherd.home().join("logs/web-0-out.log");
    wait_until("v1 to have run at least once", || {
        fs::read_to_string(&out_log)
            .unwrap_or_default()
            .contains("v1")
    });

    write_run_script(origin.path(), "v2");
    git(origin.path(), &["add", "."]);
    git(origin.path(), &["commit", "-q", "-m", "v2"]);
    let second = head_of(origin.path());

    let output = shepherd.deploy("web");
    assert!(
        output.status.success(),
        "deploy failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&second),
        "the deploy should report the new sha: {stdout}"
    );

    // `current` really moved to the new release...
    let current = fs::read_link(shepherd.home().join("deploy/web/current")).expect("current");
    assert_eq!(
        current,
        shepherd.home().join("deploy/web/releases").join(&second)
    );

    // ...deploy.toml records it...
    let state = fs::read_to_string(shepherd.home().join("deploy/web/deploy.toml")).expect("state");
    assert!(state.contains(&second), "{state}");

    // ...and the shepherd is actually running the new release's code, not
    // just pointing at it on disk.
    wait_until("v2 to have run", || {
        fs::read_to_string(&out_log)
            .unwrap_or_default()
            .contains("v2")
    });
}

/// fails if a failing build ever reaches the running app. Steps one through
/// five must never touch `current` - this is the property the whole design
/// rests on, and no unit test built out of a fake daemon can prove it,
/// because a fake `reload` can never accidentally run against a real
/// process.
#[test]
fn a_failing_build_leaves_the_previous_release_serving() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(shepherd.home(), "v1", Readiness::Probe, "");
    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first, "probed");

    register_web(&shepherd, Readiness::Probe, "");
    wait_until("the first release to come online", || {
        described_pid(&shepherd, "web").is_some()
    });
    let live_pid = described_pid(&shepherd, "web").expect("a pid");

    fs::write(
        origin.path().join("Flockfile.toml"),
        format!(
            "{}\n[build]\ncommand = 'exit 3'\n",
            app_toml(shepherd.home(), false, Readiness::Probe, "")
        ),
    )
    .expect("write a failing build");
    git(origin.path(), &["add", "."]);
    git(origin.path(), &["commit", "-q", "-m", "broken"]);

    let output = shepherd.deploy("web");
    assert!(
        !output.status.success(),
        "a failing build must not be reported as a successful deploy"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("build exited with status 3"),
        "the failure should name the build's own exit status: {stderr}"
    );

    // `current` never moved...
    let current = fs::read_link(shepherd.home().join("deploy/web/current")).expect("current");
    assert_eq!(
        current,
        shepherd.home().join("deploy/web/releases").join(&first)
    );

    // ...deploy.toml still names the first release...
    let state = fs::read_to_string(shepherd.home().join("deploy/web/deploy.toml")).expect("state");
    assert!(state.contains(&first), "{state}");

    // ...and the shepherd never reloaded: the running instance is the exact
    // same process, not a fresh one spawned onto the same, unmoved release.
    let pid_after = described_pid(&shepherd, "web").expect("still running");
    assert_eq!(
        live_pid, pid_after,
        "a failed build must never trigger a reload of the running sheep"
    );
}

/// fails if a release that cannot come up is left serving. This is the
/// property the whole crate exists for, and it is the one no mock can
/// prove: `Request::Reload` is an ACCEPTANCE, not a completion, and when
/// shep's own `AwaitReady` fails it keeps the old instance serving. A fake
/// daemon answers `describe` however the test wants and so can never
/// reproduce either fact. Verification that reads status alone passes here
/// on its first poll, reports the broken release deployed, and leaves
/// `current` pointing at it.
///
/// The release breaks by deleting `ready-marker`, which is what the
/// registered readiness probe looks for through the `current` symlink - see
/// [`register_web`]. Nothing about the app changes; the file its probe
/// tests for stops being there.
///
/// `verify = "alive"` rather than the default `probed`, and NOT for speed:
/// the two modes get the same budget now that both floors are gone, and
/// `alive` is the slower of the two by its ten-second dwell. What it buys
/// is coverage. `probed` is what every other test at this tier runs, so
/// this is the only place a real shepherd exercises the `alive` path -
/// including the dwell, which no other test reaches at all.
#[test]
fn a_release_that_cannot_come_up_is_rolled_back_and_the_old_release_serves() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(shepherd.home(), "v1", Readiness::Probe, "");
    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first, "alive");
    register_web(&shepherd, Readiness::Probe, "");

    wait_until("the first release to come online", || {
        described_pid(&shepherd, "web").is_some()
    });
    let out_log = shepherd.home().join("logs/web-0-out.log");
    wait_until("v1 to have run", || {
        last_line(&out_log).as_deref() == Some("v1")
    });

    // A release that can never become ready: the probe looks through
    // `current` for a file this commit removes.
    write_run_script(origin.path(), "v2");
    git(origin.path(), &["rm", "-q", "ready-marker"]);
    git(origin.path(), &["add", "-A"]);
    git(origin.path(), &["commit", "-q", "-m", "v2, never ready"]);
    let second = head_of(origin.path());

    let output = shepherd.deploy("web");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("rolled back"),
        "a release that never came up must be reported as rolled back: {stdout}{stderr}"
    );
    assert!(
        !stdout.contains(&format!("deployed {second}")),
        "the broken release must never be reported deployed: {stdout}"
    );
    // The rollback reached the operator's shell, not just the operator's
    // eyes. A unit test proves the mapping; only a real deploy proves the
    // mapping is on the path a rollback actually takes, and this one goes
    // through `Ok(Outcome::RolledBack)`, which exited 0 until today.
    assert_eq!(
        output.status.code(),
        Some(i32::from(ROLLED_BACK_EXIT)),
        "a rolled-back deploy must not report success: {stdout}{stderr}"
    );

    // `current` points at the old release again...
    let current = fs::read_link(shepherd.home().join("deploy/web/current")).expect("current");
    assert_eq!(
        current,
        shepherd.home().join("deploy/web/releases").join(&first),
        "current must be back on the release that works"
    );

    // ...deploy.toml still names it as what is deployed, never the release
    // that failed. The failed sha IS in the file, as the sha the poll loop
    // holds on until the branch moves, so this asks about the key rather
    // than about the file.
    let state = fs::read_to_string(shepherd.home().join("deploy/web/deploy.toml")).expect("state");
    assert!(
        state.contains(&format!("deployed = \"{first}\"")),
        "{state}"
    );
    assert!(
        !state.contains(&format!("deployed = \"{second}\"")),
        "{state}"
    );
    assert!(state.contains(&format!("failed = \"{second}\"")), "{state}");

    // ...and the process the shepherd is supervising is running the old
    // release's code. This is the assertion that fails before generation
    // aware verification: the deploy reported success, `current` stayed on
    // the broken release, and the last thing to run was v2.
    wait_until("the old release to be serving again", || {
        last_line(&out_log).as_deref() == Some("v1")
    });
    assert!(
        described_pid(&shepherd, "web").is_some(),
        "the sheep must still be running after a rollback"
    );
}

/// fails if a reload that takes longer than the verification window is
/// treated as a release that failed. This is the case that made `Alive`
/// destructive rather than merely wrong: an app with no readiness gate takes
/// shep's heuristic path, which sleeps its whole `listen_timeout` per
/// instance, so a reload legitimately outlives any window fixed in advance.
///
/// What happened with the window fixed at ten seconds and `listen_timeout`
/// at twelve, measured, deploying a release that was fine:
///
/// ```text
/// shep-deploy: rolling back after it did not come up within 10s ... failed:
/// the daemon reported Internal: web is already being reloaded
/// current   -> releases/<v1>      deployed = "<v1>"      log -> v1 v2
/// ```
///
/// Three failures in sequence: verification gave up on a healthy reload, the
/// rollback's own reload was refused because the first was still running,
/// and what was left was the split state the rollback exists to prevent.
/// Nothing repaired it, and the next poll tried again.
///
/// No mock can reproduce this. It needs a real shepherd's readiness
/// heuristic, a real reload that takes real time, and a real refusal of the
/// second reload.
#[test]
fn a_reload_slower_than_the_old_window_still_deploys() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(shepherd.home(), "v1", Readiness::Heuristic(SLOW_LISTEN), "");
    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first, "alive");
    register_web(&shepherd, Readiness::Heuristic(SLOW_LISTEN), "");

    wait_until("the first release to come online", || {
        described_online(&shepherd, "web")
    });
    let out_log = shepherd.home().join("logs/web-0-out.log");
    wait_until("v1 to have run", || {
        last_line(&out_log).as_deref() == Some("v1")
    });

    write_run_script(origin.path(), "v2");
    git(origin.path(), &["add", "-A"]);
    git(origin.path(), &["commit", "-q", "-m", "v2"]);
    let second = head_of(origin.path());

    let output = shepherd.deploy("web");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a good release must not fail its own deploy: {stdout}{stderr}"
    );
    assert!(
        stdout.contains(&format!("deployed {second}")),
        "the new release must be reported deployed: {stdout}{stderr}"
    );
    assert!(
        !stdout.contains("rolled back") && !stderr.contains("split"),
        "nothing here should roll back: {stdout}{stderr}"
    );

    let current = fs::read_link(shepherd.home().join("deploy/web/current")).expect("current");
    assert_eq!(
        current,
        shepherd.home().join("deploy/web/releases").join(&second)
    );

    let state = fs::read_to_string(shepherd.home().join("deploy/web/deploy.toml")).expect("state");
    assert!(state.contains(&second), "{state}");

    // And the process really is on the new release, which is the half of
    // "deployed" that a symlink cannot prove.
    assert_eq!(
        last_line(&out_log).as_deref(),
        Some("v2"),
        "the running process must be executing the new release"
    );
}

/// fails if the verification window stops covering a reload that spends its
/// whole drain window. This is the term three rounds of derivation left out:
/// shep bounds one swap at `listen_timeout + graceful_timeout +
/// RELOAD_DEADLINE_SLACK` (`arm_reload_deadline`, `supervisor.rs:3581`) and
/// swaps instances one at a time, and `crate::verify` needs every OLD
/// instance gone before it calls the deploy verified - so the drain is
/// inside the window this crate has to wait out, not outside it.
///
/// The shipping defaults are the wrong way round for a derivation built on
/// `listen_timeout` alone: three seconds of readiness against eight of
/// drain. This app makes that gap wider still and then refuses to die
/// politely, so a reload takes about eighteen seconds where a window of
/// `listen_timeout x instances x 2` would have been four.
///
/// This is also the only test at any tier that runs a multi-instance
/// reload.
#[test]
fn a_reload_that_uses_its_whole_drain_window_still_deploys() {
    let shepherd = Shepherd::new();
    let readiness = Readiness::Heuristic(DRAINING_LISTEN);
    let origin = origin_with_app(shepherd.home(), "v1", readiness, DRAINING_APP);
    write_stubborn_run_script(origin.path(), "v1");
    git(origin.path(), &["add", "-A"]);
    git(origin.path(), &["commit", "-q", "-m", "stubborn v1"]);

    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first, "alive");
    register_web(&shepherd, readiness, DRAINING_APP);

    wait_until("both instances to come online", || {
        described_online(&shepherd, "web")
    });
    let out_log = shepherd.home().join("logs/web-0-out.log");
    wait_until("v1 to have run", || {
        last_line(&out_log).as_deref() == Some("v1")
    });

    write_stubborn_run_script(origin.path(), "v2");
    git(origin.path(), &["add", "-A"]);
    git(origin.path(), &["commit", "-q", "-m", "stubborn v2"]);
    let second = head_of(origin.path());

    let started = Instant::now();
    let output = shepherd.deploy("web");
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "a healthy reload that used its drain window must not fail its own \
         deploy: {stdout}{stderr}"
    );
    assert!(
        stdout.contains(&format!("deployed {second}")),
        "the new release must be reported deployed: {stdout}{stderr}"
    );
    assert!(
        !stdout.contains("rolled back") && !stderr.contains("split"),
        "nothing here should roll back: {stdout}{stderr}"
    );

    // The fixture must actually have spent the drain, or this test would
    // pass for the wrong reason the day the trap stops working. Two
    // instances at eight seconds each, minus room for the machine.
    assert!(
        elapsed >= Duration::from_secs(14),
        "the app cannot have used its drain window in {elapsed:?} - this test \
         is no longer testing the term it exists for"
    );

    let current = fs::read_link(shepherd.home().join("deploy/web/current")).expect("current");
    assert_eq!(
        current,
        shepherd.home().join("deploy/web/releases").join(&second)
    );

    let state = fs::read_to_string(shepherd.home().join("deploy/web/deploy.toml")).expect("state");
    assert!(state.contains(&second), "{state}");

    assert_eq!(
        last_line(&out_log).as_deref(),
        Some("v2"),
        "the running process must be executing the new release"
    );
}

/// fails if a sheep taken over by `setup` does not follow later swaps.
///
/// This is the fact no fake can check and the one that fails silently
/// exactly one release after the mistake: a Flockfile app's DEFAULTED cwd is
/// resolved at REGISTRATION, so a sheep registered from inside a release is
/// pinned to that release forever and every later swap moves a symlink it no
/// longer reaches. Measured on a real shepherd before this crate existed:
/// the reload after a swap re-ran the OLD release's script.
///
/// It deploys TWICE for that reason. One deploy proves nothing here, because
/// a sheep pinned to release one and a sheep following `current` are
/// indistinguishable while release one IS current.
///
/// It also pins two things no unit test can reach: that `Start` on a
/// registered name adds an instance beside the old one rather than replacing
/// it, and that the cutover deleted the ORIGINAL instance and not the
/// newcomer. Both are real rows under one name on a real shepherd, and an id
/// read from the wrong side of the `Start` deletes the release that was just
/// deployed while every count assertion still passes. The surviving pid is
/// what tells them apart.
#[test]
fn a_sheep_taken_over_by_setup_follows_a_later_swap() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(shepherd.home(), "v1", Readiness::Probe, "");

    // The sheep as the operator already runs it: registered from their own
    // checkout, with no deploy tree anywhere. Held, because it is that
    // sheep's working directory until the cutover moves it.
    let _checkout = register_from_checkout(&shepherd, origin.path());
    wait_until("web to come up from the checkout", || {
        described_online(&shepherd, "web")
    });
    let before = described_pid(&shepherd, "web").expect("a pid");

    let setup = shepherd.deploy_args(&["setup", "web"]);
    assert!(
        setup.status.success(),
        "setup failed: {}{}",
        String::from_utf8_lossy(&setup.stdout),
        String::from_utf8_lossy(&setup.stderr)
    );

    // One instance again, and it must be the NEWCOMER. Two rows shared this
    // name for the length of the cutover, so this is the assertion that
    // catches an id taken from the wrong side of the `Start`.
    wait_until("the cutover to settle", || {
        described_instances(&shepherd, "web") == 1
    });
    assert_ne!(
        described_pid(&shepherd, "web"),
        Some(before),
        "the surviving instance must be the newcomer, not the one it replaced"
    );

    // Read after the cutover settled, so it names the newcomer's log rather
    // than the deleted original's.
    let out_log = out_file(&shepherd, "web");

    // Now the half a single deploy cannot prove.
    write_run_script(origin.path(), "v2");
    git(origin.path(), &["commit", "-qam", "v2"]);
    let deployed = shepherd.deploy("web");
    assert!(
        deployed.status.success(),
        "the second deploy failed: {}{}",
        String::from_utf8_lossy(&deployed.stdout),
        String::from_utf8_lossy(&deployed.stderr)
    );

    wait_until("the second release to be serving", || {
        last_line(&out_log).as_deref() == Some("v2")
    });
}

/// fails if the supervised mode does not deploy on its own.
///
/// `watch = "auto"` means nothing until the binary, given NO ARGV AT ALL as
/// the dog contract requires, connects, works out its own adopted name from
/// its own pid in a real flock listing, reads its own `[dog.<name>]`
/// section, finds its targets on disk and deploys one. Every one of those is
/// stubbed in the unit tests, and the pid lookup in particular can only be
/// wrong against a real daemon.
///
/// `shep.toml` is written after the shepherd is already up, which is
/// deliberate and safe: the daemon re-reads a dog's section per
/// `Request::DogConfig` rather than caching one at boot.
///
/// **The dog's own stdout is what settles who deployed**, and it is not
/// belt and braces. `shep adopt` vets a candidate by spawning it with no
/// argv and the real `$SHEP_HOME` before it records anything, so for about
/// fifty milliseconds a second copy of this binary is running the same poll
/// loop against the same targets. That copy is killed with its stdio on
/// `/dev/null`, so a line in `logs/deploy-0-out.log` can only have come from
/// the instance the shepherd supervises. Asserting on the running release
/// alone would not tell the two apart, and neither would the mutation this
/// test is checked with, because the vetting spawn runs the mutated loop
/// too.
#[test]
fn the_supervised_dog_deploys_a_moved_branch_without_being_asked() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(shepherd.home(), "v1", Readiness::Probe, "");
    let sha = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &sha, "probed");
    register_web(&shepherd, Readiness::Probe, "");
    wait_until("web to come up", || described_online(&shepherd, "web"));
    let out_log = out_file(&shepherd, "web");

    // A one-second interval, so a tick that finds nothing the first time
    // round costs a second rather than the default half minute.
    fs::write(
        shepherd.home().join("shep.toml"),
        "[dog.deploy]\ninterval = \"1s\"\nretention = 5\n",
    )
    .expect("write shep.toml");

    write_run_script(origin.path(), "v2");
    git(origin.path(), &["commit", "-qam", "v2"]);
    let second = head_of(origin.path());

    // Adopted and supervised, with no argv, exactly as the contract says.
    // The name is the binary's own stem with `shep-` stripped, which is what
    // makes the section above this dog's.
    shepherd.ok(&["adopt", DEPLOY_BIN, "--style", "bare"]);
    wait_until("the dog to be supervised", || {
        described_instances(&shepherd, "deploy") == 1
    });
    let dog_log = out_file(&shepherd, "deploy");

    wait_until("the poll loop to deploy v2 on its own", || {
        last_line(&out_log).as_deref() == Some("v2")
    });
    wait_until("the supervised dog to report the deploy as its own", || {
        fs::read_to_string(&dog_log)
            .unwrap_or_default()
            .contains(&format!("web deployed {second}"))
    });
}
