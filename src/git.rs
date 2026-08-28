//! Fetch, branch resolution and worktree lifecycle, all by shelling out.
//!
//! Every function here launches a real `git` binary rather than linking a
//! git library. Shelling out means this dog inherits the operator's own git
//! auth exactly - if their git can reach a private repo, so can this,
//! and there is no credential handling of our own to get wrong. The trade
//! is a `git` binary on `PATH`, which every host running shep already has.
//! A non-zero exit always becomes [`Error::Git`], carrying the command, the
//! exit status and stderr, because git's own message is the only useful
//! thing when a fetch or a worktree operation fails.
//!
//! [`run_git`] does the launching and the error mapping; it was written for
//! [`crate::shared`]'s own git calls first and is reused here rather than
//! duplicated, since both modules need the exact same shape of error out of
//! the exact same shape of subprocess call.
//!
//! # Two kinds of directory
//!
//! [`remote_url`] and [`current_branch`] run against `checkout`: the
//! operator's own working copy, read once at opt-in to learn what to track.
//! Every other function runs against `git_dir`: the deploy engine's own bare
//! clone under [`crate::paths::Tree::git`], shared by every release's
//! worktree. The two are never the same path, and nothing here writes to
//! `checkout`.
//!
//! # How `fetch` and `remote_head` agree without a stored remote name
//!
//! [`fetch`]'s `remote` argument is a URL - the same string
//! `crate::state::State::remote`'s own doc comment describes ("a git URL an
//! operator already has in their own checkout"), not a short name like
//! `origin`. That rules out `git remote add` plus a plain `git fetch
//! <name>`: this crate never persists a remote configuration into `git_dir`,
//! so there is no stored name for a later call to look up. Every fetch is
//! anonymous instead - `git fetch <url> <refspec>` - which needs no
//! configured remote at all, and it fetches with an explicit mirror refspec,
//! `+refs/heads/*:refs/heads/*`, so every branch the remote has lands
//! straight onto `git_dir`'s own `refs/heads/*`, forced past any
//! non-fast-forward (a rebased or force-pushed branch must still update).
//! [`remote_head`] then only needs a bare branch name - matching
//! `State::branch`'s own shape, "main" and not "origin/main" - because there
//! is no remote-tracking indirection to thread a remote name through.
//! Verified empirically against real git rather than assumed: an anonymous
//! `git fetch <url> '+refs/heads/*:refs/heads/*'` run twice against a bare
//! repository updates `refs/heads/*` in place on the second run exactly as
//! a named remote's default refspec would, with no `git remote add` step
//! and nothing stored in `git_dir`'s config.

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::error::Error;
use crate::shared::{run_git, run_git_within};

/// Converts `path` to `&str` for an argument `git` needs to see, refusing a
/// path this process cannot represent as one rather than lossily mangling
/// it. Every path this crate constructs itself - under `$SHEP_HOME` and the
/// operator's own checkout - is ordinary UTF-8 in practice, so this refusal
/// is not expected to fire; it exists so a filesystem built with unusual
/// bytes in a path fails loudly here instead of silently passing `git` a
/// string that is not the path it was given.
///
/// # Errors
/// [`Error::Io`], naming `path`, if it is not valid UTF-8.
fn path_str(path: &Path) -> Result<&str, Error> {
    path.to_str().ok_or_else(|| Error::Io {
        path: path.to_owned(),
        source: io::Error::other("path is not valid UTF-8"),
    })
}

/// Creates an empty bare repository at `git_dir`, making its parents.
///
/// Empty and then fetched, rather than cloned. [`fetch`] is anonymous by
/// URL with a mirror refspec and `--prune` and needs no configured remote,
/// so an empty repository plus that same fetch reaches exactly the state a
/// clone would, through the one code path the poll loop already runs every
/// thirty seconds instead of a second one that would only ever run once.
///
/// # Errors
/// [`Error::Io`], naming `git_dir`, if it cannot be created or is not
/// valid UTF-8. [`Error::Git`] if `git init` refuses.
pub fn init_bare(git_dir: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(git_dir).map_err(|source| Error::Io {
        path: git_dir.to_owned(),
        source,
    })?;
    run_git(git_dir, &["init", "-q", "--bare"]).map(|_| ())
}

