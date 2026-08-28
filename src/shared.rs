//! Deriving which of the operator's checkout files a fresh release needs.
//!
//! A fresh `git worktree` contains nothing git ignores, so it cannot run:
//! ReactMap needs `config/local.json`, a generated masterfile and several
//! more, none of which are in the repository. Something has to put them
//! there, and the rule is the operator's own checkout plays the role a
//! `shared/` directory would otherwise play - see [`to_link`].
//!
//! Three steps, one function each: [`ignored_present`] asks git what it
//! ignores and finds present on disk right now, [`shepignore_patterns`]
//! reads the operator's opt-out list, and [`to_link`] subtracts the second
//! from the first. [`link_into`] is the only function here that writes
//! anything, and it never writes to the checkout - only into a release.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::Error;

/// Runs `git <args>` in `dir` and returns its stdout as a `String`.
///
/// `pub(crate)` rather than private: [`crate::git`] shells out to git for a
/// second directory this crate cares about - the bare clone under
/// [`crate::paths::Tree::git`] - and needs the exact same error mapping this
/// module already built, so it reuses this rather than growing a second
/// copy. The parameter is named `dir`, not `checkout`, because it is called
/// with both: this module's own functions always pass the operator's
/// checkout, `crate::git`'s pass the deploy engine's own bare clone.
///
/// Launching a subprocess and decoding what it printed are not filesystem
/// calls, but they fail with the same shape of error - an
/// [`std::io::Error`] and a path worth naming - and this crate has nowhere
/// else for that shape to live, so both come back as [`Error::Io`] naming
/// `dir`. A `git` invocation that launches but exits non-zero is
/// [`Error::Git`] instead, since `git`'s own stderr is worth keeping
/// separate from "could not even run it".
/// [`run_git`], abandoning the subprocess if it outlives `budget`.
///
/// Only [`crate::git::fetch`] uses this, and only because it is the one git
/// invocation that talks to a network. The other ten callers operate on local
/// directories and cannot hang on a remote that stopped answering.
///
/// Why it matters more than an ordinary slow call: the poll loop deploys
/// targets one at a time, so an unbounded fetch does not fail one target, it
/// stops every target and the smit refresh with it, with no error and no log
/// line. A remote behind a firewall that drops packets rather than refusing
/// them produces exactly that, and it is an ordinary misconfiguration.
///
/// The child is killed on expiry rather than left to finish, because a git
/// process still holding the bare clone's lock would fail the next tick too.
///
/// Residual, stated rather than hidden: this still occupies the caller for up
/// to `budget`. The runtime is single-threaded, so a target whose remote is a
/// black hole delays the others by that much. Bounded and reported beats
/// unbounded and silent, which is the whole of what this buys.
///
/// # Errors
/// [`Error::Git`] naming the command and a `None` status if `budget` elapses
/// first. Otherwise exactly what [`run_git`] returns.
pub(crate) fn run_git_within(dir: &Path, args: &[&str], budget: Duration) -> Result<String, Error> {
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;

    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Err(source) => {
                let _ = child.kill();
                return Err(Error::Io {
                    path: dir.to_owned(),
                    source,
                });
            }
            Ok(Some(_)) => break,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            // Killed, then reaped: leaving it would keep the bare clone's
            // lock and fail the next tick for a reason that looks unrelated.
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Git {
                command: format!("git {}", args.join(" ")),
                status: None,
                stderr: format!(
                    "no answer within {}s; abandoned so the other targets keep deploying",
                    budget.as_secs()
                ),
            });
        }
        thread::sleep(POLL);
    }

    let output = child.wait_with_output().map_err(|source| Error::Io {
        path: dir.to_owned(),
        source,
    })?;
    decode(dir, args, &output)
}

/// How often [`run_git_within`] asks whether the child has finished.
///
/// Fifty milliseconds: short enough that a fetch finishing is not perceptibly
/// delayed, long enough that a five-minute budget costs six thousand cheap
/// syscalls rather than a busy loop.
const POLL: Duration = Duration::from_millis(50);

pub(crate) fn run_git(dir: &Path, args: &[&str]) -> Result<String, Error> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;

    decode(dir, args, &output)
}

