//! Test helpers shared by every module's own `mod tests`.
//!
//! Compiled only under `cfg(test)` and declared from `main.rs`, which is the
//! only way a binary crate with no library target can share one of these.
//!
//! # Why this exists
//!
//! Before it, `run_git` was copy-pasted verbatim into six modules, `head_of`
//! into three, `fixture_release` into two, and `TEST_BUDGET` into four. That
//! is not a tidiness complaint: the copies had already drifted. The one in
//! `tests/integration.rs` prints the directory when git fails and the six in
//! `src/` do not, so an improvement landed once and six fixtures kept the
//! worse message. `build.rs`'s copy of `fixture_release` even carried a doc
//! comment naming the duplication and leaving it there.
//!
//! Four more kinds went the same way afterwards, all found in one review pass.
//! A `DogConfig` literal was written out eight times, differing in nothing but
//! `interval`. A `State` literal was written out eleven, and its shas were
//! whatever length the module happened to type, which stopped being free the
//! day [`State::read`] started refusing anything but a full forty. The "git
//! init, set an identity, write a file, commit" opening was written seven
//! times, and one of the seven had already been fixed on its own after
//! failing on CI for want of a `user.email`, with the other six left alone.
//! And fourteen [`Daemon`](crate::daemon::Daemon) doubles hand-wrote 78
//! `unimplemented!()` methods between them, which buried the one or two
//! methods each double really answers.
//!
//! `tests/integration.rs` is a separate compilation target and cannot reach
//! this module, so it keeps its own copy. One copy across a target boundary is
//! the cost of the boundary; six inside one target was not.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tempfile::TempDir;

use crate::config::DogConfig;
use crate::state::{State, Verify, Watch};

/// A full commit sha, as `git rev-parse` prints one.
///
/// Spelled out rather than shortened because [`State::read`] refuses a
/// `deployed` or `failed` that is not forty hex characters, so a fixture
/// carrying `"old"` parses in one test and is rejected in the next.
pub const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// A second full sha, for a test that needs two that differ.
pub const OTHER_SHA: &str = "a1b2c3a1b2c3a1b2c3a1b2c3a1b2c3a1b2c3a1b2";

/// A git budget the test tier can never legitimately hit.
///
/// Every fixture here drives a local repository, so anything slower than a
/// minute is a hang worth failing on rather than waiting out. Distinct from
/// the production default, which is generous on purpose because a cold clone
/// of a real repository legitimately runs minutes.
pub const TEST_BUDGET: Duration = Duration::from_secs(60);

/// The dog config every test that is not about a config value runs on.
///
/// The timeouts are a minute rather than the production five minutes and hour,
/// for the same reason as [`TEST_BUDGET`]: nothing here talks to a network or
/// runs a real build, so anything slower is a hang.
///
/// A test that cares about one field says so by name and takes the rest from
/// here:
///
/// ```ignore
/// DogConfig { interval: Duration::from_secs(150), ..fixtures::dog_config() }
/// ```
pub fn dog_config() -> DogConfig {
    DogConfig {
        interval: Duration::from_secs(30),
        retention: 5,
        git_timeout: Duration::from_secs(60),
        build_timeout: Duration::from_secs(60),
        passthrough: Vec::new(),
    }
}

/// A target's record with every field set to something [`State::read`] will
/// accept: a remote, `main`, an absolute checkout, nothing deployed, nothing
/// failed, and no origin.
///
/// A test names only the fields it is about and takes the rest from here:
///
/// ```ignore
/// State { deployed: Some(fixtures::SHA.to_owned()), watch, ..fixtures::state() }
/// ```
///
/// `origin_cwd` and `origin_script` are both `None` because `read` refuses a
/// record with one and not the other, so a fixture that set one alone would be
/// a record no opt-in ever writes.
pub fn state() -> State {
    State {
        remote: "https://example.com/x".to_owned(),
        branch: "main".to_owned(),
        deployed: None,
        failed: None,
        verify: Verify::default(),
        watch: Watch::default(),
        origin_cwd: None,
        origin_script: None,
        checkout: PathBuf::from("/srv/x"),
        origin: None,
    }
}

/// Runs `git <args>` in `dir`, panicking with the command AND the directory if
/// it fails.
///
/// The directory is in the message deliberately. Every fixture here works in a
/// tempdir with a generated name, so a bare "git \[...\] failed" leaves you
/// guessing which of several repositories a test built was the broken one.
///
/// # Panics
/// If git cannot be spawned, or exits non-zero.
#[track_caller]
pub fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// The sha `HEAD` resolves to in `dir`.
///
/// # Panics
/// If git cannot be spawned, or prints something that is not UTF-8.
#[track_caller]
pub fn head_of(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    // Checked, because the failure is silent otherwise: a repository with no
    // commits gives an empty stdout and a status nobody reads, so the caller
    // gets `""` and asserts against it. That exact shape wasted two attempts
    // on a fixture earlier in this review.
    assert!(
        out.status.success(),
        "git rev-parse HEAD failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8(out.stdout)
        .expect("utf-8 sha")
        .trim()
        .to_owned()
}

/// Writes `files` as `(path, contents)` under `dir`, creating any directory a
/// path names on the way.
///
/// # Panics
/// If a directory or any file cannot be written.
#[track_caller]
pub fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (name, contents) in files {
        let full = dir.join(name);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create fixture parent");
        }
        std::fs::write(&full, contents).expect("write fixture file");
    }
}