/// The URL configured for `checkout`'s `origin` remote.
///
/// `origin` is the assumption, not a discovered name: every checkout this
/// dog is ever pointed at is expected to have been cloned the ordinary way,
/// which names its single remote `origin` by convention. A checkout with no
/// `origin` at all - or with its one remote named something else - is
/// refused with git's own complaint rather than guessed at.
///
/// # Errors
/// [`Error::Git`] if `checkout` has no `origin` remote, or is not a git
/// repository at all. [`Error::Io`] if `git` itself cannot be launched.
// Opt-in is `crate::optin::prepare`, its real caller as of Task 7.
pub fn remote_url(checkout: &Path) -> Result<String, Error> {
    run_git(checkout, &["remote", "get-url", "origin"]).map(|stdout| stdout.trim().to_owned())
}

/// The branch `checkout`'s `HEAD` currently points at.
///
/// Uses `git symbolic-ref --short HEAD`, never `git rev-parse --abbrev-ref
/// HEAD`. On a detached `HEAD`, `rev-parse --abbrev-ref` returns the literal
/// string `HEAD` - a value that type-checks as a branch name and would send
/// this dog off to track and deploy whatever `HEAD` happens to be, which is
/// not a branch and not what the operator meant. `symbolic-ref` instead
/// fails outright when `HEAD` is detached, which is the correct outcome:
/// there is no branch to track, so refusal is the only honest answer.
///
/// git's own failure for that case - `fatal: ref HEAD is not a symbolic
/// ref` - names the *mechanism* without naming the *problem*; an operator
/// reading it has to already know what a detached `HEAD` is to connect the
/// two. This function recognises that specific message and rewrites the
/// error to say "detached" plainly, while still keeping git's own text
/// inside it. Any other failure (`checkout` is not a git repository at all,
/// say) is left exactly as git reported it - only the one case that is
/// actually about a detached `HEAD` gets the clearer wording, so a real
/// "not a repository" failure is never mislabelled as "detached".
///
/// # Errors
/// [`Error::Git`] if `HEAD` is detached (message names "detached"), or for
/// any other reason `symbolic-ref` fails. [`Error::Io`] if `git` itself
/// cannot be launched.
// Opt-in is `crate::optin::prepare`, its real caller as of Task 7, called
// alongside `remote_url`.
pub fn current_branch(checkout: &Path) -> Result<String, Error> {
    match run_git(checkout, &["symbolic-ref", "--short", "HEAD"]) {
        Ok(stdout) => Ok(stdout.trim().to_owned()),
        Err(Error::Git {
            command,
            status,
            stderr,
        }) if stderr.contains("not a symbolic ref") => Err(Error::Git {
            command,
            status,
            stderr: format!(
                "HEAD is detached, not on a branch - there is nothing to track: {stderr}"
            ),
        }),
        Err(other) => Err(other),
    }
}

/// Fetches every branch `remote` has into `git_dir`, mirroring them onto
/// `git_dir`'s own `refs/heads/*` and dropping any that `remote` no longer
/// has.
///
/// `remote` is a URL, not a configured remote's short name - see the module
/// doc for why `git_dir` never gets a `git remote add` of its own and this
/// fetch is anonymous instead. Run again later against the same `git_dir`,
/// it updates every branch in place, including past a non-fast-forward
/// (rebase, force-push), since the refspec is `+`-forced.
///
/// `--prune` is not optional. A mirror refspec on its own only ever adds or
/// moves refs forward (or sideways, given the `+`); it never removes one
/// `remote` has stopped advertising, so a branch deleted or renamed upstream
/// would otherwise leave `git_dir` holding a `refs/heads/<branch>` frozen at
/// its last-known sha forever. That is the specific failure this project
/// keeps designing against: [`remote_head`] would keep resolving the stale
/// ref without error, the deploy sequence would keep reading "nothing new",
/// and the target would stall permanently with nothing in the loop ever
/// noticing - not a wrong deploy, just silence that looks exactly like
/// "up to date". `--prune` makes the deletion visible instead: once it
/// removes the local ref, [`remote_head`] fails loudly for that branch
/// rather than succeeding on a sha that no longer means anything, which is
/// the right shape of failure for "the branch you're tracking is gone" -
/// an operator needs to be told that, not left to infer it from a target
/// that silently never updates again.
///
/// # Errors
/// [`Error::Git`] if `remote` cannot be reached or refuses the fetch.
/// [`Error::Io`] if `git` itself cannot be launched.
pub fn fetch(git_dir: &Path, remote: &str, budget: Duration) -> Result<(), Error> {
    run_git_within(
        git_dir,
        &["fetch", "--prune", remote, "+refs/heads/*:refs/heads/*"],
        budget,
    )
    .map(|_| ())
}

