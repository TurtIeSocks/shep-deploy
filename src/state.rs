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
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use shep_client::shep_core::config::AppConfig;

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

/// The record's file mode: owner only, because [`State::origin`] carries
/// the app's `env` verbatim.
const RECORD_MODE: u32 = 0o600;

/// `deploy.toml`'s full contents.
///
/// Every field here is load-bearing for restoring the sheep if the dog is
/// ever removed: losing `origin_cwd` or `origin_script` means removal
/// cannot put the app back where its operator will look for it, and it is
/// left running from a path under `$SHEP_HOME` they have no reason to know
/// about.
///
/// `Debug` is derived, and since 2026-09-04 that rests on one thing:
/// [`Self::origin`] carries the app's `env` verbatim, and `AppConfig`'s own
/// hand-written `Debug` prints that as `<N vars>` (shep-core, IR-41). Every
/// other field is a git URL an operator already has in their checkout, a
/// path, a sha, a shell command line their own `shep.toml` already held in
/// plaintext, or one of the two small enums above. A field added here that
/// could carry a secret needs its own redaction; the derive is not a
/// blanket exemption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// The git remote this sheep is deployed from.
    pub remote: String,
    /// The branch polled for new commits.
    pub branch: String,
    /// The sha currently live under `current`, or `None` before the first
    /// successful deploy.
    pub deployed: Option<String>,
    /// The sha an attempt last failed on, or `None` when the last attempt
    /// landed.
    ///
    /// What stops the poll loop rebuilding one bad commit forever.
    /// `deployed` advances only after a verify, so without this the loop
    /// finds the branch head different from what is serving on every
    /// single tick and runs the whole sequence again: fetch, full rebuild,
    /// swap, reload, wait out the verification budget, roll back, reload
    /// again. Two reloads of a live app and a build every thirty seconds,
    /// indefinitely, from one bad commit.
    ///
    /// The branch moving is what clears it, which is what CI does with a
    /// red commit. That is the wrong answer for a deploy that failed on a
    /// network blip rather than on the commit, and it was chosen with that
    /// in mind: it costs a push, or one `shep deploy <sheep>`, which
    /// retries a held sha deliberately.
    #[serde(default)]
    pub failed: Option<String>,
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
    /// The whole app definition as the shepherd had it BEFORE adoption, so
    /// removal can put back everything and not only `cwd` and `script`.
    ///
    /// `origin_cwd` and `origin_script` predate this field and are kept for
    /// the records that carry only them; a record with both is restored
    /// from this one. It is what makes `deploy.toml` hold the app's `env`
    /// verbatim, which is why the file is written owner-only, the same
    /// mode shep's own roll uses for the same reason.
    ///
    /// It serialises as `[origin]` with `[origin.env]` and the probe tables
    /// under it, at the END of the file whatever this field's position:
    /// this crate's `toml` hoists every scalar above every table. So the
    /// record an operator opens still starts with the lines they edit.
    #[serde(default)]
    pub origin: Option<AppConfig>,
}

impl State {
    /// Read and parse the `deploy.toml` at `path`, refusing a record whose
    /// values cannot work.
    ///
    /// # Errors
    /// [`Error::Io`], naming `path`, if the file cannot be read - most
    /// often because this sheep is not a deploy target at all.
    /// [`Error::Config`] if it is not valid TOML, is missing a field that
    /// has no default, or fails [`Self::validate`].
    pub fn read(path: &Path) -> Result<Self, Error> {
        let text = fs::read_to_string(path).map_err(Error::at(path))?;
        let state: Self = toml::from_str(&text)
            .map_err(|source| Error::Config(format!("{}: {source}", path.display())))?;
        state.validate(path)?;
        Ok(state)
    }

