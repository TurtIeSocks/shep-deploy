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
//! `tests/integration.rs` is a separate compilation target and cannot reach
//! this module, so it keeps its own copy. One copy across a target boundary is
//! the cost of the boundary; six inside one target was not.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use tempfile::TempDir;

/// A git budget the test tier can never legitimately hit.
///
/// Every fixture here drives a local repository, so anything slower than a
/// minute is a hang worth failing on rather than waiting out. Distinct from
/// the production default, which is generous on purpose because a cold clone
/// of a real repository legitimately runs minutes.
pub const TEST_BUDGET: Duration = Duration::from_secs(60);

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

/// A throwaway directory standing in for a checked-out release, holding
/// `files` as `(name, contents)`.
///
/// # Panics
/// If the directory or any file cannot be written.
#[track_caller]
pub fn fixture_release(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, contents) in files {
        std::fs::write(dir.path().join(name), contents).expect("write fixture file");
    }
    dir
}

/// A bare throwaway directory, for tests that need a path rather than a
/// populated release.
///
/// # Panics
/// If the directory cannot be created.
#[track_caller]
pub fn tempdir() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}
