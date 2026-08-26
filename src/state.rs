//! `deploy.toml`: the only record of how a sheep ran before this dog took it
//! over.
//!
//! It lives in the deploy tree (see [`crate::paths::Tree::state_file`]),
//! keyed to the sheep, not to the dog's own `[dog.<name>]` section in
//! `shep.toml`. Keying it to the dog's name would mean renaming or
//! re-adopting the dog destroys the record of every deployment it manages -
//! unrelated things. Kept here, it survives rehoming and makes the tree
//! self-describing on its own.
//!
//! [`State::origin_cwd`] and [`State::origin_script`] are why removing the
//! dog can put a sheep back where its operator will look for it: they are
//! the `cwd` and script the sheep ran with *before* adoption, captured once
//! at opt-in and never touched again.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// How a freshly deployed release is judged healthy before traffic (or the
/// old release) is torn down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Verify {
    /// Wait for the app's configured readiness probe to answer before
    /// calling the release good. The default: an absent `verify` key must
    /// never silently mean "deploy without checking".
    #[default]
    Probed,
    /// Skip the probe; a release that stays running for the grace period is
    /// good enough.
    Alive,
}

/// Whether this sheep is polled for new commits automatically, or only
/// deployed on request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Watch {
    /// The poll loop deploys new commits on the tracked branch as they
    /// appear. The default: a target written before this field existed, or
    /// by hand without it, keeps being watched rather than silently
    /// stopping.
    #[default]
    Auto,
    /// The poll loop skips this target entirely. It deploys only when asked
    /// (`shep deploy <sheep>`). Everything else - build, atomic swap, probe
    /// verification, auto-rollback - still applies; only the trigger
    /// changes.
    Manual,
}

/// `deploy.toml`'s full contents.
///
/// Every field here is load-bearing for restoring the sheep if the dog is
/// ever removed: losing `origin_cwd` or `origin_script` means removal
/// cannot put the app back where its operator will look for it, and it is
/// left running from a path under `$SHEP_HOME` they have no reason to know
/// about.
///
/// `Debug` is derived deliberately: nothing here is a secret. `remote` is a
/// git URL an operator already has in their own checkout, `origin_script` is
/// a shell command line their own `shep.toml`/process manager already ran in
/// plaintext, and every other field is a path, a sha, or one of the two
/// small enums above. This dog does no credential handling of its own (see
/// `error::Error`'s own doc comment), so nothing here carries one either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// The git remote this sheep is deployed from.
    pub remote: String,
    /// The branch polled for new commits.
    pub branch: String,
    /// The sha currently live under `current`, or `None` before the first
    /// successful deploy.
    pub deployed: Option<String>,
    /// How a freshly deployed release is judged healthy.
    #[serde(default)]
    pub verify: Verify,
    /// Whether this sheep is polled automatically or only deploys on
    /// request.
    #[serde(default)]
    pub watch: Watch,
    /// The working directory the sheep ran from before adoption, for
    /// restore on removal. `None` when the dog itself bootstrapped the
    /// sheep, so there is nothing to restore it to.
    pub origin_cwd: Option<PathBuf>,
    /// The script/command the sheep ran before adoption, for restore on
    /// removal. `None` alongside `origin_cwd` for a dog-bootstrapped sheep.
    pub origin_script: Option<String>,
    /// The user's own git checkout this deploy was set up from. Read-only:
    /// the dog clones from it once at opt-in and never restructures, moves,
    /// or writes to it again.
    pub checkout: PathBuf,
}

impl State {
    /// Serialise to TOML and write to `path` atomically.
    ///
    /// The write lands at `<path>.tmp` in the same directory first, then
    /// `rename(2)`s it over `path`. `rename(2)` within one directory is a
    /// single atomic syscall, so a process killed mid-write leaves either
    /// the old `deploy.toml` intact or the new one in full - never a
    /// truncated file. That matters here specifically because this file is
    /// the only record of how to restore the sheep once the dog is removed.
    ///
    /// # Errors
    /// [`Error::Io`], naming whichever of `path` or `<path>.tmp` the
    /// failing operation touched. This includes the (practically
    /// unreachable, but not impossible) case of a `checkout` or
    /// `origin_cwd` containing non-UTF-8 bytes, which TOML cannot represent
    /// as a string; that failure is reported against `path` before any
    /// write is attempted, so it never touches the temp file at all.
    pub fn write(&self, path: &Path) -> Result<(), Error> {
        let text = toml::to_string_pretty(self).map_err(|source| Error::Io {
            path: path.to_owned(),
            source: io::Error::other(source),
        })?;

        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);