    /// Refuses a record that parses and still cannot be acted on, naming the
    /// field and what is wrong with it.
    ///
    /// This file is hand-edited, and every field here is handed to git or
    /// joined onto a path later, where a bad value fails in words that name
    /// the mechanism rather than the mistake. An empty `branch` reaches
    /// `git rev-parse --verify refs/heads/` and fails as "needed a single
    /// revision". A `deployed` of `""` makes `Tree::release("")` the
    /// `releases/` directory itself, which exists, so a rollback would point
    /// `current` at it. A `remote` beginning with `-` is an option to git
    /// rather than a URL, and `--upload-pack=<command>` is one it runs.
    /// Measured 2026-09-03, all three.
    ///
    /// # Errors
    /// [`Error::Config`], naming `path` and the field, for: an empty
    /// `remote`, `branch` or `checkout`; a `remote` or `branch` beginning
    /// with `-` or carrying a character that would rewrite a log line; a
    /// relative `checkout`; a
    /// `deployed` or `failed` that is not a full commit sha; `deployed` and
    /// `failed` naming the same sha; or exactly one of `origin_cwd` and
    /// `origin_script` set.
    pub(crate) fn validate(&self, path: &Path) -> Result<(), Error> {
        let refuse =
            |field: &str, why: &str| Error::Config(format!("{}: `{field}` {why}", path.display()));

        for (field, value) in [("remote", &self.remote), ("branch", &self.branch)] {
            if value.trim().is_empty() {
                return Err(refuse(field, "is empty"));
            }
            if value.starts_with('-') {
                return Err(refuse(
                    field,
                    "begins with `-`, which git would read as an option rather than a name",
                ));
            }
            if value.chars().any(crate::shared::forges_a_line) {
                return Err(refuse(
                    field,
                    "carries a control or invisible character that would rewrite a log line",
                ));
            }
        }

        if self.checkout.as_os_str().is_empty() {
            return Err(refuse("checkout", "is empty"));
        }
        if !self.checkout.is_absolute() {
            return Err(refuse(
                "checkout",
                "is not an absolute path, and a relative one would be resolved against \
                 wherever the dog happens to be running",
            ));
        }

        for (field, value) in [("deployed", &self.deployed), ("failed", &self.failed)] {
            if let Some(sha) = value
                && !is_sha(sha)
            {
                return Err(refuse(
                    field,
                    "is not a full commit sha: 40 (or 64) hexadecimal characters, as `git \
                     rev-parse` prints one",
                ));
            }
        }
        if self.deployed.is_some() && self.deployed == self.failed {
            return Err(refuse(
                "failed",
                "names the sha `deployed` names. A release that is serving cannot also be the \
                 one being held; remove `failed`",
            ));
        }

        if self.origin_cwd.is_some() != self.origin_script.is_some() {
            let missing = if self.origin_cwd.is_none() {
                "origin_cwd"
            } else {
                "origin_script"
            };
            return Err(refuse(
                missing,
                "is missing while its partner is set. Both are written together at setup and \
                 both are needed to put the sheep back on removal; set both or neither",
            ));
        }

        Ok(())
    }

    /// Serialise to TOML and write to `path` atomically and durably.
    ///
    /// The write lands at `<path>.<pid>.tmp` in the same directory first, is
    /// flushed to disk, then `rename(2)`d over `path`, and the directory is
    /// flushed after that. `rename(2)` within one directory is a single
    /// atomic syscall, so a process killed mid-write leaves either the old
    /// `deploy.toml` intact or the new one in full - never a truncated file.
    /// The two flushes extend that from a killed process to a lost power
    /// supply: without them the rename can reach the disk ahead of the bytes
    /// it names, which leaves a zero-length record. That matters here
    /// specifically because this file is the only record of how to restore
    /// the sheep once the dog is removed.
    ///
    /// A failed rename removes the temporary file it leaves behind, so one
    /// failure does not sit next to the record forever, unread by anything.
    ///
    /// The file is created owner-only. Since 2026-09-04 it can carry the
    /// app's `env` verbatim in [`Self::origin`], and shep writes its own
    /// roll at the same mode for the same reason. A record left at a wider
    /// mode by an earlier version is tightened on the next write, because
    /// the rename replaces it with the fresh file.
    ///
    /// # Errors
    /// [`Error::Config`] if the record fails [`Self::validate`]: nothing is
    /// written. [`Error::Io`], naming whichever of `path` or `<path>.<pid>.tmp` the
    /// failing operation touched. This includes the (practically
    /// unreachable, but not impossible) case of a `checkout` or
    /// `origin_cwd` containing non-UTF-8 bytes, which TOML cannot represent
    /// as a string; that failure is reported against `path` before any
    /// write is attempted, so it never touches the temp file at all.
    pub fn write(&self, path: &Path) -> Result<(), Error> {
        // Validated on the way out as well as on the way in, because every
        // writer is inside this crate: a record this crate persists and then
        // refuses to read on every later command is a target nothing can
        // touch, and the mistake is cheaper at the moment it is made.
        self.validate(path)?;
        let text = toml::to_string_pretty(self).map_err(|source| Error::Io {
            path: path.to_owned(),
            source: io::Error::other(source),
        })?;

        // The pid in the name, because `set_watch` writes without the tree
        // lock (see its doc for why) and two processes staging into one
        // `<path>.tmp` would truncate each other's file: the second rename
        // then fails, and a rename landing between the other's truncate and
        // write exposes an empty record.
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".{}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let at_tmp = |source| Error::Io {
            path: tmp.clone(),
            source,
        };

        let written = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(RECORD_MODE)
            .open(&tmp)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .map_err(at_tmp);
        if let Err(err) = written {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }

        if let Err(source) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }

        // The directory entry, so the rename itself survives a power loss.
        // Best effort: a filesystem that refuses to sync a directory has
        // already accepted the data, and failing the deploy over the entry
        // would be the worse outcome.
        if let Some(dir) = path.parent() {
            let _ = fs::File::open(dir).and_then(|dir| dir.sync_all());
        }
        Ok(())
    }
}

/// Whether `text` is a full git object id: 40 hexadecimal characters for
/// SHA-1, 64 for SHA-256.
///
/// The one rule for what a release is named by, shared with
/// `crate::retention` so the two cannot disagree about which directories
/// under `releases/` are releases.
#[must_use]
pub fn is_sha(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, SHA};

    /// fails if a typo in `deploy.toml` is silently dropped.
    ///
    /// This file is not ours alone: `crate::deploy` tells an operator to type
    /// `verify = "alive"` into it by hand. `verify` is `#[serde(default)]`, so
    /// without this a `verfiy = "alive"` parses fine, leaves the mode at
    /// `Probed`, and the next deploy fails on the identical message with
    /// nothing anywhere indicating the edit did nothing.
    ///
    /// Matches what `crate::config` already does to a `[deploy]` typo, and
    /// for the same stated reason: a setting that silently does something
    /// other than what it says is worse than one that is refused.
    #[test]
    fn an_unknown_key_in_the_record_is_refused_and_named() {
        // `checkout` is present, and load-bearing: it has no default, so
        // leaving it out made the document invalid twice over and the test
        // passed on whichever error serde reported first. It still caught the
        // guard being removed, but failed with "TOML parse error at line 1"
        // instead of naming the typo, which sends the next reader to the wrong
        // place.
        let toml = r#"
remote = "https://example.invalid/repo.git"
branch = "main"
checkout = "/srv/web"
verfiy = "alive"
"#;
        let err = toml::from_str::<State>(toml).expect_err("a typo must not parse");
        assert!(
            format!("{err}").contains("verfiy"),
            "the refusal must name the key an operator typed: {err}"
        );
    }

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
            deployed: Some("a1b2c3a1b2c3a1b2c3a1b2c3a1b2c3a1b2c3a1b2".into()),
            failed: None,
            verify: Verify::Probed,
            watch: Watch::Manual,
            origin_cwd: Some(PathBuf::from("/srv/reactmap")),
            origin_script: Some("bun .".into()),
            checkout: PathBuf::from("/srv/reactmap"),
            origin: None,
        };
        let text = toml::to_string(&original).expect("serialises");
        let back: State = toml::from_str(&text).expect("parses");
        assert_eq!(back, original);
    }

    /// Whether any `*.tmp` sits in `dir`: the staging file a write leaves
    /// behind when it does not finish.
    fn any_tmp(dir: &Path) -> bool {
        fs::read_dir(dir)
            .expect("list")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
    }

    /// Writes `text` as a record and reads it back through `State::read`,
    /// so the validation runs the way it does in production.
    fn read_text(text: &str) -> Result<State, Error> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deploy.toml");
        fs::write(&path, text).expect("write record");
        State::read(&path)
    }

    /// fails if a record that parses is acted on when its values cannot
    /// work. Each of these used to reach git, or a path join, and fail
    /// there in words about the mechanism: an empty `branch` as "needed a
    /// single revision", `deployed = ""` as a rollback onto `releases/`
    /// itself, a `remote` beginning with `-` as whatever git made of the
    /// option. The refusal has to name the field an operator typed.
    #[test]
    fn a_record_whose_values_cannot_work_is_refused_by_field() {
        let base = |extra: &str| {
            format!(
                "remote = \"https://example.com/x\"\nbranch = \"main\"\ncheckout = \"/srv/x\"\n{extra}"
            )
        };
        let cases = [
            (
                "remote",
                "remote = \"\"\nbranch = \"main\"\ncheckout = \"/srv/x\"".to_owned(),
            ),
            (
                "remote",
                "remote = \"--upload-pack=x\"\nbranch = \"main\"\ncheckout = \"/srv/x\"".to_owned(),
            ),
            (
                "branch",
                "remote = \"https://example.com/x\"\nbranch = \"\"\ncheckout = \"/srv/x\""
                    .to_owned(),
            ),
            (
                "branch",
                "remote = \"https://example.com/x\"\nbranch = \"-x\"\ncheckout = \"/srv/x\""
                    .to_owned(),
            ),
            (
                "branch",
                "remote = \"https://example.com/x\"\nbranch = \"ma\\nin\"\ncheckout = \"/srv/x\""
                    .to_owned(),
            ),
            (
                "checkout",
                "remote = \"https://example.com/x\"\nbranch = \"main\"\ncheckout = \"\"".to_owned(),
            ),
            (
                "checkout",
                "remote = \"https://example.com/x\"\nbranch = \"main\"\ncheckout = \"srv/x\""
                    .to_owned(),
            ),
            ("deployed", base("deployed = \"\"")),
            ("deployed", base("deployed = \"main\"")),
            ("deployed", base("deployed = \"abc123\"")),
            ("failed", base("failed = \"..\"")),
            (
                "failed",
                base(&format!("deployed = \"{SHA}\"\nfailed = \"{SHA}\"")),
            ),
            ("origin_script", base("origin_cwd = \"/srv/x\"")),
            ("origin_cwd", base("origin_script = \"bun .\"")),
        ];
        for (field, text) in cases {
            let err = read_text(&text).expect_err(&format!("must refuse: {text}"));
            assert!(matches!(err, Error::Config(_)), "{text}: {err}");
            assert!(
                err.to_string().contains(&format!("`{field}`")),
                "must name `{field}`: {err}"
            );
        }
    }

    /// fails if a record with every value in shape is refused, or if a
    /// zero-length one - what a crash mid-write used to be able to leave -
    /// is read as anything but a named failure.
    #[test]
    fn a_sound_record_reads_and_an_empty_one_is_refused() {
        let text = format!(
            "remote = \"https://example.com/x\"\nbranch = \"main\"\ncheckout = \"/srv/x\"\n\
             deployed = \"{SHA}\"\norigin_cwd = \"/srv/x\"\norigin_script = \"bun .\"\n"
        );
        let state = read_text(&text).expect("a sound record");
        assert_eq!(state.deployed.as_deref(), Some(SHA));

        let err = read_text("").expect_err("nothing to read");
        assert!(matches!(err, Error::Config(_)), "{err}");
        assert!(err.to_string().contains("deploy.toml"), "{err}");
    }

    /// fails if the pre-adoption app definition does not survive a
    /// write-then-read, `env` and probe included, or if a record written
    /// before the field existed stops reading. The first is what removal
    /// restores from; the second is every record on disk today.
    #[test]
    fn the_origin_round_trips_and_an_older_record_still_reads() {
        let origin: AppConfig = toml::from_str(
            "name = \"web\"\nscript = \"server.js\"\ninstances = 2\n[env]\nTOKEN = \"s3cret\"\n\
             [readiness_probe]\nkind = \"http\"\ntarget = \"http://127.0.0.1:3000/health\"\n",
        )
        .expect("an app");
        let mut with = sample(None);
        with.origin = Some(origin.clone());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deploy.toml");
        with.write(&path).expect("writes");
        let back = State::read(&path).expect("reads");
        assert_eq!(back.origin.as_ref(), Some(&origin));

        let older = read_text(
            "remote = \"https://example.com/x\"\nbranch = \"main\"\ncheckout = \"/srv/x\"\n",
        )
        .expect("a record from before the field");
        assert_eq!(older.origin, None);
    }

    /// fails if the record is readable by anyone but its owner. It carries
    /// the app's `env` verbatim once an origin is recorded, and shep's own
    /// roll is owner-only for the same reason.
    #[test]
    fn the_record_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deploy.toml");
        sample(None).write(&path).expect("writes");
        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, RECORD_MODE);
    }

    /// fails if a rename that cannot land leaves its temporary file behind.
    /// Nothing reads `deploy.toml.tmp`, so a stale one would sit beside the
    /// record forever, a full copy of it that no listing ever mentions.
    #[test]
    fn a_failed_write_leaves_no_tmp_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory where the record should go, so the rename fails.
        let path = dir.path().join("deploy.toml");
        fs::create_dir_all(&path).expect("something in the way");

        sample(None)
            .write(&path)
            .expect_err("cannot rename onto a directory");

        assert!(
            !any_tmp(dir.path()),
            "the temporary file must be removed with the failure"
        );
    }

    /// fails if reading a `deploy.toml` that is not there produces
    /// anything other than a named I/O failure. An operator asking to
    /// deploy a sheep that was never set up as a target meets this path,
    /// and the path it names is the only clue about what is missing.
    #[test]
    fn reading_a_missing_state_file_names_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deploy.toml");
        let err = State::read(&path).expect_err("nothing to read");
        assert!(err.to_string().contains("deploy.toml"));
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
            deployed: deployed.map(str::to_owned),
            ..fixtures::state()
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
        let tmp = dir.path();

        let first = sample(None);
        first.write(&path).expect("first write");
        assert!(path.exists(), "the state file must exist after a write");
        assert!(
            !any_tmp(tmp),
            "a completed write must not leave a .tmp file"
        );

        let second = sample(Some(fixtures::OTHER_SHA));
        second.write(&path).expect("second write");
        assert!(
            !any_tmp(tmp),
            "a second, overwriting write must also leave no .tmp file"
        );

        assert_eq!(State::read(&path).expect("reads"), second);
    }
}
