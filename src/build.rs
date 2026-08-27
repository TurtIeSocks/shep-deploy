//! Running the release's own build, as the target sheep's user and primary
//! group, and salvaging whatever it produced outside the release.
//!
//! **This is the dangerous step in the whole design.** Every other module
//! here reads a repository or shells out to `git`; this one executes
//! arbitrary code from it. `bun install`'s postinstall scripts are the
//! single most common supply-chain vector in the Node ecosystem, and
//! `make build` is arbitrary by construction - there is no way to make
//! running someone else's build script safe, only ways to bound the damage
//! when it turns out to be malicious. [`run`] is that bound, and how much of
//! a bound it is depends entirely on one field.
//!
//! ## The privilege drop happens only when the app sets `user`
//!
//! **With `user` set**, the build runs as that user's uid, primary gid and
//! nothing else: none of the shepherd's own privilege survives into it. Uid
//! alone would not be enough - the design spec recommends running the
//! shepherd as root specifically so it can drop privileges, and a child that
//! only dropped uid would keep root's own gid for the whole build - see
//! [`run`]'s own doc for the detail.
//!
//! **With `user` unset, which is the default, there is no drop at all.** The
//! build runs as whatever the shepherd runs as. Where the shepherd is root -
//! the arrangement its own docs recommend, precisely so that it CAN drop
//! privileges - a repository's `bun install` postinstall or `make build`
//! executes as root, with the shepherd's full privilege, on every deploy.
//! Nothing here refuses that deploy: this crate's job is to run the build the
//! Flockfile describes, and an app with no `user` is a legitimate
//! configuration that shep itself accepts.
//!
//! That is a deliberate decision by this project's owner rather than an
//! oversight, and it is stated here in full so an operator can act on it:
//! **setting `user` on the app is what turns this module's bound on.**
//! Without it the deploy still works and the failure modes above still hold
//! - a failing build still never reaches the swap - but "a compromised build
//! gets the app's privileges and nothing more" is not one of the guarantees
//! in force.
//!
//! Three behaviours worth knowing before reading [`run`]'s body:
//!
//! - **An absent build command is a no-op, not an error.** `ReactMap` run
//!   as `bun .` compiles its client with vite's own API at startup and
//!   declares no build at all; the readiness probe already covers that
//!   case, so this module does not need to know a compile happened.
//! - **A failing build is an error that stops everything.** `current`
//!   never moves and the running app is untouched, because it lives in a
//!   directory the build never touches. This is the part that replaces a
//!   hardcoded sleep between a build and a restart.
//! - **`build.artifacts` exists because of `build.env`.** A shared
//!   `CARGO_TARGET_DIR` keeps Rust compilation warm across releases, which
//!   matters because a from-scratch Koji build per deploy is not
//!   acceptable. It does this by moving cargo's entire output tree outside
//!   the release, so a declared artifact has to be copied back in or
//!   `script = ./target/release/koji` resolves to nothing and rollback has
//!   no binary to roll back to.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio::process::Command;

use crate::error::Error;

/// What to run to turn a freshly checked-out release into something its
/// declared script can execute, and what to salvage from wherever that run
/// actually left its output.
///
/// This is the `[build]` block of a release's own Flockfile, parsed by
/// [`crate::flockfile::build_spec`]:
///
/// ```toml
/// [build]
/// command = "bun install && bun run build"
/// env = { CARGO_TARGET_DIR = "/srv/cache/koji" }
/// artifacts = ["target/release/koji"]
/// ```
///
/// Every field is optional and an absent `[build]` block is the default
/// value of this struct, which [`run`] treats as a no-op. Unknown keys are
/// refused rather than ignored, matching `shep_core`'s own `AppConfig`: a
/// typo in a build block would otherwise mean a build that silently never
/// runs.
#[derive(Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuildSpec {
    /// The build command, run through `sh -c` inside the release. `None`
    /// is a no-op, not an error - see [`run`] for why.
    pub command: Option<String>,
    /// Environment layered over the build's own on top of whatever it
    /// inherits from the process running this dog. `CARGO_TARGET_DIR` is
    /// the one entry this crate's design gives special meaning to - see
    /// [`artifact_source`].
    pub env: BTreeMap<String, String>,
    /// Paths, relative to the release, to copy back in after a successful
    /// build. See the module doc for why this exists at all.
    pub artifacts: Vec<PathBuf>,
}