/// Turns a finished `git` invocation into stdout or the error it earned.
///
/// Shared by [`run_git`] and [`run_git_within`] rather than duplicated: the
/// two differ only in how they wait for the child, and a second copy of this
/// is how the bounded path would drift into reporting failures differently
/// from the unbounded one.
fn decode(dir: &Path, args: &[&str], output: &std::process::Output) -> Result<String, Error> {
    if !output.status.success() {
        return Err(Error::Git {
            command: format!("git {}", args.join(" ")),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    String::from_utf8(output.stdout.clone()).map_err(|err| Error::Io {
        path: dir.to_owned(),
        source: io::Error::other(err),
    })
}

/// Every path in `checkout` that git ignores and that exists on disk right
/// now, relative to `checkout`.
///
/// Asks `git status --ignored=matching --porcelain` rather than parsing
/// `.gitignore` by hand. Parsing gets negations
/// (`!server/src/configs/.gitkeep`), anchored globs (`/docker-compose.yml`)
/// and nested ignore files in subdirectories wrong; git already answers
/// this question correctly, because it is git's own question to answer.
///
/// `=matching` (over the porcelain default, which git calls `traditional`)
/// is what keeps a wholly-ignored directory as one entry - `node_modules/`,
/// correctly, since it is meant to move as a single symlink - while still
/// naming an individually-ignored file on its own when it sits beside
/// ordinary tracked content, such as `config/local.json` next to a tracked
/// `config/schema.sql`. The default `traditional` mode collapses both cases
/// down to the containing directory, which would make a single ignored
/// file impossible to symlink without dragging its tracked siblings along
/// as dangling links.
///
/// # Errors
/// [`Error::Io`] if `git` cannot be launched or answers with non-UTF-8
/// bytes; [`Error::Git`] if it launches but exits non-zero.
pub fn ignored_present(checkout: &Path) -> Result<Vec<PathBuf>, Error> {
    let stdout = run_git(
        checkout,
        &[
            "-c",
            "core.quotePath=false",
            "status",
            "--ignored=matching",
            "--porcelain",
        ],
    )?;

    Ok(stdout
        .lines()
        .filter_map(|line| line.strip_prefix("!! "))
        .map(|path| PathBuf::from(path.trim_end_matches('/')))
        .collect())
}

/// The patterns listed in `checkout`'s `.shepignore`, one per line, blank
/// lines and `#`-comments dropped.
///
/// An absent file is not an error: it returns an empty list, which is the
/// zero-configuration case - no `.shepignore` means share everything
/// [`ignored_present`] finds.
///
/// A pattern containing a glob metacharacter (`*`, `?`, `[`) is refused
/// with [`Error::Config`] rather than accepted and silently matched
/// against nothing. `.shepignore`'s syntax is narrower than `.gitignore`'s,
/// see [`to_link`] for what it does support, and an operator who writes
/// `*.log` believing otherwise deserves a failure they see immediately,
/// naming the pattern, rather than an artifact this subtraction was built
/// to keep out quietly staying shared forever because the glob never
/// matched anything.
///
/// # Errors
/// [`Error::Io`], naming `checkout/.shepignore`, if the file exists but
/// cannot be read for any reason other than simply not being there.
/// [`Error::Config`] if any pattern contains a glob metacharacter.
pub fn shepignore_patterns(checkout: &Path) -> Result<Vec<String>, Error> {
    let path = checkout.join(".shepignore");

    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::Io { path, source }),
    };

    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|pattern| {
            if pattern.contains(['*', '?', '[']) {
                Err(Error::Config(format!(
                    ".shepignore pattern {pattern:?} uses glob syntax (`*`, `?`, `[`), which \
                     is not supported - a .shepignore pattern is a bare name (matches at any \
                     depth) or a path containing `/` (anchored to the checkout root), nothing \
                     else"
                )))
            } else {
                Ok(pattern.to_owned())
            }
        })
        .collect()
}

