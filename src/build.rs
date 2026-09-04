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
//! Without it the deploy still works and the failure modes above still
//! hold, a failing build still never reaching the swap, but "a compromised
//! build gets the app's privileges and nothing more" is not one of the
//! guarantees in force. [`run`] warns when it is about to run a build as
//! root with no `user` set; it never refuses one.
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
//! - **`build.artifacts` exists for a build that writes outside the
//!   release.** Warm Rust builds do NOT need it: each sheep gets a
//!   dog-owned cache symlinked in as `target` (see
//!   [`crate::paths::Tree::cache_target`]), so compilation stays warm
//!   across releases AND a hardcoded `./target/release/koji` still
//!   resolves, with nothing copied back. Setting `CARGO_TARGET_DIR`
//!   instead was measured on 2026-08-26 and rejected: with it set
//!   `./target` is never created, so Koji's own `make build`, which ends
//!   in `cp ./target/release/koji koji`, exits 1. `artifacts` remains for
//!   the builds that genuinely do put their output somewhere the release
//!   cannot see, including one an operator points elsewhere themselves.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;

use crate::error::Error;
use crate::shared::{is_eloop, o_nofollow, printable};

/// The environment variables a build keeps from this process, by name.
///
/// Deliberately tiny. `sh -c` needs `PATH` to find anything; a toolchain
/// manager needs `HOME` to find its own installation; `LANG` and `LC_ALL`
/// keep a build's own output readable, and `TZ` keeps a timestamp in a
/// generated artifact from moving with the machine's default. Everything
/// else an operator wants is opted into by name through `passthrough` in
/// `[dog.deploy]`, so it appears in `shep.toml` where it can be read rather
/// than being inherited invisibly.
///
/// Notably absent and absent on purpose: `SSH_AUTH_SOCK`. A forwarded agent
/// reaching a build means the build can authenticate as the operator
/// anywhere that agent is trusted. `crate::git::fetch` runs in THIS process
/// and keeps its own environment, so a private repository still clones; it
/// is only the build that loses the socket, and an operator who genuinely
/// needs it during a build can name it in `passthrough`.
const BASE_ENV: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TZ"];

/// What to run to turn a freshly checked-out release into something its
/// declared script can execute, and what to salvage from wherever that run
/// actually left its output.
///
/// This is the `[dog.deploy.build]` block of a release's own Flockfile, parsed by
/// [`crate::flockfile::Merged::build_spec`]:
///
/// ```toml
/// [dog.deploy.build]
/// command = "bun install && bun run build"
/// env = { CARGO_TARGET_DIR = "/srv/cache/koji" }
/// artifacts = ["target/release/koji"]
/// ```
///
/// Every field is optional and an absent `[dog.deploy.build]` block is the default
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
    /// Environment for the build, layered over [`BASE_ENV`] and the
    /// operator's `passthrough`. The build inherits nothing else from the
    /// process running this dog: see [`run`]. `CARGO_TARGET_DIR` is the one
    /// entry this crate's design gives special meaning to - see
    /// [`artifact_source`], and it has to be absolute.
    pub env: BTreeMap<String, String>,
    /// Paths, relative to the release, to copy back in after a successful
    /// build. See the module doc for why this exists at all.
    ///
    /// Refused at parse time if absolute or containing `..` - see
    /// [`contained_artifacts`] for why that refusal is a security boundary
    /// and not a tidiness rule.
    #[serde(deserialize_with = "contained_artifacts")]
    pub artifacts: Vec<PathBuf>,
}

/// Refuses any artifact path that could name something outside the release.
///
/// This runs at parse time, before a build is spawned, because by the time
/// [`copy_artifact`] runs the privilege drop is already behind us: `uid` and
/// `gid` are set on the build's `Command`, so they bound the CHILD, and the
/// copy-back loop runs in this process afterwards at the dog's own uid. Under
/// the arrangement shep's docs recommend, that is root.
///
/// The escape is not hypothetical and the naive case hides it. With no
/// `CARGO_TARGET_DIR`, source and destination are the same expression, so any
/// `..` collapses to `from == to` and [`copy_artifact`]'s self-copy guard
/// returns early. Set `CARGO_TARGET_DIR` and the same string resolves against
/// a different base, so the two differ and the copy proceeds. Measured
/// 2026-08-28: `artifacts = ["target/../../../deploy.toml"]` with
/// `CARGO_TARGET_DIR` set writes through to the tree's own `deploy.toml`,
/// whose `remote` every later fetch reads. A commit on the tracked branch
/// could therefore repoint the deploy at a repository of its own choosing.
///
/// Refused rather than sanitised, matching
/// [`crate::flockfile`]'s treatment of a committed `user`: a path that cannot
/// mean what it says is an operator error worth naming, and silently
/// rewriting one would leave a Flockfile whose text and behaviour disagree.
///
/// # Errors
/// A deserialization error naming the offending path, if any entry is
/// absolute or contains a `..` component.
fn contained_artifacts<'de, D>(deserializer: D) -> Result<Vec<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    let artifacts = Vec::<PathBuf>::deserialize(deserializer)?;
    for artifact in &artifacts {
        if artifact.as_os_str().is_empty() {
            return Err(D::Error::custom(
                "build.artifacts has an empty entry, which names nothing; remove it or name a \
                 path relative to the release",
            ));
        }
        if artifact.is_absolute() {
            return Err(D::Error::custom(format!(
                "build.artifacts entry `{}` is an absolute path; artifacts are \
                 copied into the release and must be relative to it",
                printable(artifact.display())
            )));
        }
        if artifact
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(D::Error::custom(format!(
                "build.artifacts entry `{}` contains `..`, which would name a \
                 path outside the release",
                printable(artifact.display())
            )));
        }
    }
    Ok(artifacts)
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
        .map_err(Error::at(release))?;

    if !output.status.success() {
        return Err(Error::Config(format!(
            "the app's `user = {user:?}` does not resolve to a {kind} on this host (`id {flag} \
             {user}` failed); create that account, or pin a different one in \
             Flockfile.override.toml"
        )));
    }

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| Error::Config(format!("`id {flag} {user}` did not print a {kind}")))
}