        fs::write(&tmp, text).map_err(|source| Error::Io {
            path: tmp.clone(),
            source,
        })?;
        fs::rename(&tmp, path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if state does not survive a write-then-read. This file is the
    /// only record of how a sheep ran BEFORE the dog took over, so losing a
    /// field means removal cannot restore the sheep and the operator is
    /// left with an app running from a path under $SHEP_HOME they have no
    /// reason to know about.
    ///
    /// `watch` is set to the non-default `Manual` deliberately (the brief's
    /// literal for this test predates the `watch` field and omitted it,
    /// which does not compile against the struct as specified): `verify`
    /// below is already the default value, so setting `watch` to its
    /// *non*-default value is what makes this test additionally prove that
    /// an explicitly-written non-default enum value round-trips, not just
    /// the default one. The two tests below cover the default-substitution
    /// path on their own.
    #[test]
    fn state_round_trips_through_toml() {
        let original = State {
            remote: "https://github.com/WatWowMap/ReactMap".into(),
            branch: "main".into(),
            deployed: Some("a1b2c3d".into()),
            verify: Verify::Probed,
            watch: Watch::Manual,
            origin_cwd: Some(PathBuf::from("/srv/reactmap")),
            origin_script: Some("bun .".into()),
            checkout: PathBuf::from("/srv/reactmap"),
        };
        let text = toml::to_string(&original).expect("serialises");
        let back: State = toml::from_str(&text).expect("parses");
        assert_eq!(back, original);
    }

    /// fails if `watch` stops defaulting to auto. A target written before
    /// this field existed, or by hand without it, must keep being watched
    /// rather than silently stop deploying.
    #[test]
    fn an_absent_watch_defaults_to_auto() {
        let text = r#"
            remote = "https://example.com/x"
            branch = "main"
            checkout = "/srv/x"
        "#;
        let state: State = toml::from_str(text).expect("parses");
        assert_eq!(state.watch, Watch::Auto);
    }

    /// fails if `verify` stops defaulting to the safe value. An absent
    /// verify key must mean "wait for the readiness probe", never "deploy
    /// without checking".
    #[test]
    fn an_absent_verify_defaults_to_probed() {
        let text = r#"
            remote = "https://example.com/x"
            branch = "main"
            checkout = "/srv/x"
        "#;
        let state: State = toml::from_str(text).expect("parses");
        assert_eq!(state.verify, Verify::Probed);
    }

    /// A minimal, valid `State` for the write tests below, where the exact
    /// values don't matter - only that they change between the two writes.
    fn sample(deployed: Option<&str>) -> State {
        State {
            remote: "https://example.com/x".into(),
            branch: "main".into(),
            deployed: deployed.map(str::to_owned),
            verify: Verify::default(),
            watch: Watch::default(),
            origin_cwd: None,
            origin_script: None,
            checkout: PathBuf::from("/srv/x"),
        }
    }

    /// Pins that `write` works and that a completed write leaves no `.tmp`
    /// file behind - it does NOT, and cannot, prove atomicity. A
    /// single-threaded test has no way to observe a process crashing
    /// mid-write, so the atomic-rename claim rests on the implementation
    /// shape (write to a sibling temp file, then one `rename(2)` within the
    /// same directory) rather than on anything this test checks. Two writes
    /// are run specifically to prove the *second* write's tmp file is also
    /// cleaned up, not just a first write starting from nothing.
    #[test]
    fn write_survives_a_second_write_and_leaves_no_tmp_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deploy.toml");
        let tmp = dir.path().join("deploy.toml.tmp");

        let first = sample(None);
        first.write(&path).expect("first write");
        assert!(path.exists(), "the state file must exist after a write");
        assert!(
            !tmp.exists(),
            "a completed write must not leave a .tmp file"
        );

        let second = sample(Some("a1b2c3d"));
        second.write(&path).expect("second write");
        assert!(
            !tmp.exists(),
            "a second, overwriting write must also leave no .tmp file"
        );

        let read_back: State =
            toml::from_str(&fs::read_to_string(&path).expect("read")).expect("parses");
        assert_eq!(read_back, second);
    }
}