/// Whether `path` (relative to the checkout) is named by `.shepignore`
/// entry `pattern`.
///
/// Follows `.gitignore`'s own anchoring rule and nothing more: a pattern
/// with no `/` in it matches a path component at any depth, so
/// `node_modules` excludes both the top-level directory and every
/// `packages/*/node_modules` beneath it. A pattern containing `/` is
/// anchored to the checkout root instead and matches only that exact
/// subtree, so `packages/app/dist` never touches a top-level `dist`.
/// Wildcards never reach this function: `shepignore_patterns` refuses any
/// pattern containing a glob metacharacter before `to_link` ever calls
/// this, rather than accepting one and silently matching nothing.
fn pattern_matches(path: &Path, pattern: &str) -> bool {
    let pattern = Path::new(pattern);

    if pattern.components().count() == 1 {
        path.components()
            .any(|component| component.as_os_str() == pattern.as_os_str())
    } else {
        path.starts_with(pattern)
    }
}

/// [`ignored_present`], minus whatever `.shepignore` names.
///
/// This subtraction is the entire reason `.shepignore` exists. `.gitignore`
/// conflates config that must be shared, caches that may be, and build
/// outputs that must not, because git ignores all three for the same
/// reason. Symlinking a build output back to a single shared copy means the
/// next release's build writes straight through that link, replacing the
/// assets the current release is serving mid-build; rolling back afterwards
/// then serves the new build's output under the old release's name. That
/// kills blue/green and rollback together, which is why this subtraction
/// must never be skipped.
///
/// # Errors
/// Whatever [`ignored_present`] or [`shepignore_patterns`] returns.
pub fn to_link(checkout: &Path) -> Result<Vec<PathBuf>, Error> {
    let ignored = ignored_present(checkout)?;
    let patterns = shepignore_patterns(checkout)?;

    Ok(ignored
        .into_iter()
        .filter(|path| {
            !patterns
                .iter()
                .any(|pattern| pattern_matches(path, pattern))
        })
        .collect())
}

/// Symlinks every path in `paths` from `checkout` into `release`, creating
/// whatever parent directories the release needs along the way.
///
/// `checkout` is canonicalised before anything is joined onto it. A
/// symlink's target text is stored exactly as given - `symlink()` performs
/// no resolution of its own - and the OS later resolves a relative target
/// against the *symlink's own containing directory*, not against this
/// process's working directory or against whatever the caller meant by
/// `checkout`. A relative `checkout` therefore produced a symlink whose
/// target text was embedded literally and dangled the moment anything
/// read through it: `symlink()` itself still succeeded, so the deploy
/// would carry on and the break would only surface after the swap and
/// after the reload, when something finally tried to read a shared file.
/// Canonicalising first makes the target text absolute regardless of what
/// form `checkout` arrived in, and turns a checkout that cannot be
/// resolved at all into an immediate, named error instead of a link that
/// looks fine until it is used.
///
/// Reads from `checkout` and writes only under `release` - the dog never
/// writes to the operator's own checkout, and any code path that would is a
/// bug. `paths` are relative, the same relative paths [`to_link`] returns,
/// and are joined onto both roots here: onto `checkout` to find the real
/// file, onto `release` to decide where its symlink belongs.
///
/// # Errors
/// [`Error::Io`], naming `checkout`, if it cannot be canonicalised - it
/// does not exist, or a component of it cannot be resolved. Otherwise
/// [`Error::Io`], naming the release-side path that failed, if a parent
/// directory cannot be created or the symlink itself cannot be made.
///
/// One collision is not an [`Error::Io`] at all. A release-side path that
/// already exists gives [`Error::Config`] instead, because the cause is a
/// `.shepignore` that shares something the release builds for itself, which
/// the operator fixes by editing that file rather than by looking at the
/// filesystem. The message names the colliding path and the file to edit.
pub fn link_into(release: &Path, checkout: &Path, paths: &[PathBuf]) -> Result<(), Error> {
    let checkout = fs::canonicalize(checkout).map_err(|source| Error::Io {
        path: checkout.to_owned(),
        source,
    })?;

    for relative in paths {
        let target = checkout.join(relative);
        let link = release.join(relative);

        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_owned(),
                source,
            })?;
        }

        symlink(&target, &link).map_err(|source| {
            if source.kind() == io::ErrorKind::AlreadyExists {
                return Error::Config(format!(
                    "{} is already present in the release, so {} cannot be linked from the \
                     checkout. The usual cause is a build output that git ignores and \
                     `.shepignore` does not: this dog gives each sheep its own build cache and \
                     links it in first, and the operator's own build artifacts must not be \
                     shared into a release at all, because the next release's build would write \
                     through the link and replace what the current one is serving. Add {} to \
                     `.shepignore` in the checkout.",
                    link.display(),
                    relative.display(),
                    relative.display()
                ));
            }
            Error::Io {
                path: link.clone(),
                source,
            }
        })?;
    }

    Ok(())
}