/// A throwaway directory standing in for a checked-out release, holding
/// `files` as `(name, contents)`.
///
/// # Panics
/// If the directory or any file cannot be written.
#[track_caller]
pub fn fixture_release(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    write_files(dir.path(), files);
    dir
}

/// A throwaway git repository on `main` with a commit identity set, and
/// nothing committed yet.
///
/// The branch is named explicitly because `init.defaultBranch` is a user
/// setting no test can assume, and the identity is set because a commit
/// without one fails. A developer's machine has a global `user.email` and a
/// CI runner does not, so leaving the identity out makes a fixture pass
/// locally and fail only on CI, which is where this repository's very first
/// push failed.
///
/// # Panics
/// If the directory cannot be created, or git fails.
#[track_caller]
pub fn empty_checkout() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    run_git(dir.path(), &["init", "-q", "-b", "main"]);
    run_git(dir.path(), &["config", "user.email", "test@example.com"]);
    run_git(dir.path(), &["config", "user.name", "test"]);
    dir
}

/// Writes `files` into the repository at `dir` and commits everything staged
/// by `git add .` as one commit named `message`.
///
/// `git add .` rather than adding the paths by name, so an entry matched by a
/// `.gitignore` stays untracked instead of being forced into the commit.
/// Several fixtures below want exactly that split: something tracked,
/// something ignored and present.
///
/// # Panics
/// If a file cannot be written, or git fails.
#[track_caller]
pub fn commit(dir: &Path, files: &[(&str, &str)], message: &str) {
    write_files(dir, files);
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-q", "-m", message]);
}

/// An [`empty_checkout`] with `files` written and committed as its first and
/// only commit.
///
/// # Panics
/// If the directory cannot be created, a file cannot be written, or git
/// fails. Committing nothing fails, so `files` must name at least one file
/// that is not ignored.
#[track_caller]
pub fn checkout(files: &[(&str, &str)]) -> TempDir {
    let dir = empty_checkout();
    commit(dir.path(), files, "first");
    dir
}

/// Fills in the [`Daemon`](crate::daemon::Daemon) methods a test double never
/// has called on it, each panicking with its own name.
///
/// Every double in this crate implements one or two methods and stands the
/// other seven or eight up as `unimplemented!()`, which was 78 lines of
/// boilerplate across fourteen doubles and, worse, hid which methods a
/// double actually answers behind a wall of identical ones. A double that
/// answers every method the same way, a daemon that cannot be reached, say,
/// names the answer once with the `answering` form.
///
/// ```ignore
/// impl Daemon for Recording {
///     async fn set_smit(&self, sheep: &str, text: &str) -> Result<(), Error> { .. }
///
///     fixtures::daemon_methods!(unimplemented;
///         dog_config, list_flock, describe, start, delete, reload, restart, save_roll,
///     );
/// }
/// ```
///
/// The signatures are spelled out arm by arm rather than generated, because a
/// macro cannot see a trait's own declarations. A method added to `Daemon`
/// therefore needs an arm here too, and a call site naming a method with no
/// arm fails to compile rather than silently expanding to nothing.
macro_rules! daemon_methods {
    (@method dog_config, $body:expr) => {
        async fn dog_config(&self, _name: &str) -> Result<String, crate::error::Error> {
            $body
        }
    };
    (@method list_flock, $body:expr) => {
        async fn list_flock(
            &self,
        ) -> Result<Vec<shep_client::shep_core::protocol::ProcessInfo>, crate::error::Error>
        {
            $body
        }
    };
    (@method describe, $body:expr) => {
        async fn describe(
            &self,
            _sheep: &str,
        ) -> Result<Vec<shep_client::shep_core::protocol::ProcessInfo>, crate::error::Error>
        {
            $body
        }
    };
    (@method start, $body:expr) => {
        async fn start(
            &self,
            _apps: Vec<shep_client::shep_core::config::AppConfig>,
        ) -> Result<Vec<u32>, crate::error::Error> {
            $body
        }
    };
    (@method delete, $body:expr) => {
        async fn delete(&self, _id: u32) -> Result<(), crate::error::Error> {
            $body
        }
    };
    (@method reload, $body:expr) => {
        async fn reload(&self, _sheep: &str) -> Result<(), crate::error::Error> {
            $body
        }
    };
    (@method restart, $body:expr) => {
        async fn restart(&self, _sheep: &str) -> Result<(), crate::error::Error> {
            $body
        }
    };
    (@method save_roll, $body:expr) => {
        async fn save_roll(&self) -> Result<std::path::PathBuf, crate::error::Error> {
            $body
        }
    };
    (@method set_smit, $body:expr) => {
        async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), crate::error::Error> {
            $body
        }
    };
    // The methods a double never expects to be asked.
    (unimplemented; $($method:ident),* $(,)?) => {
        $(crate::fixtures::daemon_methods!(@method $method, unimplemented!(stringify!($method)));)*
    };
    // The methods a double answers the same way every time, `body` being
    // that answer: a daemon that cannot be reached, say.
    (answering $body:expr; $($method:ident),* $(,)?) => {
        $(crate::fixtures::daemon_methods!(@method $method, $body);)*
    };
}

pub(crate) use daemon_methods;

/// A bare throwaway directory, for tests that need a path rather than a
/// populated release.
///
/// # Panics
/// If the directory cannot be created.
#[track_caller]
pub fn tempdir() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}