/// `Debug` does not print `env`'s values (IR-41).
///
/// `env` is exactly the kind of thing that carries secrets - a registry
/// token for a private package feed is an entirely plausible thing to find
/// in a real `build.env` - so this prints how many variables there are and
/// nothing about what they hold. Mirrors `shep_core`'s own `AppConfig`,
/// which redacts its `env` field the same way for the same reason, rather
/// than inventing a second format for the same problem.
impl fmt::Debug for BuildSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildSpec")
            .field("command", &self.command)
            .field("env", &format_args!("<{} vars>", self.env.len()))
            .field("artifacts", &self.artifacts)
            .finish()
    }
}

/// Resolves `user` to an id by shelling out to `id <flag> user`, for
/// whichever of `-u` (uid) or `-g` (primary gid) `flag` names. `kind` is
/// only for the error message ("uid" or "gid").
///
/// The crate forbids unsafe code outright, which rules out `getpwnam`
/// directly; asking `id` instead needs none, and it is the same "ask the
/// host rather than reimplement its answer" idiom [`crate::git`] already
/// uses for git itself. It also answers correctly regardless of *how* this
/// host resolves users - `/etc/passwd`, LDAP, anything else NSS is
/// configured for - where a hand-rolled `/etc/passwd` parser would only
/// ever cover the local case.
///
/// Shared by [`uid_for`] and [`gid_for`] rather than duplicated: both need
/// the exact same shape of subprocess call and the exact same two failure
/// modes, and the only thing that differs between them is which flag to
/// pass `id` and which word to use in the message.
///
/// # Errors
/// [`Error::Io`], naming `release`, if `id` itself cannot be launched.
/// [`Error::Config`] if `id <flag>` exits non-zero - most likely because
/// `user` does not exist on this host - or prints something that does not
/// parse as an id.
async fn resolve_id(release: &Path, user: &str, flag: &str, kind: &str) -> Result<u32, Error> {
    let output = Command::new("id")
        .arg(flag)
        .arg(user)
        .output()
        .await
        .map_err(|source| Error::Io {
            path: release.to_owned(),
            source,
        })?;

    if !output.status.success() {
        return Err(Error::Config(format!(
            "as_user {user:?} does not resolve to a {kind} on this host (`id {flag} {user}` \
             failed)"
        )));
    }

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| Error::Config(format!("`id {flag} {user}` did not print a {kind}")))
}

/// Resolves `user` to a uid. See [`resolve_id`].
async fn uid_for(release: &Path, user: &str) -> Result<u32, Error> {
    resolve_id(release, user, "-u", "uid").await
}

/// Resolves `user` to their *primary* gid. See [`resolve_id`].
///
/// Primary only, deliberately: [`run`]'s `as_user` is a single string, not
/// a user/group pair, so the primary gid `id -g` reports is the only group
/// identity there is anything to derive from here without changing that
/// interface. `shep_core::AppConfig` does carry its own separate `group`
/// field, so an explicit group is available upstream if a later task wants
/// to thread one through - this just doesn't guess at it from `as_user`
/// alone.
async fn gid_for(release: &Path, user: &str) -> Result<u32, Error> {
    resolve_id(release, user, "-g", "gid").await
}

/// Where `artifact` actually landed after the build, given the environment
/// it ran with.
///
/// A path beginning `target/` is resolved against `CARGO_TARGET_DIR`
/// instead, if the build set one: `build.env` exists for exactly this
/// case, and a shared target dir works by moving cargo's entire output
/// tree outside the release, so `target/release/koji` never actually
/// exists at that path on disk - the real file sits at
/// `$CARGO_TARGET_DIR/release/koji`. Every other artifact path is assumed
/// to already sit inside `release` at the path it names, which is the
/// ordinary case for a build with no such redirect.
///
/// Scoped to `CARGO_TARGET_DIR` specifically, not generalised to "any env
/// var that looks like a directory": it is the one redirect this crate's
/// design actually names, and guessing at others would invent behaviour
/// nothing has asked for yet.
fn artifact_source(release: &Path, env: &BTreeMap<String, String>, artifact: &Path) -> PathBuf {
    if let Some(target_dir) = env.get("CARGO_TARGET_DIR")
        && let Ok(rest) = artifact.strip_prefix("target")
    {
        return Path::new(target_dir).join(rest);
    }
    release.join(artifact)
}