/// This process's own effective uid, or `None` if it cannot be read.
///
/// Asked of `id -u`, with no user named, for the same reason
/// [`resolve_id`] asks it about somebody else: this crate is
/// `#![forbid(unsafe_code)]`, which rules out `geteuid(2)` directly, and
/// shelling out to the host needs no unsafe and no dependency added for one
/// call.
///
/// `None` rather than an error, and it folds every failure into that: the
/// only caller is a warning, and a deploy that cannot work out its own uid
/// should still deploy. What it loses is the warning, which is the right
/// thing to lose.
async fn effective_uid() -> Option<u32> {
    let output = Command::new("id").arg("-u").output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// The warning to print before running `sheep`'s build, or `None` when
/// there is nothing to warn about: [`effective_uid`] and
/// [`root_build_warning`] composed, which is everything [`run`] does with
/// them besides printing.
///
/// The split is where testability runs out and it is worth being exact
/// about where. The decision below is tested at every combination of uid
/// and `user`; this composition is tested against the uid the test host
/// actually has; the `eprintln!` in [`run`] is tested by nothing, because
/// capturing another function's stderr in-process is not something this
/// crate has a harness for. What stands in for a test there is the compile
/// gate: delete the call and this function has no callers, which
/// `-D warnings` refuses. That is weaker than a test and it is the reason
/// this is one function rather than two lines inline.
async fn root_warning(sheep: &str, as_user: Option<&str>) -> Option<String> {
    // A build that drops to a user has nothing to warn about, so the `id`
    // spawn is skipped rather than made and then thrown away, which it was
    // on every deploy of every correctly configured app.
    if as_user.is_some() {
        return None;
    }
    root_build_warning(sheep, effective_uid().await?, as_user)
}

/// The warning to print before running `sheep`'s build, or `None` when
/// there is nothing to warn about.
///
/// Fires only when both halves are true: this process is root, and the app
/// sets no `user` for the build to drop to. Then the build - a command the
/// deployed repository chose - runs with the shepherd's own privilege,
/// which is the exposure the module doc describes.
///
/// It warns and never refuses, which is this project owner's explicit
/// decision. An app with no `user` is a configuration shep itself accepts,
/// and a deploy dog is not the place to start declining it.
///
/// Split out from [`run`] so the decision can be tested at every
/// combination without the test having to be root, which is the one thing a
/// test of this cannot arrange.
fn root_build_warning(sheep: &str, euid: u32, as_user: Option<&str>) -> Option<String> {
    const ROOT: u32 = 0;

    (euid == ROOT && as_user.is_none()).then(|| {
        format!(
            "shep-deploy: warning: {sheep}'s build is about to run as root, because its app sets \
             no `user`. The build command comes from the deployed repository, so whatever that \
             repository can be made to run, it runs as root. Set `user` on the app to build as \
             that user instead."
        )
    })
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

/// The deepest ancestor of `path` that exists, fully resolved.
///
/// `canonicalize` fails on a path that does not exist yet, and an artifact's
/// destination usually does not, so this walks up until something does. What
/// comes back has every symlink on it followed, which is the whole point: a
/// destination is only safe if the directory it will really land in is safe.
fn resolve_deepest(path: &Path) -> Option<PathBuf> {
    let mut probe = path.to_owned();
    loop {
        if let Ok(real) = probe.canonicalize() {
            return Some(real);
        }
        if !probe.pop() {
            return None;
        }
    }
}

/// Whether `candidate` really lands under one of `roots`, symlinks followed.
///
/// The lexical check in [`contained_artifacts`] cannot answer this, and that
/// is not a gap in it but a limit of strings. A repository can commit a
/// symlink, and `crate::shared::link_cache` leaves a `target` entry alone when
/// the release already ships one, so `target/out` with no `..` and no leading
/// slash walks wherever that committed link points. Demonstrated 2026-08-28:
/// a release shipping `target -> /tmp/outside` had `target/victim` written
/// through it, at the dog's uid, which is root under the arrangement shep's
/// own docs recommend.
fn lands_within(roots: &[PathBuf], candidate: &Path) -> bool {
    resolve_deepest(candidate).is_some_and(|real| roots.iter().any(|root| real.starts_with(root)))
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
/// [`Error::Config`], naming the entry, in four cases: `artifact` is absolute
/// or contains `..` (see [`contained_artifacts`] for why that is a security
/// boundary); either end resolves outside the release and the build cache;
/// the destination changed underneath the check; or the build never produced
/// it at the path declared. [`Error::Io`] naming the destination's parent if
/// that directory cannot be created, the destination if it cannot be opened or
/// have its permissions set, and the source in every other case - including a
/// write to the destination that fails. `io::copy` does not report which side
/// of a copy an error came from, so the one path it can name is the artifact
/// that could not be copied. Read `No space left on device` against a source
/// path as the release's filesystem rather than the cache's.
fn copy_artifact(
    release: &Path,
    roots: &[PathBuf; 2],
    env: &BTreeMap<String, String>,
    artifact: &Path,
) -> Result<(), Error> {
    // Checked again here, not only in `contained_artifacts`, because a
    // `BuildSpec` can be built in code without going through the parser -
    // every test in this module does exactly that. Placed BEFORE the
    // `create_dir_all` below so a refused artifact cannot leave directories
    // behind on its way out, and before the `from == to` guard because that
    // guard is what hides the escape in the no-`CARGO_TARGET_DIR` case.
    if artifact.is_absolute()
        || artifact
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(Error::Config(format!(
            "build.artifacts entry `{}` would name a path outside the release",
            printable(artifact.display())
        )));
    }

    let from = artifact_source(release, env, artifact);
    let to = release.join(artifact);

    // Both ends checked against where they REALLY land, not against how they
    // are spelled. The lexical refusal above catches `..` and an absolute
    // path; it cannot catch a committed symlink, and it says nothing at all
    // about the source, which `CARGO_TARGET_DIR` points wherever the
    // repository's own Flockfile says. See `artifact_roots` for what the
    // two roots are and the escapes they were measured against.
    for (end, path) in [("destination", &to), ("source", &from)] {
        if !lands_within(roots, path) {
            return Err(Error::Config(format!(
                "build.artifacts entry `{}` resolves its {end} to `{}`, which is \
                 outside the release and its build cache",
                printable(artifact.display()),
                resolve_deepest(path)
                    .unwrap_or_else(|| path.clone())
                    .display()
            )));
        }
    }

    if from == to {
        // Already where it belongs, which is the ordinary case with no
        // `CARGO_TARGET_DIR` redirect: the build wrote it into the release
        // directly and there is nothing to copy.
        //
        // Checked rather than assumed, because this branch is reached for
        // EVERY declared artifact when no redirect is in play, and returning
        // `Ok` without looking makes an artifact that was never produced
        // indistinguishable from one that was. A build that exits 0 without
        // writing what it said it would, or a typo in `artifacts`, then rides
        // through the swap and the reload and surfaces as a runtime failure in
        // a release the operator has been told is deployed.
        if !to.exists() {
            return Err(Error::Config(format!(
                "build.artifacts names `{}`, which the build did not leave there; \
                 either the command did not produce it or the entry is wrong",
                printable(artifact.display())
            )));
        }
        return Ok(());
    }

    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(Error::at(parent))?;
    }

    // Opened BEFORE the last containment check, and the check then runs
    // against the object actually opened rather than against the name again.
    //
    // The build's own command can leave a background job running: `sh -c`
    // exits, `run` returns, and that job keeps going. It can swap a component
    // for a symlink in the gap between a name being approved and the same name
    // being followed a second time by `fs::copy`. Checking a path, then using
    // the path, is the check-then-use shape this whole review has been about.
    //
    // Comparing device and inode is what makes the two the same thing: the
    // handle cannot be redirected once open, so if the file it refers to is
    // the same file the resolved-and-contained path names, the read is of
    // something that passed the check.
    let mut source = fs::File::open(&from).map_err(Error::at(&from))?;
    let opened = source.metadata().map_err(Error::at(&from))?;
    let resolved = resolve_deepest(&from)
        .and_then(|real| fs::metadata(real).ok())
        .filter(|named| named.dev() == opened.dev() && named.ino() == opened.ino());
    if resolved.is_none() || !lands_within(roots, &from) {
        return Err(Error::Config(format!(
            "build.artifacts entry `{}` changed underneath the check; refusing to copy it",
            printable(artifact.display())
        )));
    }

    // `O_NOFOLLOW` on the destination, because the source was the only half
    // the previous fix covered. `File::create` follows symlinks and truncates,
    // so a component swapped in after the check was followed at write time,
    // at the dog's uid. Measured 2026-08-28: a background job toggling
    // `release/target` between a directory and a symlink won that race in
    // 0.03s and put build output outside the release.
    //
    // Refusing rather than resolving-and-rechecking: the kernel deciding at
    // open time is not a race, and a legitimate artifact destination is never
    // a symlink.
    // Identity, not spelling. `link_cache` symlinks `release/target` at the
    // dog's own cache, so a redirect through `CARGO_TARGET_DIR` gives a source
    // and a destination that differ as strings and name one file. The lexical
    // `from == to` above cannot see that, and opening the destination for
    // writing truncates the source before anything is read.
    //
    // The original `fs::copy` version carried a comment warning about exactly
    // this and guarded it lexically, which held only while `fs::copy` was the
    // route. Replacing it with an explicit open reopened the hole, so the
    // guard is now on the same footing as the source check above it.
    if fs::metadata(&to)
        .is_ok_and(|there| there.dev() == opened.dev() && there.ino() == opened.ino())
    {
        return Ok(());
    }

    // Resolved once more, because every line between the first check and this
    // open is a window a backgrounded build job can act in, and the first
    // check is several syscalls back: a `create_dir_all`, the source's own
    // open and two stats. `O_NOFOLLOW` below does not cover this - the kernel
    // honours it on the last component and follows every directory above it,
    // so a component swapped for a link during that window was followed.
    //
    // What is left is the gap between this line and the open itself. Closing
    // that needs a handle-based walk, which is a dependency and a rewrite
    // rather than a line this crate can spell: `docs/specs/deferred.md`
    // records the constraints such a walk has to satisfy, and what three
    // attempts at describing one got wrong. A recorded decision rather than
    // an oversight either way.
    //
    // A component that is ALREADY a link when the first check runs is caught
    // there rather than here, at any depth, because `resolve_deepest` follows
    // the whole chain. Refusing every symlinked component instead was tried
    // and is wrong: `shared::link_cache` makes `release/target` a link at the
    // dog's own cache, so that refusal broke the ordinary cargo arrangement.
    if !lands_within(roots, &to) {
        return Err(Error::Config(format!(
            "build.artifacts entry `{}` changed underneath the check; refusing to copy it",
            printable(artifact.display())
        )));
    }

    // Unlinked first, then created fresh with `create_new`, and the two
    // together are the fix for a hard link. The checks above resolve names
    // and compare inodes, and a hard link has neither a name to resolve nor
    // an inode of its own: `release/dist/app.js` linked by the build to a
    // file elsewhere on the same filesystem passes every one of them, and
    // opening it for writing then truncates and rewrites the far file at the
    // dog's uid. `remove_file` takes the link away without touching what it
    // pointed at, and `create_new` refuses to open anything that appeared in
    // between, symlink or otherwise. That also covers the `O_NOFOLLOW` case
    // below on its own terms: a symlink here is gone before the open.
    //
    // Only a regular file is unlinked. A directory, a symlink or anything
    // else at the destination is not an artifact the build left, and
    // unlinking it would take the release's own `target -> cache` link with
    // it when `artifacts` names `target` itself: `remove_file` removes a
    // link where the old `O_NOFOLLOW` open refused one. The window between
    // this look, the unlink and the open is the one `docs/specs/deferred.md`
    // records; what a job swapping a parent for a link inside it can now
    // cost is a file unlinked as well as one written.
    match fs::symlink_metadata(&to) {
        Ok(there) if !there.file_type().is_file() => {
            return Err(Error::Config(format!(
                "build.artifacts entry `{}` names a destination that is not a regular file; \
                 refusing to replace it",
                printable(artifact.display())
            )));
        }
        Ok(_) => fs::remove_file(&to).map_err(Error::at(&to))?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(Error::Io {
                path: to.clone(),
                source: err,
            });
        }
    }
    let mut sink = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(o_nofollow())
        .open(&to)
        .map_err(|err| {
            // Something reappeared at the name between the unlink and the
            // open. A backgrounded build job is the only thing that could,
            // and the copy is refused rather than written through it. Said
            // as a refusal, because the raw `File exists` or `Too many
            // levels of symbolic links` reads as a broken host.
            if err.kind() == io::ErrorKind::AlreadyExists || is_eloop(&err) {
                Error::Config(format!(
                    "build.artifacts entry `{}` had its destination replaced underneath the \
                     copy; refusing to write through it",
                    printable(artifact.display())
                ))
            } else {
                Error::Io {
                    path: to.clone(),
                    source: err,
                }
            }
        })?;
    io::copy(&mut source, &mut sink).map_err(Error::at(&from))?;

    // `fs::copy` carried the source's mode and this does not: a file created
    // by `OpenOptions` gets `0o666 & !umask`, so 0755 arrives as 0644 and the
    // executable bit is gone. That was silent and total for the arrangement
    // this whole function exists to serve. `CARGO_TARGET_DIR` points at the
    // dog's cache, cargo builds the binary there, `copy_artifact` brings it
    // back so `script = "./target/release/koji"` resolves, and shep then
    // cannot exec it. The release builds cleanly, fails verification, and is
    // rolled back for a reason nothing reports.
    //
    // Set from the handle rather than the path, so it lands on the file that
    // was actually written rather than on whatever the name resolves to now.
    // Same reasoning as the `O_NOFOLLOW` open above.
    //
    // Masked to the ordinary nine bits, which is the difference between
    // copying a mode and copying a privilege. `Permissions` carries setuid,
    // setgid and the sticky bit too, and this copy runs in the PARENT at the
    // dog's own uid, which shep's docs recommend be root. So a build dropped to
    // `svc` could write `target/x` with mode 04755, and this would create a
    // root-owned file in the release and make it setuid: anyone who can run it
    // is root, and the release is what the app serves.
    //
    // Introduced by the fix one commit earlier that made the copy keep the
    // executable bit. Preserving the mode was right; preserving all of it was
    // not.
    let mode = opened.permissions().mode() & 0o777;
    sink.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(Error::at(to))?;

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
/// directory and an environment built from scratch: [`BASE_ENV`], then
/// whatever the operator's `passthrough` names, then `spec.env`, and
/// nothing this process itself inherited beyond those. Its stdout and
/// stderr are inherited rather than captured, so
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
/// resolve, if a declared artifact resolves outside the release and the build
/// cache, or if one the build was meant to produce is not there - see
/// [`copy_artifact`] for all three. [`Error::Io`] naming `release` if the shell
/// (or `id`, for `as_user`) cannot even be launched, and naming the artifact's
/// source, its destination, or that destination's parent if a declared
/// artifact cannot be copied - see [`copy_artifact`], which names the source
/// for the copy itself even when it was the destination that failed.
/// [`Error::Build`] if the command launches and
/// exits non-zero, or is killed by a signal, naming the exit status when there
/// is one. [`Error::BuildTimedOut`] if it runs past `budget`, which is
/// `build_timeout` from `[dog.deploy]`.
pub async fn run(
    sheep: &str,
    release: &Path,
    spec: &BuildSpec,
    as_user: Option<&str>,
    passthrough: &[String],
    cache: &Path,
    budget: Duration,
) -> Result<(), Error> {
    let Some(command) = spec.command.as_deref() else {
        return Ok(());
    };

    // Once, here, rather than at parse time or per instance: this is the
    // moment the exposure becomes real, and an absent command never reaches
    // it because there is nothing to run.
    if let Some(warning) = root_warning(sheep, as_user).await {
        eprintln!("{warning}");
    }

    // Refused before the build runs, not when its artifacts are copied: a
    // relative `CARGO_TARGET_DIR` is resolved against wherever this dog
    // happens to be running, so cargo would build somewhere no artifact
    // path could name and the refusal at copy time printed a bare relative
    // path the operator could not find on disk.
    if let Some(target_dir) = spec.env.get("CARGO_TARGET_DIR")
        && !Path::new(target_dir).is_absolute()
    {
        return Err(Error::Config(format!(
            "[dog.deploy.build] env sets CARGO_TARGET_DIR = {target_dir:?}, which is not an \
             absolute path; a relative one is resolved against whatever directory this dog \
             is running in, not against the release. Name the directory in full"
        )));
    }

    let mut child = Command::new("sh");
    child.arg("-c").arg(command);
    child.current_dir(release);

    // Cleared, then rebuilt deliberately. Dropping uid and gid below bounds
    // what the build can TOUCH; it does nothing about what it can READ out of
    // its own environment, because those values are copied into the child
    // before any of it happens. A dog started with a registry token or a
    // forwarded agent socket in its environment would hand both to every
    // build it runs, and a build command is chosen by whoever can land a
    // commit on the tracked branch.
    //
    // What survives is named in three places and nowhere else: BASE_ENV, the
    // operator's `passthrough` list, and the release's own `[dog.deploy.build] env`.
    //
    // `var_os` rather than `var`, because `var` reports a value that is not
    // UTF-8 as absent and a `PATH` with one such component then vanished,
    // leaving the build on `sh`'s built-in default path.
    child.env_clear();
    for key in BASE_ENV {
        if let Some(value) = env::var_os(key) {
            child.env(key, value);
        }
    }
    for key in passthrough {
        match env::var_os(key) {
            Some(value) => {
                child.env(key, value);
            }
            // Said, because `passthrough` is the only door for a registry
            // token and a typo in it used to be a build that silently did
            // not get the token. The config is checked at parse time; whether
            // the variable is SET is only known here.
            None => eprintln!(
                "shep-deploy: {sheep}: passthrough names {key}, which is not set in this dog's \
                 environment, so the build does not get it"
            ),
        }
    }
    child.envs(&spec.env);

    if let Some(user) = as_user {
        child.gid(gid_for(release, user).await?);
        child.uid(uid_for(release, user).await?);
    }

    // Its own process group, so a build that has to be abandoned can be
    // abandoned whole. `sh -c` is a shell, and what hangs is usually something
    // it started rather than the shell itself, so killing only the shell would
    // leave the real process running while the deploy reports a failure. Same
    // shape and same reason as `crate::shared::run_git_within`.
    child.process_group(0);

    let mut child = child.spawn().map_err(Error::at(release))?;

    // Captured before the wait, because `Child::id` answers `None` once the
    // child has been reaped and the group is named after the leader's pid.
    let group = child.id();

    // Bounded, because an unbounded wait here does not stop one target, it
    // stops the dog. `crate::poll::tick` deploys targets one at a time, so a
    // build that never finishes holds that loop forever: no other target
    // deploys, no smit is refreshed, and nothing is logged, because from the
    // loop's point of view nothing has gone wrong. That is the failure
    // `git_timeout` was added for, one subprocess along.
    //
    // A test that drives a real build cannot use `#[tokio::test(start_paused
    // = true)]` because of this line. Paused time advances to the next
    // deadline whenever the runtime is idle, and a real subprocess makes it
    // idle, so the budget expires at once whatever it is set to. See
    // `crate::deploy`'s `a_current_that_moved_during_the_build_is_not_swapped_over`.
    let status = match timeout(budget, child.wait()).await {
        Ok(waited) => waited.map_err(Error::at(release))?,
        Err(_) => {
            abandon(&mut child).await;
            return Err(Error::BuildTimedOut { after: budget });
        }
    };

    // The group goes when the build does, however the build ended. `sh -c`
    // exiting is not the same as the build being over: a command can leave a
    // job running behind it, and this module's own comments have said so since
    // round 4, because that job is the thing every containment check below is
    // racing. It can swap a component between a name being approved and the
    // same name being opened, and one was measured winning that race in 0.03s.
    //
    // Killing it here removes the racer instead of out-manoeuvring it, which no
    // amount of re-checking can do on its own. It also stops a build leaving a
    // process behind that outlives the deploy and can still write to a release
    // an operator has been told is live.
    //
    // A process that genuinely means to persist calls `setsid` and leaves the
    // group, so a compiler daemon survives this and a stray `&` does not. That
    // is the right split: one asked to outlive its parent, the other just did.
    if let Some(pid) = group {
        crate::shared::kill_group(pid);
    }

    if !status.success() {
        return Err(Error::Build {
            status: status.code(),
        });
    }

    // Resolved once for every artifact rather than once per artifact:
    // `canonicalize` walks the whole chain and the two roots do not change.
    let roots = artifact_roots(release, cache);
    for artifact in &spec.artifacts {
        copy_artifact(release, &roots, &spec.env, artifact)?;
    }

    Ok(())
}

/// The two places an artifact may really land, resolved: the release and
/// the dog's own build cache for this sheep.
///
/// `cache` is passed in rather than read from `release/target`, which is
/// exactly the entry an attacker controls: `link_cache` leaves a release's
/// own `target` alone when it ships one.
///
/// Two escapes measured 2026-08-28, both writing or reading at the dog's uid
/// because the copy runs in the parent, after the build's own drop:
/// `target -> /tmp/outside` wrote through the committed link, and
/// `CARGO_TARGET_DIR = /any/path` with `artifacts = ["target/id_rsa"]` read
/// an arbitrary file into the release, where a static-serving app then
/// hands it out over HTTP.
fn artifact_roots(release: &Path, cache: &Path) -> [PathBuf; 2] {
    [
        release
            .canonicalize()
            .unwrap_or_else(|_| release.to_owned()),
        cache.canonicalize().unwrap_or_else(|_| cache.to_owned()),
    ]
}

/// Kills an abandoned build's whole process group.
///
/// The group, not the child: `run` spawns `sh -c`, and a hung build is
/// normally something the shell started. Killing the shell alone leaves that
/// running against the release the deploy is about to give up on.
///
/// Failures are ignored throughout. This runs on the path where the build has
/// already been given up on, and a kill that cannot be delivered leaves
/// nothing this function can do about it that the caller has not already
/// decided.
async fn abandon(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        crate::shared::kill_group(pid);
    }
    // The group signal is the one that matters; this covers a host whose
    // `kill` refuses the group form.
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    /// fails if a build that SUCCEEDS leaves its background job running.
    ///
    /// `sh -c` exiting is not the build being over. A command can leave a job
    /// behind, and this module has known that since round 4: it is named in
    /// `copy_artifact` as the thing every containment check is racing, and one
    /// was measured winning that race in 0.03s, swapping a component between a
    /// name being approved and the same name being opened.
    ///
    /// Every fix for that raced the job. None removed it. A build that exits 0
    /// with a job still running left it running, free to alter the release
    /// after the artifacts were validated and to outlive the deploy entirely.
    ///
    /// A process that means to persist calls `setsid` and leaves the group, so
    /// this kills a stray `&` and not a compiler daemon.
    #[tokio::test]
    async fn a_successful_build_does_not_leave_its_background_job_running() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("sleep 400 & echo $! > bg.pid".into()),
            env: BTreeMap::new(),
            artifacts: vec![],
        };

        build(rel.path(), &spec).await.expect("this build exits 0");

        let pid = std::fs::read_to_string(rel.path().join("bg.pid"))
            .expect("the build wrote its background job's pid");
        assert!(
            !still_running(pid.trim()),
            "the background job (pid {}) outlived the build that started it",
            pid.trim()
        );
    }

    /// fails if a build that never finishes is waited on forever.
    ///
    /// It does not stop one target, it stops the dog. `crate::poll::tick`
    /// deploys targets one at a time, so a build that hangs holds that loop
    /// with no other target deploying, no smit refreshed and nothing logged,
    /// because from the loop's point of view nothing has failed. The same
    /// shape `git_timeout` exists to prevent for a fetch.
    ///
    /// Not `start_paused`, and it cannot be: see the note on the timeout in
    /// `run` for why a real subprocess and a paused clock cannot be combined.
    #[tokio::test]
    async fn a_build_that_never_finishes_is_abandoned() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            // `&` plus `wait` keeps the SHELL alive with a descendant. A bare
            // `sleep 600` is exec-replaced by the shell, so the child being
            // signalled would be `sleep` itself and `child.kill()` alone would
            // end it: the test would pass whether or not the group kill worked,
            // which makes it no test of the group at all.
            //
            // The descendant writes its own pid, so the assertion below can be
            // about the process rather than about the error. Asserting only
            // that the build timed out leaves the group kill exactly as
            // unproven as the bare `sleep 600` did.
            command: Some("sleep 600 & echo $! > descendant.pid; wait".into()),
            env: BTreeMap::new(),
            artifacts: vec![],
        };

        let err = run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            fixtures::tempdir().path(),
            // Two seconds, not 250ms, for the reason `shep`'s own
            // `run_bounded` test learned the hard way: a budget this small is
            // shared with process startup, and on a loaded runner the shell
            // has not yet written its pid when the deadline fires. The
            // assertion below then reads a file that is not there and fails
            // for a reason that has nothing to do with process groups.
            //
            // Still three hundred times shorter than the `sleep 600` it has to
            // interrupt, so what the test is about is unchanged.
            Duration::from_secs(2),
        )
        .await
        .expect_err("a build that never finishes must be abandoned");

        assert!(
            matches!(err, Error::BuildTimedOut { .. }),
            "must say it timed out rather than that it failed: {err:?}"
        );
        assert!(
            format!("{err}").contains("build_timeout"),
            "must name the knob that changes it: {err}"
        );

        // The whole point of the process group. Killing the shell alone would
        // leave this running against a release the deploy has given up on.
        let pid = std::fs::read_to_string(rel.path().join("descendant.pid")).expect(
            "the build never got far enough to write its descendant's pid, so this \
             proves nothing about the group kill either way",
        );
        assert!(
            !still_running(pid.trim()),
            "the backgrounded descendant (pid {}) outlived the build it belonged to",
            pid.trim()
        );
    }

    /// fails if an artifact the build never produced is reported as copied.
    ///
    /// With no `CARGO_TARGET_DIR` redirect, `copy_artifact`'s source and
    /// destination collapse to the same release-relative path, so the
    /// already-where-it-belongs branch is taken for EVERY declared artifact.
    /// Returning `Ok` there without looking made an artifact that was never
    /// written indistinguishable from one that was: a build exiting 0 without
    /// producing what it declared, or a typo in `artifacts`, rode through the
    /// swap and the reload and surfaced as a runtime failure in a release the
    /// operator had been told was deployed.
    #[tokio::test]
    async fn an_artifact_the_build_did_not_produce_is_a_failure() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("true".into()),
            env: BTreeMap::new(),
            artifacts: vec![PathBuf::from("dist/app.js")],
        };

        let err = build(rel.path(), &spec)
            .await
            .expect_err("a declared artifact that is not there must fail the build");

        assert!(
            format!("{err}").contains("did not leave there"),
            "must say what is missing and why: {err}"
        );
    }

    /// fails if a copied artifact keeps setuid, setgid or the sticky bit.
    ///
    /// The copy runs in the PARENT, at the dog's own uid, which shep's docs
    /// recommend be root so it can drop per app. So the destination is created
    /// root-owned. A build dropped to an unprivileged `user` that writes
    /// `target/x` with mode 04755 would then have this hand it back as a
    /// root-owned setuid executable inside the release, which is what the app
    /// serves. Anyone who can run it is root.
    ///
    /// Introduced by the commit that made the copy preserve the executable
    /// bit. Preserving the mode was right; preserving all of it was not.
    #[tokio::test]
    async fn a_copied_artifact_does_not_keep_setuid() {
        use std::os::unix::fs::PermissionsExt;

        let rel = fixtures::fixture_release(&[]);
        let cache = fixtures::tempdir();
        let built = cache.path().join("koji");
        std::fs::write(&built, b"#!/bin/sh\necho hi\n").expect("the build's output");
        std::fs::set_permissions(&built, std::fs::Permissions::from_mode(0o4755)).expect("chmod");

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/koji")],
        };

        run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            cache.path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect("builds and copies");

        let mode = std::fs::metadata(rel.path().join("target/koji"))
            .expect("copied")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o7777,
            0o755,
            "the copy must keep the permission bits and drop the privilege ones"
        );
    }

    /// fails if a copied artifact loses the source's permission bits.
    ///
    /// This is the arrangement the redirect branch exists for: cargo builds
    /// into `CARGO_TARGET_DIR`, and the binary is copied back so a `script`
    /// pointing into `target/release` resolves. `fs::copy` carried the mode.
    /// Replacing it with `OpenOptions` plus `io::copy` did not, so 0755
    /// arrived as 0644, shep could not exec the binary, and a release that
    /// built cleanly failed verification and rolled back with nothing saying
    /// why.
    ///
    /// The old test asserted the destination merely existed, which a
    /// non-executable file does.
    #[tokio::test]
    async fn a_copied_artifact_keeps_the_source_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let rel = fixtures::fixture_release(&[]);
        let cache = fixtures::tempdir();
        let built = cache.path().join("koji");
        std::fs::write(&built, b"#!/bin/sh\necho hi\n").expect("the build's output");
        std::fs::set_permissions(&built, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/koji")],
        };

        run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            cache.path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect("builds and copies");

        let copied = rel.path().join("target/koji");
        let mode = std::fs::metadata(&copied)
            .expect("copied")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "the copy must be executable, or shep cannot start the release it just built"
        );
    }

    /// Whether `pid` is still alive, polled rather than sampled once.
    ///
    /// Signalling a group and the kernel reaping its members are not the same
    /// instant, so one `kill -0` immediately after the build returns can catch
    /// a process that is already doomed and answer "alive". Bounded so a
    /// genuine leak still fails rather than hanging.
    ///
    /// `kill -0` because `#![forbid(unsafe_code)]` rules out the syscall, and
    /// its exit status is exactly the question being asked.
    fn still_running(pid: &str) -> bool {
        for _ in 0..50 {
            let alive = std::process::Command::new("kill")
                .args(["-0", pid])
                .status()
                .expect("kill -0")
                .success();
            if !alive {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// A build budget the test tier can never legitimately hit.
    ///
    /// Every build here is `true` or a one-line shell command, so anything
    /// slower than a minute is a hang worth failing on rather than waiting
    /// out. Distinct from the production default, which is an hour because a
    /// cold build of a real workspace legitimately runs tens of minutes.
    const TEST_BUILD_BUDGET: Duration = Duration::from_secs(60);

    /// Runs a build with every knob at its default: sheep `"web"`, no
    /// `as_user`, no passthrough, a fresh cache tempdir, and
    /// `TEST_BUILD_BUDGET`.
    ///
    /// Exists because this module's own `run` calls had started to drift the
    /// way `fixtures.rs`'s module doc describes for `run_git` and
    /// `head_of`: one nine-line block, copied a couple dozen times, one
    /// budget or one cache dir away from its neighbor by accident rather
    /// than intent. A test that needs a specific cache, user, passthrough or
    /// budget still calls `run` directly instead of going through this.
    async fn build(rel: &Path, spec: &BuildSpec) -> Result<(), Error> {
        run(
            "web",
            rel,
            spec,
            None,
            &[],
            fixtures::tempdir().path(),
            TEST_BUILD_BUDGET,
        )
        .await
    }

    use crate::fixtures;

    use super::*;

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

    /// fails if the root warning stops firing on the one combination that
    /// warrants it, or starts firing on one that does not. Both halves have
    /// to be true: this process is root, and the app names no `user` for
    /// the build to drop to.
    ///
    /// Tested as a decision rather than through `run`, because the one
    /// input that matters is this process's uid and a test cannot arrange
    /// to be root.
    #[test]
    fn only_a_rootless_build_as_root_is_warned_about() {
        assert!(root_build_warning("web", 0, None).is_some());

        // Root, but the build drops to somebody: this is the configuration
        // the warning exists to ask for.
        assert!(root_build_warning("web", 0, Some("reactmap")).is_none());
        // Not root: there is no shepherd privilege to hand a build.
        assert!(root_build_warning("web", 501, None).is_none());
        assert!(root_build_warning("web", 501, Some("reactmap")).is_none());
    }

    /// fails if the warning stops naming the sheep, what the exposure is,
    /// or what to do about it. It is the only notice an operator gets, and
    /// a warning that says "running as root" without saying whose code or
    /// which knob is one they learn to skip.
    #[test]
    fn the_root_warning_names_the_sheep_and_the_way_out() {
        let warning = root_build_warning("bpm", 0, None).expect("warns");
        assert!(warning.contains("bpm"), "{warning}");
        assert!(warning.contains("root"), "{warning}");
        assert!(warning.contains("repository"), "{warning}");
        assert!(warning.contains("`user`"), "{warning}");
    }

    /// fails if this process cannot read its own effective uid. The warning
    /// above is skipped entirely when it cannot, so this is what says the
    /// skip is not silently permanent.
    #[tokio::test]
    async fn the_effective_uid_can_be_read() {
        assert!(effective_uid().await.is_some());
    }

    /// fails if reading the uid and deciding on it stop being wired
    /// together. It asserts against whatever uid this host runs tests as
    /// rather than against a fixed answer, so it says the same thing as
    /// root and as anybody else: an app that names a `user` is never warned
    /// about, and an app that does not is warned about exactly when this
    /// process is root.
    #[tokio::test]
    async fn the_warning_reads_this_process_and_agrees_with_the_decision() {
        let euid = effective_uid().await.expect("a uid");

        assert!(root_warning("web", Some("reactmap")).await.is_none());
        assert_eq!(
            root_warning("web", None).await.is_some(),
            euid == 0,
            "warned about as uid {euid}"
        );
    }

    /// fails if a failing build is treated as success. This is the guard
    /// that keeps a broken build from ever reaching the swap: current
    /// never moves and the running app is untouched, because it lives in a
    /// different directory.
    #[tokio::test]
    async fn a_failing_build_is_an_error() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("exit 3".into()),
            ..Default::default()
        };
        assert!(build(rel.path(), &spec).await.is_err());
    }

    /// fails if an absent build command is an error rather than a no-op.
    /// ReactMap run as `bun .` compiles its client at startup and declares
    /// no build at all; the readiness probe covers it.
    #[tokio::test]
    async fn an_absent_build_command_is_not_an_error() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec::default();
        assert!(build(rel.path(), &spec).await.is_ok());
    }

    /// fails if declared artifacts are not copied into the release. With
    /// CARGO_TARGET_DIR pointed at a shared cache, the binary lands outside
    /// the release, and `script = ./target/release/koji` would resolve to
    /// nothing.
    #[tokio::test]
    async fn declared_artifacts_are_copied_into_the_release() {
        let rel = fixtures::fixture_release(&[]);
        // The redirect points at the dog's OWN cache, which is the only
        // outside-the-release source that is still allowed. See
        // `a_source_outside_the_release_and_cache_is_refused` for why.
        let cache = fixtures::tempdir();
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
        run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            cache.path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect("builds");
        assert!(rel.path().join("target/release/koji").exists());
    }

    /// fails if the dog's own environment leaks into a build unasked.
    ///
    /// Dropping uid and gid bounds what a build can TOUCH. It does nothing
    /// about what the build can READ out of its environment, because those
    /// values are copied into the child before the drop happens. A dog
    /// started with a registry token in its environment would hand it to
    /// every build, and the build command is chosen by whoever can land a
    /// commit on the tracked branch.
    ///
    /// `CARGO_PKG_NAME` is the probe because cargo sets it for the test
    /// process itself, so it is genuinely present in this process's
    /// environment and genuinely absent from `BASE_ENV`. Setting one here
    /// instead is not available: `std::env::set_var` is unsafe in edition
    /// 2024 and this crate forbids unsafe outright.
    #[tokio::test]
    async fn the_dogs_own_environment_does_not_reach_a_build() {
        assert!(
            std::env::var("CARGO_PKG_NAME").is_ok(),
            "the probe variable must exist in this process or the test proves nothing"
        );
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("printenv CARGO_PKG_NAME > leaked.txt; true".into()),
            env: BTreeMap::new(),
            artifacts: vec![],
        };
        build(rel.path(), &spec).await.expect("builds");
        let leaked = std::fs::read_to_string(rel.path().join("leaked.txt")).unwrap_or_default();
        assert!(
            leaked.trim().is_empty(),
            "the build saw CARGO_PKG_NAME = {leaked:?}, so the environment was inherited"
        );
    }

    /// fails if `BASE_ENV` stops reaching the build.
    ///
    /// Nothing else pins it: with the `BASE_ENV` loop deleted the suite
    /// stayed green, because `sh` resolves `printenv` through its own
    /// built-in default path when `PATH` is unset. A build with no `PATH`
    /// and no `HOME` would find `cargo` and `npm` missing on a host where
    /// both are installed, which reads as a broken host rather than a
    /// dropped variable.
    #[tokio::test]
    async fn the_base_environment_reaches_the_build() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("printenv PATH > path.txt; printenv HOME > home.txt; true".into()),
            env: BTreeMap::new(),
            artifacts: vec![],
        };
        build(rel.path(), &spec).await.expect("builds");
        for name in ["path.txt", "home.txt"] {
            let value = std::fs::read_to_string(rel.path().join(name)).unwrap_or_default();
            assert!(
                !value.trim().is_empty(),
                "{name} is empty, so a BASE_ENV variable did not reach the build"
            );
        }
    }

    /// fails if `passthrough` stops being the way a build gets a variable.
    ///
    /// The counterpart to the test above: the bound is only usable if there
    /// is a way through it, and that way has to be named in `shep.toml` so
    /// the exposure is readable rather than inherited.
    #[tokio::test]
    async fn a_named_passthrough_variable_reaches_the_build() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("printenv CARGO_PKG_NAME > passed.txt; true".into()),
            env: BTreeMap::new(),
            artifacts: vec![],
        };
        run(
            "web",
            rel.path(),
            &spec,
            None,
            &["CARGO_PKG_NAME".to_owned()],
            fixtures::tempdir().path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect("builds");
        assert_eq!(
            std::fs::read_to_string(rel.path().join("passed.txt"))
                .expect("the build wrote it")
                .trim(),
            "shep-deploy"
        );
    }

    /// fails if an artifact can truncate the file it is copying.
    ///
    /// `link_cache` symlinks `release/target` at the dog's own cache, so a
    /// `CARGO_TARGET_DIR` pointing there gives a source and a destination that
    /// differ as strings and name one file. Opening the destination for
    /// writing then empties the source before anything is read.
    ///
    /// The original `fs::copy` version guarded this lexically and said so in
    /// its own doc. Replacing the copy with an explicit open reopened the
    /// hole, which is why the guard now compares device and inode like the
    /// containment check above it rather than comparing spellings.
    #[tokio::test]
    async fn an_artifact_that_is_already_where_it_belongs_is_not_truncated() {
        let rel = fixtures::fixture_release(&[]);
        let cache = fixtures::tempdir();
        std::fs::create_dir_all(cache.path().join("release")).expect("cache");
        std::fs::write(cache.path().join("release/koji"), b"binary").expect("built");
        // Exactly what `link_cache` does.
        std::os::unix::fs::symlink(cache.path(), rel.path().join("target")).expect("link");

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/release/koji")],
        };
        run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            cache.path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect("builds");

        assert_eq!(
            std::fs::read(cache.path().join("release/koji")).expect("still there"),
            b"binary",
            "the artifact must survive being copied onto itself"
        );
    }

    /// fails if `CARGO_TARGET_DIR` can read a file from anywhere on the host.
    ///
    /// The simplest of the three escapes and the one needing no trick at all:
    /// `env` comes from the deployed repository's own Flockfile, and nothing
    /// validated where it pointed. `artifacts = ["target/id_rsa"]` is relative
    /// and has no `..`, so both lexical guards pass, and `copy_artifact` then
    /// reads whatever that source names into the release. No build code runs.
    ///
    /// It matters because the copy happens in THIS process, at the dog's uid,
    /// which shep's docs recommend running as root. And a release is the
    /// directory a static-serving app hands out over HTTP.
    #[tokio::test]
    async fn a_source_outside_the_release_and_cache_is_refused() {
        let rel = fixtures::fixture_release(&[]);
        let elsewhere = fixtures::tempdir();
        std::fs::write(elsewhere.path().join("id_rsa"), b"PRIVATE KEY").expect("secret");

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                elsewhere.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/id_rsa")],
        };
        let err = build(rel.path(), &spec)
            .await
            .expect_err("a source outside the tree must be refused");
        assert!(
            format!("{err}").contains("outside the release"),
            "must say why: {err}"
        );
        assert!(
            !rel.path().join("target/id_rsa").exists(),
            "nothing may be read into the release"
        );
    }

    /// fails if a committed symlink can carry a write out of the release.
    ///
    /// No `..` and no absolute path, so the lexical guard passes and cannot
    /// help: the escape is a filesystem object, not a string. A repository can
    /// commit a symlink, and `crate::shared::link_cache` leaves a `target`
    /// entry alone when the release already ships one, so the dog writes
    /// straight through it at its own uid.
    #[tokio::test]
    async fn a_committed_target_symlink_cannot_carry_a_write_out() {
        let rel = fixtures::fixture_release(&[]);
        let outside = fixtures::tempdir();
        std::fs::write(outside.path().join("victim"), b"original").expect("victim");
        std::os::unix::fs::symlink(outside.path(), rel.path().join("target")).expect("link");

        let cache = fixtures::tempdir();
        std::fs::write(cache.path().join("victim"), b"ATTACKER").expect("payload");

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/victim")],
        };
        let err = run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            cache.path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect_err("a write through a committed symlink must be refused");
        assert!(
            format!("{err}").contains("outside the release"),
            "must say why: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("victim")).expect("still there"),
            "original",
            "the file outside the release must be untouched"
        );
    }

    /// fails if a symlinked component BELOW `target` can carry a write out.
    ///
    /// `a_committed_target_symlink_cannot_carry_a_write_out` covers `target`
    /// itself. This covers a component one level deeper, which is where the
    /// round-7 review expected the guard to end: `O_NOFOLLOW` is honoured on
    /// the leaf only, and `create_dir_all` treats an existing
    /// symlink-to-directory as a job already done. What actually holds the
    /// line here is neither of those, it is `lands_within` resolving the
    /// deepest existing ancestor before anything is created, so the depth of
    /// the component does not matter.
    ///
    /// Verified 2026-08-28 that the equivalent link pointing INSIDE the
    /// release does copy through, and that this is correct rather than a
    /// second hole: `shared::link_cache` makes `release/target` a link at the
    /// dog's own cache, so refusing every symlinked component would refuse
    /// the ordinary cargo arrangement.
    #[tokio::test]
    async fn a_symlinked_component_below_target_cannot_carry_a_write_out() {
        let rel = fixtures::fixture_release(&[]);
        let cache = fixtures::tempdir();
        std::fs::create_dir(cache.path().join("sub")).expect("build output dir");
        std::fs::write(cache.path().join("sub/out"), b"ARTIFACT").expect("build output");

        let outside = fixtures::tempdir();
        std::fs::create_dir(rel.path().join("target")).expect("target");
        std::os::unix::fs::symlink(outside.path(), rel.path().join("target/sub")).expect("link");

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/sub/out")],
        };
        let err = run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            cache.path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect_err("a write through a symlinked component must be refused");
        assert!(
            format!("{err}").contains("outside the release"),
            "must say why: {err}"
        );
        assert!(
            !outside.path().join("out").exists(),
            "nothing may be written outside the release"
        );
    }

    /// fails if a `..` in a declared artifact can reach outside the release.
    ///
    /// This is a security boundary, not tidiness. `copy_artifact` runs in
    /// THIS process after the child exited, so the `uid`/`gid` set on the
    /// build's `Command` never applied to it; under the arrangement shep's
    /// docs recommend that is root. Measured 2026-08-28 against the unfixed
    /// code: this exact spec overwrote the tree's own `deploy.toml`, whose
    /// `remote` every later fetch reads, so a commit on the tracked branch
    /// could repoint the deploy at a repository of its own.
    ///
    /// `CARGO_TARGET_DIR` is load-bearing here and the test is worthless
    /// without it. With no override, `from` and `to` are the same expression,
    /// so the `..` collapses to `from == to` and the self-copy guard returns
    /// `Ok` before reaching anything this test is about. The unfixed code
    /// passes a version of this test that omits the override.
    #[tokio::test]
    async fn an_artifact_that_escapes_the_release_is_refused() {
        let tree = fixtures::tempdir();
        let release = tree.path().join("releases/abc123");
        std::fs::create_dir_all(&release).unwrap();
        let sentinel = tree.path().join("deploy.toml");
        std::fs::write(&sentinel, b"remote = \"https://real.example/repo.git\"").unwrap();

        let cache = fixtures::tempdir();
        let stolen = cache.path().join("deploy.toml");
        std::fs::write(&stolen, b"remote = \"https://attacker.example/evil.git\"").unwrap();

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().join("a/b/c").display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/../../../deploy.toml")],
        };

        let err = build(&release, &spec)
            .await
            .expect_err("an escaping artifact must be refused");
        assert!(
            format!("{err}").contains("outside the release"),
            "the refusal must say why, got: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "remote = \"https://real.example/repo.git\"",
            "the tree's own state file must be untouched"
        );
    }

    /// fails if an escaping artifact reaches a build at all.
    ///
    /// The refusal above is the last line; this is the first. A Flockfile
    /// naming such a path is refused when it is parsed, so no build is ever
    /// spawned for it, matching how `crate::flockfile` refuses a committed
    /// `user` rather than ignoring it.
    #[test]
    fn an_escaping_artifact_is_refused_at_parse_time() {
        for bad in ["target/../../../deploy.toml", "/etc/passwd"] {
            let toml = format!("command = \"true\"\nartifacts = [\"{bad}\"]\n");
            let err = toml::from_str::<BuildSpec>(&toml)
                .expect_err("an escaping artifact must not parse");
            assert!(
                format!("{err}").contains(bad),
                "the refusal must name the entry, got: {err}"
            );
        }
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
        let rel = fixtures::fixture_release(&[]);
        let cache = fixtures::tempdir();
        let spec = BuildSpec {
            command: None,
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/release/nothing-built-this")],
        };
        assert!(build(rel.path(), &spec).await.is_ok());
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
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("true".into()),
            ..Default::default()
        };
        let user = current_username();
        run(
            "web",
            rel.path(),
            &spec,
            Some(&user),
            &[],
            fixtures::tempdir().path(),
            TEST_BUILD_BUDGET,
        )
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
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("true".into()),
            ..Default::default()
        };
        let err = run(
            "web",
            rel.path(),
            &spec,
            Some("shep-deploy-test-no-such-user"),
            &[],
            fixtures::tempdir().path(),
            TEST_BUILD_BUDGET,
        )
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
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("mkdir -p dist && printf hello > dist/app.js".into()),
            artifacts: vec![PathBuf::from("dist/app.js")],
            ..Default::default()
        };
        build(rel.path(), &spec).await.expect("builds");
        let contents = fs::read_to_string(rel.path().join("dist/app.js")).expect("reads");
        assert_eq!(contents, "hello");
    }
}