/// The commit `branch` points at in `git_dir`, after a [`fetch`].
///
/// Resolves `refs/heads/<branch>` specifically, rather than the bare name -
/// see the module doc for why `git_dir`'s mirrored branches live directly
/// under `refs/heads/` rather than behind a `refs/remotes/<name>/`
/// indirection. `--verify` is what turns "no such branch" into a clean,
/// git-reported failure instead of `rev-parse` guessing at some other
/// meaning for the string.
///
/// # Errors
/// [`Error::Git`] if `git_dir` has no branch named `branch` - most likely
/// because [`fetch`] has not run yet, or the remote has no such branch.
/// [`Error::Io`] if `git` itself cannot be launched.
pub fn remote_head(git_dir: &Path, branch: &str) -> Result<String, Error> {
    let refname = format!("refs/heads/{branch}");
    run_git(git_dir, &["rev-parse", "--verify", &refname]).map(|stdout| stdout.trim().to_owned())
}

/// Adds a worktree at `at`, checked out to `sha`.
///
/// `sha` is a commit, not a branch, so the new worktree is always detached -
/// nothing here ever checks out a branch that might already be checked out
/// elsewhere, which `git worktree add` would refuse.
///
/// # Errors
/// [`Error::Git`] if `sha` does not resolve in `git_dir`, or `at` already
/// exists. [`Error::Io`] if `git` itself cannot be launched, or if `at` is
/// not valid UTF-8.
pub fn worktree_add(git_dir: &Path, at: &Path, sha: &str) -> Result<(), Error> {
    let at = path_str(at)?;
    run_git(git_dir, &["worktree", "add", at, sha]).map(|_| ())
}

/// Removes the worktree at `at`, forcibly.
///
/// Always passes `--force`. A worktree this crate built has been written
/// into by a build step, so it is never clean by the time removal is
/// wanted; plain `git worktree remove` refuses any worktree with modified
/// or untracked files, which is every worktree this crate ever removes.
/// Without `--force`, retention would call this function, get refused every
/// time, and silently reclaim no disk at all - the failure would have no
/// caller left to notice it, since retention runs unattended.
///
/// # Errors
/// [`Error::Git`] if `at` is not a worktree of `git_dir`. [`Error::Io`] if
/// `git` itself cannot be launched, or if `at` is not valid UTF-8.
pub fn worktree_remove(git_dir: &Path, at: &Path) -> Result<(), Error> {
    let at = path_str(at)?;
    run_git(git_dir, &["worktree", "remove", "--force", at]).map(|_| ())
}

