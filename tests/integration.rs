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
        Command::new(DEPLOY_BIN)
            .args(["deploy", sheep])
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
fn app_toml(home: &Path, with_cwd: bool) -> String {
    let current = home.join("deploy/web/current");
    let cwd = if with_cwd {
        format!("cwd = {:?}\n", current.to_str().expect("utf-8 path"))
    } else {
        String::new()
    };
    format!(
        "[[app]]\nname = \"web\"\nscript = \"./run.sh\"\n{cwd}\n[app.readiness_probe]\nkind = \
         \"exec\"\ntarget = \"test -f {marker}\"\ninterval = \"1s\"\ntimeout = \
         \"2s\"\nfailure_threshold = 1\n",
        marker = current.join("ready-marker").display(),
    )
}

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
fn origin_with_app(home: &Path, version: &str) -> tempfile::TempDir {
    let origin = tempfile::tempdir().expect("tempdir");
    git(origin.path(), &["init", "-q", "-b", "main"]);
    git(origin.path(), &["config", "user.email", "test@example.com"]);
    git(origin.path(), &["config", "user.name", "test"]);
    fs::write(origin.path().join("Flockfile.toml"), app_toml(home, false))
        .expect("write Flockfile");
    fs::write(origin.path().join("ready-marker"), "").expect("write ready-marker");
    write_run_script(origin.path(), version);
    git(origin.path(), &["add", "."]);
    git(origin.path(), &["commit", "-q", "-m", version]);
    origin
}

/// Overwrites `dir`'s `run.sh` to echo `version` before sleeping.
fn write_run_script(dir: &Path, version: &str) {
    let path = dir.join("run.sh");
    fs::write(&path, format!("#!/bin/sh\necho {version}\nsleep 300\n")).expect("write run.sh");
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
fn register_web(shepherd: &Shepherd) {
    let path = shepherd.home().join("register.toml");
    fs::write(&path, app_toml(shepherd.home(), true)).expect("write register.toml");
    shepherd.ok(&[
        "start",
        path.to_str().expect("utf-8 path"),
        "--style",
        "bare",
    ]);
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

/// fails if a deploy of a fixture repo does not reach the running sheep at
/// all. This is the property no unit test can prove: every module below
/// `main` is exercised against a fake `Daemon`, and a fake can never be
/// wrong about whether `reload` actually replaces the process shep is
/// supervising.
#[test]
fn a_real_deploy_swaps_reloads_and_verifies() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(shepherd.home(), "v1");
    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first, "probed");

    register_web(&shepherd);
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
    let origin = origin_with_app(shepherd.home(), "v1");
    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first, "probed");

    register_web(&shepherd);
    wait_until("the first release to come online", || {
        described_pid(&shepherd, "web").is_some()
    });
    let live_pid = described_pid(&shepherd, "web").expect("a pid");

    fs::write(
        origin.path().join("Flockfile.toml"),
        format!(
            "{}\n[build]\ncommand = 'exit 3'\n",
            app_toml(shepherd.home(), false)
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
/// `verify = "alive"` rather than the default `probed`, for wall clock
/// alone: both modes demand the same generation turnover, and `alive`
/// reaches its verdict after a ten-second window instead of `probed`'s
/// ninety-second one. The `probed` timeout is covered in `src/verify.rs`
/// against a paused clock.
#[test]
fn a_release_that_cannot_come_up_is_rolled_back_and_the_old_release_serves() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(shepherd.home(), "v1");
    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first, "alive");
    register_web(&shepherd);

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

    // `current` points at the old release again...
    let current = fs::read_link(shepherd.home().join("deploy/web/current")).expect("current");
    assert_eq!(
        current,
        shepherd.home().join("deploy/web/releases").join(&first),
        "current must be back on the release that works"
    );

    // ...deploy.toml still names it, never the release that failed...
    let state = fs::read_to_string(shepherd.home().join("deploy/web/deploy.toml")).expect("state");
    assert!(state.contains(&first), "{state}");
    assert!(!state.contains(&second), "{state}");

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