/// Copies one declared artifact from wherever the build actually left it
/// into its named path inside `release`.
///
/// A no-op when the source and destination are already the same path -
/// the ordinary case when no `CARGO_TARGET_DIR` redirect is in play - and
/// that check is not a cheap-exit nicety: verified empirically, `fs::copy`
/// truncates a file to empty when its source and destination are the same
/// path, because it opens the destination for writing before it reads the
/// source. Skipping unconditionally would corrupt exactly the artifacts
/// this function exists to preserve.
///
/// # Errors
/// [`Error::Io`], naming the destination's parent, if that directory
/// cannot be created. [`Error::Io`], naming the source, if it cannot be
/// copied - most likely because the build never produced it at the path
/// declared.
fn copy_artifact(
    release: &Path,
    env: &BTreeMap<String, String>,
    artifact: &Path,
) -> Result<(), Error> {
    let from = artifact_source(release, env, artifact);
    let to = release.join(artifact);

    if from == to {
        return Ok(());
    }

    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_owned(),
            source,
        })?;
    }

    fs::copy(&from, &to).map_err(|source| Error::Io { path: from, source })?;

    Ok(())
}

/// Runs `spec.command` inside `release`, as `as_user` if given, then copies
/// every declared artifact back into the release.
///
/// An absent `command` is a no-op: `spec.artifacts` is never even
/// consulted, so declaring artifacts alongside no command copies nothing
/// rather than failing to find files a build that never ran could not have
/// produced. See the module doc for why an absent command is the right
/// reading rather than a misconfiguration.
///
/// The command runs through `sh -c`, with `release` as its working
/// directory and `spec.env` layered on top of whatever this process itself
/// inherited. Its stdout and stderr are inherited rather than captured, so
/// an operator watching a deploy sees the build's real output as it
/// happens instead of a blob replayed after the fact.
///
/// ## The one privilege boundary in this whole crate
///
/// When `as_user` is `Some`, the child drops to that user's uid **and**
/// primary gid - resolved via [`uid_for`]/[`gid_for`] - before it runs
/// anything at all. Uid alone is not enough: the design spec recommends
/// running the shepherd as root specifically so it can drop privileges, and
/// a child that only drops uid keeps whatever gid the shepherd itself was
/// running as - root's group, if the shepherd is root - for the entire
/// build. Dropping the gid too closes that gap, and doing both together
/// also clears supplementary groups for free: `std`'s own process-spawn
/// implementation runs `setgroups(0, ...)` whenever a uid and a gid are
/// both set and no explicit group list is given, which is exactly this
/// call shape (verified against the standard library's own source for the
/// unix `Command` spawn path, since this crate calls no `setgroups` or raw
/// libc itself).
///
/// The build executes code from somebody else's repository, so none of the
/// shepherd's own privilege - uid, primary gid, or supplementary groups -
/// may survive into it: a compromised build then gets the target sheep's
/// uid and gid and nothing more, which is the one control in this whole
/// design that actually bounds damage rather than merely detecting it.
///
/// `child.gid(...)` is called before `child.uid(...)` below, matching the
/// general Unix rule that a process must set its group *before* dropping
/// the uid that lets it change groups at all - reversing the two would
/// leave the gid drop silently ineffective once implemented with raw
/// syscalls. For this specific pair of `tokio::process::Command` builder
/// calls that ordering is not actually load-bearing: verified against the
/// standard library's own unix spawn path, `Command::spawn` performs
/// `setgid` before `setuid` internally whenever either is set, regardless
/// of which order the builder methods were called in. The source-level
/// order below is kept anyway because it reads as the intent it has, and
/// because a future change to how this crate drops privilege - raw
/// `libc::setuid`/`setgid`, or a different process API - would need to
/// preserve the real ordering rule this comment states, not just this
/// call's incidental one.
///
/// A different ordering effect *is* real here, and easy to mistake for the
/// syscall one above: `gid_for(...).await` runs before `uid_for(...).await`
/// textually, so for a user that resolves to neither, `gid_for` is the one
/// that fails and its message is the one `run` returns - not because the
/// privilege drop itself works any differently, but because whichever `id`
/// lookup is awaited first is also the one whose error surfaces first. This
/// is why the reject-side test below pins the specific "gid" wording rather
/// than only `Error::Config(_)`, and why `uid_for`/`gid_for` also each have
/// their own direct test - a `run`-level test alone can only ever observe
/// whichever of the two lookups happens to run first.
///
/// ## A failing build stops everything
///
/// A non-zero exit becomes [`Error::Build`]. This is the guard that keeps
/// a broken build from ever reaching the swap: the caller never moves
/// `current`, and the release being built lives in a directory the
/// running app does not share, so nothing already serving traffic is
/// touched. This is the part that replaces a hardcoded sleep between a
/// build and a restart in the deploy scripts this dog exists to retire.
///
/// # Errors
/// [`Error::Config`] if `as_user` names a user `id -u`/`id -g` cannot
/// resolve. [`Error::Io`], naming `release`, if the shell (or `id`, for
/// `as_user`) cannot even be launched, or if a declared artifact cannot be
/// copied - see [`copy_artifact`]. [`Error::Build`] if the command launches
/// and exits non-zero, or is killed by a signal, naming the exit status
/// when there is one.
pub async fn run(release: &Path, spec: &BuildSpec, as_user: Option<&str>) -> Result<(), Error> {
    let Some(command) = spec.command.as_deref() else {
        return Ok(());
    };

    let mut child = Command::new("sh");
    child.arg("-c").arg(command);
    child.current_dir(release);
    child.envs(&spec.env);

    if let Some(user) = as_user {
        child.gid(gid_for(release, user).await?);
        child.uid(uid_for(release, user).await?);
    }

    let status = child.status().await.map_err(|source| Error::Io {
        path: release.to_owned(),
        source,
    })?;

    if !status.success() {
        return Err(Error::Build {
            status: status.code(),
        });
    }

    for artifact in &spec.artifacts {
        copy_artifact(release, &spec.env, artifact)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Builds a release directory containing the given files - mirrors
    /// every other module's own `fixture_release`, e.g. `flockfile.rs`'s.
    fn fixture_release(files: &[(&str, &str)]) -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, contents) in files {
            fs::write(dir.path().join(name), contents).expect("write fixture file");
        }
        dir
    }

    /// A bare tempdir, for the tests below that need one standing in for a
    /// build cache rather than a release.
    fn tempdir() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// The username running this test process, via `id -un` rather than
    /// the `USER` environment variable - not every CI runner is guaranteed
    /// to set it, and asking the host directly is the same idiom [`uid_for`]
    /// itself uses.
    fn current_username() -> String {
        let output = std::process::Command::new("id")
            .arg("-un")
            .output()
            .expect("id -un runs");
        assert!(output.status.success(), "id -un must succeed");
        String::from_utf8(output.stdout)
            .expect("id -un prints utf-8")
            .trim()
            .to_owned()
    }

    /// This test process's own uid, via bare `id -u` (no username - `id`
    /// defaults to the caller).
    fn current_uid() -> u32 {
        let output = std::process::Command::new("id")
            .arg("-u")
            .output()
            .expect("id -u runs");
        assert!(output.status.success(), "id -u must succeed");
        String::from_utf8(output.stdout)
            .expect("id -u prints utf-8")
            .trim()
            .parse()
            .expect("id -u prints a number")
    }

    /// As [`current_uid`], for this test process's primary gid.
    fn current_gid() -> u32 {
        let output = std::process::Command::new("id")
            .arg("-g")
            .output()
            .expect("id -g runs");
        assert!(output.status.success(), "id -g must succeed");
        String::from_utf8(output.stdout)
            .expect("id -g prints utf-8")
            .trim()
            .parse()
            .expect("id -g prints a number")
    }

    /// fails if a failing build is treated as success. This is the guard
    /// that keeps a broken build from ever reaching the swap: current
    /// never moves and the running app is untouched, because it lives in a
    /// different directory.
    #[tokio::test]
    async fn a_failing_build_is_an_error() {
        let rel = fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("exit 3".into()),
            ..Default::default()
        };
        assert!(run(rel.path(), &spec, None).await.is_err());
    }

    /// fails if an absent build command is an error rather than a no-op.
    /// ReactMap run as `bun .` compiles its client at startup and declares
    /// no build at all; the readiness probe covers it.
    #[tokio::test]
    async fn an_absent_build_command_is_not_an_error() {
        let rel = fixture_release(&[]);
        let spec = BuildSpec::default();
        assert!(run(rel.path(), &spec, None).await.is_ok());
    }

    /// fails if declared artifacts are not copied into the release. With
    /// CARGO_TARGET_DIR pointed at a shared cache, the binary lands outside
    /// the release, and `script = ./target/release/koji` would resolve to
    /// nothing.
    #[tokio::test]
    async fn declared_artifacts_are_copied_into_the_release() {
        let rel = fixture_release(&[]);
        let cache = tempdir();
        std::fs::create_dir_all(cache.path().join("release")).unwrap();
        std::fs::write(cache.path().join("release/koji"), b"binary").unwrap();
        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/release/koji")],
        };
        run(rel.path(), &spec, None).await.expect("builds");
        assert!(rel.path().join("target/release/koji").exists());
    }

    /// fails if `Debug` stops redacting `env`'s values, or starts hiding a
    /// field entirely instead of just its values. Exact string pinned so a
    /// lazy `derive(Debug)` refactor fails here, the same way shep_core's
    /// own `AppConfig` test is pinned.
    #[test]
    fn debug_redacts_env_values() {
        let spec = BuildSpec {
            command: Some("make build".into()),
            env: [
                ("REGISTRY_TOKEN".to_string(), "secret".to_string()),
                ("RUST_LOG".to_string(), "info".to_string()),
            ]
            .into(),
            artifacts: vec![],
        };
        assert_eq!(
            format!("{spec:?}"),
            "BuildSpec { command: Some(\"make build\"), env: <2 vars>, artifacts: [] }"
        );
    }

    /// fails if an absent build command stops being a no-op the moment
    /// artifacts are also declared. `run` must return before ever looking
    /// at `spec.artifacts` when there is no command - a build that never
    /// ran cannot have produced anything, so attempting the copy here would
    /// fail on a file that was never going to exist, contradicting "an
    /// absent build command is a no-op".
    ///
    /// `CARGO_TARGET_DIR` is set here specifically so the declared
    /// artifact's source and destination are genuinely different paths -
    /// with no override, both sides of `copy_artifact` collapse to the
    /// same `release`-relative path and its self-copy guard would return
    /// `Ok` for a reason that has nothing to do with the command being
    /// absent, letting this test pass even if the early return above it
    /// were deleted entirely.
    #[tokio::test]
    async fn an_absent_command_skips_artifacts_too() {
        let rel = fixture_release(&[]);
        let cache = tempdir();
        let spec = BuildSpec {
            command: None,
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/release/nothing-built-this")],
        };
        assert!(run(rel.path(), &spec, None).await.is_ok());
    }

    /// fails if `as_user` is silently ignored on the accept side of the
    /// predicate. Dropping to the user already running the process must be
    /// a permitted no-op (`setuid`/`setgid` to one's own ids never require
    /// extra privilege), so this exercises the real
    /// `uid_for`/`gid_for` + `Command::uid`/`gid` path without needing root
    /// in CI - both ids drop together here, since the current user's own
    /// primary gid is, definitionally, whatever `gid_for` will resolve.
    #[tokio::test]
    async fn running_as_the_current_user_succeeds() {
        let rel = fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("true".into()),
            ..Default::default()
        };
        let user = current_username();
        run(rel.path(), &spec, Some(&user))
            .await
            .expect("dropping to one's own user and group is always permitted");
    }

    /// fails if a nonexistent `as_user` is silently accepted instead of
    /// refused. The reject side of the same predicate as the test above:
    /// resolving a user that does not exist on this host must fail loudly
    /// rather than, say, running as whatever uid/gid `0` or an unparsed
    /// value would mean.
    ///
    /// Asserts the *gid* wording specifically, not just `Error::Config(_)`
    /// in general: `run` resolves the gid before the uid (see `run`'s own
    /// doc comment for why), so for the same unknown user it is `gid_for`
    /// that fails first. A weaker assertion - `Error::Config(_)` alone -
    /// would keep passing even if the `child.gid(...)` call were deleted
    /// from `run` entirely, since `uid_for` would then fail instead and
    /// still produce *some* `Error::Config`; the specific wording is what
    /// proves the gid step actually ran. `uid_for_reports_a_config_error_
    /// for_an_unknown_user` below covers the uid half of the same shared
    /// `resolve_id` logic directly, independent of which one `run` happens
    /// to call first.
    #[tokio::test]
    async fn an_unknown_as_user_is_a_config_error() {
        let rel = fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("true".into()),
            ..Default::default()
        };
        let err = run(rel.path(), &spec, Some("shep-deploy-test-no-such-user"))
            .await
            .expect_err("no such user");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("does not resolve to a gid"));
    }

    /// fails if `uid_for` stops resolving to a known user's real uid.
    /// Exercised directly rather than through `run`, so it is not
    /// entangled with whichever of `uid_for`/`gid_for` `run` happens to
    /// call first.
    #[tokio::test]
    async fn uid_for_resolves_the_current_users_own_uid() {
        let user = current_username();
        let uid = uid_for(Path::new("."), &user).await.expect("resolves");
        assert_eq!(uid, current_uid());
    }

    /// fails if `gid_for` stops resolving to a known user's *primary* gid.
    /// This is the exact resolution the fix round added: `run` originally
    /// dropped only uid, which left a build's effective gid at whatever the
    /// shepherd's own process gid was - root's, if the shepherd runs as
    /// root specifically to be able to drop privileges at all, which the
    /// design spec recommends.
    #[tokio::test]
    async fn gid_for_resolves_the_current_users_primary_gid() {
        let user = current_username();
        let gid = gid_for(Path::new("."), &user).await.expect("resolves");
        assert_eq!(gid, current_gid());
    }

    /// fails if `uid_for`'s reject side stops naming its own specific
    /// wording. The `uid_for` half of `resolve_id`'s two call sites -
    /// `an_unknown_as_user_is_a_config_error` above only exercises the
    /// `gid_for` half, since that is the one `run` reaches first for the
    /// same bad username.
    #[tokio::test]
    async fn uid_for_reports_a_config_error_for_an_unknown_user() {
        let err = uid_for(Path::new("."), "shep-deploy-test-no-such-user")
            .await
            .expect_err("no such user");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("does not resolve to a uid"));
    }

    /// The `gid_for` counterpart of the test above, exercised directly for
    /// the same reason.
    #[tokio::test]
    async fn gid_for_reports_a_config_error_for_an_unknown_user() {
        let err = gid_for(Path::new("."), "shep-deploy-test-no-such-user")
            .await
            .expect_err("no such user");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("does not resolve to a gid"));
    }

    /// fails if the self-copy guard in `copy_artifact` is removed. Without
    /// it, declaring an artifact that the build already placed directly
    /// inside the release (the ordinary case with no `CARGO_TARGET_DIR`
    /// redirect) would copy the file onto itself - verified empirically to
    /// truncate it to zero bytes, since `fs::copy` opens its destination
    /// for writing before it reads the source. This test would go red
    /// immediately if that guard were deleted, since the content would come
    /// back empty rather than what the build actually wrote.
    #[tokio::test]
    async fn an_artifact_already_in_the_release_is_left_intact() {
        let rel = fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("mkdir -p dist && printf hello > dist/app.js".into()),
            artifacts: vec![PathBuf::from("dist/app.js")],
            ..Default::default()
        };
        run(rel.path(), &spec, None).await.expect("builds");
        let contents = fs::read_to_string(rel.path().join("dist/app.js")).expect("reads");
        assert_eq!(contents, "hello");
    }
}