/// Cleans up `git_dir`'s worktree bookkeeping for any worktree whose
/// directory is gone.
///
/// [`worktree_remove`] already deletes both the directory and the
/// bookkeeping together, so this exists for the case that removal does not
/// cover: a worktree directory that vanished by some other means (an
/// operator's own `rm -rf`, a crash mid-build) and left `git_dir` still
/// listing it.
///
/// # Errors
/// [`Error::Io`] if `git` itself cannot be launched. `git worktree prune`
/// has no non-zero exit path for an ordinary bare repository, so
/// [`Error::Git`] is not expected in practice, but is still possible if
/// `git_dir` is not a valid repository at all.
pub fn worktree_prune(git_dir: &Path) -> Result<(), Error> {
    run_git(git_dir, &["worktree", "prune"]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use crate::fixtures;

    /// A budget the test tier can never legitimately hit.
    ///
    /// These fetches are between two local directories, so anything slower
    /// than this is a hang worth failing on rather than waiting out.
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    /// A throwaway non-bare repo with `commits` commits on its initial
    /// branch, named `main` explicitly - `init.defaultBranch` is a user
    /// setting this test cannot assume, so every fixture pins the name
    /// itself rather than trusting whatever a host happens to default to.
    fn fixture_repo_with_commits(commits: u32) -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fixtures::run_git(dir.path(), &["init", "-q", "-b", "main"]);
        fixtures::run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        fixtures::run_git(dir.path(), &["config", "user.name", "test"]);

        for n in 0..commits {
            fs::write(dir.path().join(format!("file-{n}.txt")), "x").expect("write fixture file");
            fixtures::run_git(dir.path(), &["add", "."]);
            fixtures::run_git(dir.path(), &["commit", "-q", "-m", &format!("commit {n}")]);
        }

        dir
    }

    /// Detaches `repo`'s `HEAD` from its branch, leaving it pointed straight
    /// at a commit instead.
    fn detach_head(repo: &TempDir) {
        fixtures::run_git(repo.path(), &["checkout", "-q", "--detach", "HEAD"]);
    }

    /// A bare repository with no branches yet, standing in for
    /// `Tree::git()`'s bare clone.
    fn bare_git_dir() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fixtures::run_git(dir.path(), &["init", "-q", "--bare"]);
        dir
    }

    /// fails if a detached HEAD is treated as a branch. There is no branch to
    /// track, so the dog must refuse with a message naming the problem rather
    /// than deploy something arbitrary.
    #[test]
    fn a_detached_head_is_refused_by_name() {
        let repo = fixture_repo_with_commits(2);
        detach_head(&repo);
        let err = current_branch(repo.path()).expect_err("refuses");
        assert!(err.to_string().to_lowercase().contains("detached"));
    }

    /// fails if a checkout that is not a git repository at all also gets
    /// labelled "detached". Both failures exit non-zero and both come out
    /// of the exact same command, so a guard that fires on any failure
    /// rather than specifically on a detached HEAD would pass the test
    /// above for the wrong reason and mislabel this one.
    #[test]
    fn a_non_repository_failure_is_not_mislabelled_as_detached() {
        let dir = tempfile::tempdir().expect("tempdir, deliberately never `git init`-ed");
        let err = current_branch(dir.path()).expect_err("refuses");
        assert!(!err.to_string().to_lowercase().contains("detached"));
    }

    /// fails if `current_branch` stops naming the branch actually checked
    /// out, or starts trimming incorrectly.
    #[test]
    fn current_branch_names_the_checked_out_branch() {
        let repo = fixture_repo_with_commits(1);
        assert_eq!(current_branch(repo.path()).expect("on a branch"), "main");
    }

    /// fails if `remote_url` stops reading `origin` specifically, or starts
    /// mangling the URL it finds.
    #[test]
    fn remote_url_reads_the_origin_remote() {
        let repo = fixture_repo_with_commits(1);
        fixtures::run_git(
            repo.path(),
            &["remote", "add", "origin", "https://example.com/x.git"],
        );
        assert_eq!(
            remote_url(repo.path()).expect("origin is configured"),
            "https://example.com/x.git"
        );
    }

    /// fails if a checkout with no `origin` remote is silently accepted
    /// rather than refused with git's own complaint.
    #[test]
    fn remote_url_refuses_a_checkout_with_no_origin() {
        let repo = fixture_repo_with_commits(1);
        let err = remote_url(repo.path()).expect_err("no origin configured");
        assert!(matches!(err, Error::Git { .. }));
    }

    /// fails if `fetch` stops mirroring the remote's branches onto
    /// `git_dir`'s own `refs/heads/*`, which is what lets [`remote_head`]
    /// resolve a bare branch name with no remote name in play at all.
    #[test]
    fn fetch_mirrors_the_remotes_branches_onto_refs_heads() {
        let origin = fixture_repo_with_commits(1);
        let git_dir = bare_git_dir();

        fetch(
            git_dir.path(),
            origin.path().to_str().expect("utf-8 path"),
            fixtures::TEST_BUDGET,
        )
        .expect("fetches");

        let expected = origin.path();
        let sha = String::from_utf8(
            Command::new("git")
                .current_dir(expected)
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("rev-parse origin HEAD")
                .stdout,
        )
        .expect("utf-8 sha");
        assert_eq!(
            remote_head(git_dir.path(), "main").expect("resolves"),
            sha.trim()
        );
    }

    /// fails if a second `fetch` stops updating an already-mirrored branch -
    /// the poll loop this module supports calls `fetch` over and over
    /// against the same `git_dir`, and the first fetch succeeding proves
    /// nothing about whether the second one actually moves the ref forward
    /// rather than leaving it stuck at the first commit it ever saw.
    #[test]
    fn a_second_fetch_moves_an_already_mirrored_branch_forward() {
        let origin = fixture_repo_with_commits(1);
        let git_dir = bare_git_dir();
        let url = origin.path().to_str().expect("utf-8 path");

        fetch(git_dir.path(), url, fixtures::TEST_BUDGET).expect("first fetch");
        let first = remote_head(git_dir.path(), "main").expect("resolves after first fetch");

        fs::write(origin.path().join("second.txt"), "y").expect("write second commit's file");
        fixtures::run_git(origin.path(), &["add", "."]);
        fixtures::run_git(origin.path(), &["commit", "-q", "-m", "second"]);

        fetch(git_dir.path(), url, fixtures::TEST_BUDGET).expect("second fetch");
        let second = remote_head(git_dir.path(), "main").expect("resolves after second fetch");

        assert_ne!(first, second);
    }

    /// fails if a branch deleted upstream keeps resolving to its
    /// last-known sha instead of disappearing. This is the textbook failure
    /// of an unpruned mirror fetch: without `--prune`, `git_dir` would keep
    /// `refs/heads/feature` around forever once it existed, `remote_head`
    /// would keep succeeding on a sha the remote no longer has any record
    /// of, and a poll loop tracking `feature` would read "up to date"
    /// permanently - the exact silent-stall failure `--prune` exists to
    /// turn into a loud one.
    #[test]
    fn a_branch_deleted_upstream_stops_resolving_after_a_pruning_fetch() {
        let origin = fixture_repo_with_commits(1);
        fixtures::run_git(origin.path(), &["branch", "feature"]);
        let git_dir = bare_git_dir();
        let url = origin.path().to_str().expect("utf-8 path");

        fetch(git_dir.path(), url, fixtures::TEST_BUDGET).expect("first fetch sees feature");
        remote_head(git_dir.path(), "feature").expect("feature resolves before deletion");

        fixtures::run_git(origin.path(), &["branch", "-D", "feature"]);
        fetch(git_dir.path(), url, fixtures::TEST_BUDGET).expect("second fetch prunes feature");

        let err =
            remote_head(git_dir.path(), "feature").expect_err("a deleted branch must not resolve");
        assert!(matches!(err, Error::Git { .. }));
    }

    /// fails if `remote_head` stops refusing a branch `git_dir` has never
    /// heard of, instead of `rev-parse` resolving the string as something
    /// else entirely.
    #[test]
    fn remote_head_refuses_an_unknown_branch() {
        let origin = fixture_repo_with_commits(1);
        let git_dir = bare_git_dir();
        fetch(
            git_dir.path(),
            origin.path().to_str().expect("utf-8 path"),
            fixtures::TEST_BUDGET,
        )
        .expect("fetches");

        let err = remote_head(git_dir.path(), "no-such-branch").expect_err("no such branch");
        assert!(matches!(err, Error::Git { .. }));
    }

    /// fails if `worktree_add` stops actually checking `sha` out at `at`, or
    /// starts leaving the worktree on a branch instead of detached.
    #[test]
    fn worktree_add_checks_out_the_given_sha() {
        let repo = fixture_repo_with_commits(1);
        let at = repo.path().join("wt-checkout");
        let sha = String::from_utf8(
            Command::new("git")
                .current_dir(repo.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("rev-parse HEAD")
                .stdout,
        )
        .expect("utf-8 sha");

        worktree_add(repo.path(), &at, sha.trim()).expect("adds");
        assert!(at.join("file-0.txt").exists());

        // `at` lives inside `repo`'s own tempdir, so `TempDir`'s drop would
        // eventually reclaim the directory either way; removing it here
        // through the real function keeps this test from also leaving a
        // stale worktree registration behind in `repo/.git/worktrees` for
        // whichever test runs against the same fixture next.
        worktree_remove(repo.path(), &at).expect("cleans up");
    }

    /// fails if worktree removal stops forcing. A built worktree is ALWAYS
    /// dirty, so plain `git worktree remove` always refuses and retention
    /// would silently never reclaim anything.
    #[test]
    fn worktree_removal_forces_because_built_trees_are_dirty() {
        let repo = fixture_repo_with_commits(1);
        let at = repo.path().join("rel-abc");
        worktree_add(repo.path(), &at, "HEAD").expect("adds");
        std::fs::write(at.join("build-output.txt"), "x").expect("writes");
        worktree_remove(repo.path(), &at).expect("removes a dirty tree");
        assert!(!at.exists());
    }

    /// fails if `worktree_prune` stops clearing bookkeeping for a worktree
    /// whose directory is already gone by some means other than
    /// `worktree_remove` - the one case `worktree_remove` itself cannot
    /// cover, since it deletes the directory it is told about, not one that
    /// vanished on its own.
    #[test]
    fn worktree_prune_clears_a_worktree_whose_directory_is_already_gone() {
        let repo = fixture_repo_with_commits(1);
        let at = repo.path().join("wt-vanished");
        worktree_add(repo.path(), &at, "HEAD").expect("adds");
        fs::remove_dir_all(&at).expect("simulate the directory vanishing on its own");

        let before = run_git(repo.path(), &["worktree", "list"]).expect("lists");
        assert!(before.contains("wt-vanished"));

        worktree_prune(repo.path()).expect("prunes");

        let after = run_git(repo.path(), &["worktree", "list"]).expect("lists");
        assert!(!after.contains("wt-vanished"));
    }
}