/// Points `release/target` at the dog's own build cache, creating the cache
/// if this is the first release to want it.
///
/// Runs BEFORE [`link_into`], so a checkout that shares its own `target`
/// (no `.shepignore`, which the design says is a misconfiguration rather
/// than a mode) collides in `link_into` where the error can name the fix,
/// rather than here where it cannot.
///
/// A release that already has a `target` path is left exactly as it is and
/// gets no cache. That is a repository which committed the directory, which
/// is unusual and its own business; overruling it would be this crate
/// deciding the repository's layout for it.
///
/// # Errors
/// [`Error::Io`], naming the cache, if it cannot be created; naming the
/// link, if the symlink cannot be made for any reason other than something
/// already being there.
pub fn link_cache(release: &Path, cache_target: &Path) -> Result<(), Error> {
    let link = release.join("target");
    if link.exists() || link.symlink_metadata().is_ok() {
        return Ok(());
    }

    fs::create_dir_all(cache_target).map_err(|source| Error::Io {
        path: cache_target.to_owned(),
        source,
    })?;

    symlink(cache_target, &link).map_err(|source| Error::Io { path: link, source })
}

#[cfg(test)]
mod tests {
    use crate::fixtures;

    /// fails if a git subprocess can outlive its budget.
    ///
    /// Without the bound this hangs forever rather than failing, which is the
    /// whole point: the poll loop deploys targets one at a time, so an
    /// unanswered fetch stops every target with no error and no log line.
    ///
    /// `10.255.255.1` is RFC 1918 space that routes nowhere on an ordinary
    /// host, so the connect blocks rather than being refused. Measured before
    /// this test was written: the same fetch was still blocked after three
    /// seconds. A refusal would make this test pass for the wrong reason, so
    /// it asserts on the timeout's own message rather than merely on `Err`.
    #[test]
    fn a_git_subprocess_that_never_answers_is_abandoned() {
        let dir = tempfile::tempdir().expect("tempdir");
        run_git(dir.path(), &["init", "-q"]).expect("init");

        let err = run_git_within(
            dir.path(),
            &[
                "fetch",
                "git://10.255.255.1/x",
                "+refs/heads/*:refs/heads/*",
            ],
            Duration::from_millis(400),
        )
        .expect_err("an unanswered fetch must not hang");

        let said = format!("{err}");
        assert!(
            said.contains("no answer within"),
            "must fail on the budget, not on something else: {said}"
        );
    }

    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Builds a throwaway git repo for one test: `entries` are (path,
    /// contents) pairs written to disk, then `git add .` and committed.
    /// `git add` silently skips anything matched by `.gitignore`, so an
    /// entry meant to be "ignored and present" - `config/local.json` in the
    /// fixtures below - simply stays untracked while a `.gitignore` or
    /// `tracked.txt` entry lands in the commit. That is exactly the split
    /// every test here needs: something tracked, something ignored.
    fn fixture_repo(entries: &[(&str, &str)]) -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fixtures::run_git(dir.path(), &["init", "-q"]);
        fixtures::run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        fixtures::run_git(dir.path(), &["config", "user.name", "test"]);

        for (path, contents) in entries {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("mkdir fixture parent");
            }
            fs::write(&full, contents).expect("write fixture file");
        }

        fixtures::run_git(dir.path(), &["add", "."]);
        fixtures::run_git(dir.path(), &["commit", "-q", "-m", "seed"]);
        dir
    }

    /// Guards `link_into_resolves_even_when_checkout_is_relative`, the one
    /// test in this module that mutates the process's current directory.
    /// `std::env::set_current_dir` is global, process-wide state that
    /// Rust's default parallel test runner does nothing to serialise, so a
    /// lock is the difference between "one test briefly changes cwd" and
    /// "two threads race to change and restore cwd at once".
    static CWD_GUARD: Mutex<()> = Mutex::new(());

    /// fails if enumeration stops using git's own answer. Parsing
    /// `.gitignore` by hand gets negations (`!server/src/configs/.gitkeep`),
    /// anchored globs (`/docker-compose.yml`) and nested ignore files
    /// wrong; `git status --ignored` gets all three right because it is
    /// git deciding.
    #[test]
    fn enumeration_asks_git_rather_than_parsing_gitignore() {
        let repo = fixture_repo(&[
            (".gitignore", "config/\n!config/.gitkeep\n"),
            ("config/local.json", "{}"),
            ("config/.gitkeep", ""),
            ("tracked.txt", "x"),
        ]);
        let found = ignored_present(repo.path()).expect("enumerates");
        assert!(found.iter().any(|p| p.ends_with("config")));
        assert!(!found.iter().any(|p| p.ends_with("tracked.txt")));
    }

    /// fails if a plain untracked-but-not-ignored file is treated as shared.
    /// `ignored_present` keeps only lines beginning `!! `; a file git status
    /// reports as `?? ` (untracked, not ignored) has no business in this
    /// list, and nothing in the test above proves that half of the filter -
    /// its only untracked-looking entry (`tracked.txt`) is committed, not
    /// merely present.
    #[test]
    fn ignored_present_excludes_untracked_files_that_are_not_ignored() {
        let repo = fixture_repo(&[(".gitignore", "dist/\n"), ("dist/app.js", "//")]);
        fs::write(repo.path().join("scratch.txt"), "untracked, not ignored")
            .expect("write scratch file");

        let found = ignored_present(repo.path()).expect("enumerates");
        assert!(found.iter().any(|p| p.ends_with("dist")));
        assert!(!found.iter().any(|p| p.ends_with("scratch.txt")));
    }

    /// fails if a `.shepignore` entry is still linked. This is the whole
    /// reason `.shepignore` exists: symlinking a build output means release
    /// B's build writes through the link and replaces what release A is
    /// currently serving, which kills blue/green and rollback in one line.
    #[test]
    fn shepignored_paths_are_not_linked() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\nconfig/local.json\n"),
            (".shepignore", "dist\n"),
            ("dist/app.js", "//"),
            ("config/local.json", "{}"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(linked.iter().any(|p| p.ends_with("config/local.json")));
        assert!(!linked.iter().any(|p| p.ends_with("dist")));
    }

    /// fails if a repo with no `.shepignore` stops sharing everything
    /// ignored. That is the zero-configuration case and the common one.
    #[test]
    fn no_shepignore_means_share_everything_ignored() {
        let repo = fixture_repo(&[(".gitignore", "config/\n"), ("config/local.json", "{}")]);
        assert!(!to_link(repo.path()).expect("computes").is_empty());
    }

    /// fails if `.shepignore` parsing stops skipping blank lines, or stops
    /// skipping `#` comments - two separate clauses of the same filter, and
    /// a filter proven on one clause can still be broken on the other.
    /// `shepignored_paths_are_not_linked` above only exercises a
    /// `.shepignore` with neither blank lines nor comments in it, so this
    /// is the only test standing between either clause and going unguarded.
    #[test]
    fn shepignore_patterns_skips_blank_lines_and_comments() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (
                ".shepignore",
                "# build output, never share this\n\ndist\n\n",
            ),
            ("dist/app.js", "//"),
        ]);
        let patterns = shepignore_patterns(repo.path()).expect("reads");
        assert_eq!(patterns, vec!["dist".to_string()]);
    }

    /// fails if a bare `.shepignore` pattern stops matching a nested
    /// directory of the same name. ReactMap's real ignored set has
    /// `node_modules/` at the top level and several more nested under
    /// `packages/*/node_modules/`; a user writing `node_modules` in
    /// `.shepignore` means all of them, matching how a bare name in
    /// `.gitignore` itself matches at any depth.
    #[test]
    fn shepignore_bare_pattern_matches_at_any_depth() {
        let repo = fixture_repo(&[
            (".gitignore", "node_modules/\ndist/\n"),
            (".shepignore", "node_modules\n"),
            ("node_modules/a.js", "//"),
            ("packages/foo/node_modules/b.js", "//"),
            ("dist/app.js", "//"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(!linked.iter().any(|p| p.ends_with("node_modules")));
        assert!(linked.iter().any(|p| p.ends_with("dist")));
    }

    /// fails if a `.shepignore` pattern containing `/` stops being anchored
    /// to the checkout root. A naive "match the last component anywhere"
    /// implementation would make `packages/dist` also exclude an unrelated
    /// top-level `dist`; the anchored rule must exclude only the exact
    /// subtree named.
    #[test]
    fn shepignore_pattern_with_slash_is_anchored_to_its_own_subtree() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\npackages/dist/\n"),
            (".shepignore", "packages/dist\n"),
            ("dist/app.js", "//"),
            ("packages/dist/bundle.js", "//"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(linked.contains(&PathBuf::from("dist")));
        assert!(!linked.contains(&PathBuf::from("packages/dist")));
    }

    /// fails if `link_into` stops actually linking back to the checkout, or
    /// stops creating the parent directories a nested shared path needs.
    /// `config/local.json` exercises both: the release has no `config/`
    /// directory until `link_into` makes one, and the file it links to must
    /// still be the checkout's own copy, not one dragged along.
    #[test]
    fn link_into_creates_symlinks_that_resolve_into_the_checkout() {
        let repo = fixture_repo(&[
            (".gitignore", "config/local.json\n"),
            ("config/local.json", r#"{"real":true}"#),
        ]);
        let release = tempfile::tempdir().expect("release tempdir");

        let paths = to_link(repo.path()).expect("computes");
        link_into(release.path(), repo.path(), &paths).expect("links");

        let linked_path = release.path().join("config").join("local.json");
        assert!(linked_path.is_symlink());
        let contents = fs::read_to_string(&linked_path).expect("read through symlink");
        assert_eq!(contents, r#"{"real":true}"#);
    }

    /// fails if `link_into` goes back to embedding `checkout` literally as
    /// the symlink's target text. A relative `checkout` used to produce a
    /// symlink whose target the OS resolves against the symlink's own
    /// directory inside the release, not against anything the caller
    /// meant - `symlink()` itself never noticed, so the only way to catch
    /// this is to actually read through the result. cwd is changed to the
    /// checkout's own parent so `relative_checkout` is a genuinely relative
    /// path the fix must canonicalise, not merely a path that happens to
    /// already be absolute.
    #[test]
    fn link_into_resolves_even_when_checkout_is_relative() {
        let repo = fixture_repo(&[
            (".gitignore", "config/local.json\n"),
            ("config/local.json", r#"{"real":true}"#),
        ]);
        let release = tempfile::tempdir().expect("release tempdir");
        let paths = vec![PathBuf::from("config/local.json")];

        let _guard = CWD_GUARD.lock().expect("cwd guard poisoned");
        let original_cwd = std::env::current_dir().expect("read cwd");
        std::env::set_current_dir(repo.path().parent().expect("repo has a parent"))
            .expect("chdir into repo's parent");
        let relative_checkout = PathBuf::from(repo.path().file_name().expect("repo has a name"));

        let result = link_into(release.path(), &relative_checkout, &paths);

        std::env::set_current_dir(&original_cwd).expect("restore cwd");
        result.expect("links despite a relative checkout");

        let linked_path = release.path().join("config").join("local.json");
        let contents = fs::read_to_string(&linked_path).expect("read through symlink");
        assert_eq!(contents, r#"{"real":true}"#);
    }

    /// fails if `link_into` stops surfacing a checkout it cannot resolve as
    /// an immediate error. Silently doing nothing, or creating a dangling
    /// link anyway, is exactly the failure-that-looks-like-success shape
    /// the canonicalisation fix exists to close off.
    #[test]
    fn link_into_fails_loudly_when_checkout_does_not_exist() {
        let release = tempfile::tempdir().expect("release tempdir");
        let missing_checkout = release.path().join("no-such-checkout");
        let paths = vec![PathBuf::from("config/local.json")];

        let err = link_into(release.path(), &missing_checkout, &paths)
            .expect_err("a checkout that does not exist cannot be canonicalised");
        assert!(matches!(err, Error::Io { .. }));
    }

    /// fails if a `.shepignore` pattern using `*` stops being refused. An
    /// operator writing `*.log`, trusting the spec's "same idiom as
    /// .gitignore" line, must get a loud failure naming the pattern rather
    /// than a glob that silently matches nothing forever while the
    /// artifact it named stays shared.
    #[test]
    fn shepignore_refuses_a_pattern_with_an_asterisk() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "*.log\n"),
            ("dist/app.js", "//"),
        ]);
        let err = shepignore_patterns(repo.path()).expect_err("must refuse a glob pattern");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("*.log"));
    }

    /// fails if a `.shepignore` pattern using `?` stops being refused - the
    /// second of the three metacharacters `pattern_matches` never gets a
    /// chance to mishandle, since none of them are meant to reach it.
    #[test]
    fn shepignore_refuses_a_pattern_with_a_question_mark() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "cache?.tmp\n"),
            ("dist/app.js", "//"),
        ]);
        let err = shepignore_patterns(repo.path()).expect_err("must refuse a glob pattern");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("cache?.tmp"));
    }

    /// fails if a `.shepignore` pattern using a `[...]` class stops being
    /// refused - the third metacharacter, and the one most likely to be
    /// dropped from a hand-written `contains` check without a test naming
    /// it specifically.
    #[test]
    fn shepignore_refuses_a_pattern_with_a_bracket_class() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "cache[0-9].tmp\n"),
            ("dist/app.js", "//"),
        ]);
        let err = shepignore_patterns(repo.path()).expect_err("must refuse a glob pattern");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("cache[0-9].tmp"));
    }

    /// fails if a release does not get a `target` pointing at the dog's own
    /// cache. Without it every deploy of a Rust project is a from-scratch
    /// build, which the design calls not acceptable for Koji specifically.
    #[test]
    fn a_release_gets_target_linked_at_the_cache() {
        let root = tempfile::tempdir().expect("tempdir");
        let release = root.path().join("release");
        let cache = root.path().join("cache/target");
        fs::create_dir_all(&release).expect("release dir");

        link_cache(&release, &cache).expect("links");

        let link = release.join("target");
        assert_eq!(fs::read_link(&link).expect("a symlink"), cache);
        assert!(cache.is_dir(), "the cache itself must be created");
    }

    /// fails if a release that ships its own tracked `target/` is treated
    /// as an error. A repository committing that directory is unusual and
    /// its own business; refusing the deploy over it would be this crate
    /// overruling the repository about the repository's own layout.
    #[test]
    fn a_release_that_ships_its_own_target_is_left_alone() {
        let root = tempfile::tempdir().expect("tempdir");
        let release = root.path().join("release");
        fs::create_dir_all(release.join("target")).expect("a committed target");
        let cache = root.path().join("cache/target");

        link_cache(&release, &cache).expect("does nothing, successfully");

        assert!(
            release.join("target").is_dir(),
            "the repository's own directory must survive"
        );
        assert!(
            fs::read_link(release.join("target")).is_err(),
            "and must not have been replaced by a link"
        );
    }

    /// fails if a checkout sharing its own `target` collides with the
    /// dog's cache and produces a bare "File exists". The two are genuinely
    /// different things, the operator's dev artifacts and the dog's shared
    /// cache, and the design says the operator's own target directory must
    /// never be linked. The fix is one line in `.shepignore`, so the error
    /// says so.
    #[test]
    fn a_checkout_sharing_target_says_how_to_fix_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let release = root.path().join("release");
        let checkout = root.path().join("checkout");
        fs::create_dir_all(&release).expect("release");
        fs::create_dir_all(checkout.join("target")).expect("their own target");
        link_cache(&release, &root.path().join("cache/target")).expect("links");

        let err = link_into(&release, &checkout, &[PathBuf::from("target")]).expect_err("collides");
        let shown = err.to_string();
        assert!(shown.contains(".shepignore"), "{shown}");
        assert!(shown.contains("target"), "{shown}");
    }
}
