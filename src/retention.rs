//! Reclaiming release worktrees once they are past the retention count.
//!
//! # Why this runs only after a VERIFIED deploy
//!
//! A process's working directory is resolved when it is spawned, so an
//! instance started before a swap goes on executing from the old release
//! even after `current` moves. Removing that release out from under a
//! running process is the hazard this module has to avoid, and it avoids
//! it by when it runs rather than by checking: verification requires full
//! turnover, every instance under a pid the pre-reload generation never
//! had, so by the time this is called there is no instance left that was
//! spawned from an older RELEASE. Move this call anywhere earlier in the
//! sequence and that argument stops holding.
//!
//! "Retention could delete what something is running from" is the right
//! worry to have about this module, so here is the whole of the answer:
//! this reads `releases/` and nothing else. A sheep's pre-adoption
//! instance runs from the operator's own checkout, which is not in there;
//! `current` is spared explicitly whatever its age; and every other
//! instance was spawned from a release this deploy just turned over. There
//! is no path by which a running process's working directory is a
//! candidate here.
//!
//! # Why a failure here never fails a deploy
//!
//! It runs after `deploy.toml` is written, at which point `current`, the
//! record and the running process all agree and the deploy is genuinely
//! over. A worktree that cannot be removed costs disk, not correctness,
//! and reporting the deploy as failed because of it would report a failure
//! that did not happen.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::error::Error;
use crate::paths::Tree;
use crate::{git, swap};

/// Removes every release beyond the newest `keep`, and answers with the
/// shas it removed.
///
/// Never removes the release `current` names, whatever its age. That is
/// belt and braces rather than the primary guard (the newest release is
/// the one just deployed, so ordering alone would spare it), and it exists
/// because the cost of being wrong is a sheep whose `cwd` resolves to
/// nothing.
///
/// # Errors
/// [`Error::Io`], naming `releases/`, if it cannot be listed.
/// [`Error::Git`] if a worktree cannot be removed or the bookkeeping
/// cannot be pruned. A caller is expected to warn on these rather than
/// fail the deploy that triggered them: see this module's own doc.
pub fn prune(tree: &Tree, keep: usize) -> Result<Vec<String>, Error> {
    let live = swap::resolve(&tree.current())?;
    let live = live.as_deref().and_then(sha_of);

    let mut found = Vec::new();
    let releases = tree.releases();
    let entries = match fs::read_dir(&releases) {
        Ok(entries) => entries,
        // A target with no releases directory has nothing to reclaim,
        // which is every target before its first deploy.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                path: releases,
                source,
            });
        }
    };

    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: releases.clone(),
            source,
        })?;
        let (Some(name), Ok(modified)) = (
            entry.file_name().to_str().map(str::to_owned),
            entry.metadata().and_then(|meta| meta.modified()),
        ) else {
            continue;
        };
        // Only real release directories reach the ordering below. Every
        // release is a worktree keyed by a full git object id, so anything
        // else under `releases/` is not this module's to reason about, and
        // treating it as a release is not harmless: an entry with a fresh
        // mtime takes one of the `keep` slots and pushes a real release into
        // the doomed set. The one it pushes out is the second newest, which
        // is the release a failed deploy rolls back to.
        if !names_a_release(&entry, &name) {
            continue;
        }
        found.push((name, modified));
    }

    let mut removed = Vec::new();
    let mut failure = None;
    for sha in doomed(&found, keep, live.as_deref()) {
        match git::worktree_remove(&tree.git(), &tree.release(&sha)) {
            Ok(()) => removed.push(sha),
            // One release that will not go must not strand the rest, and
            // must not skip the prune below. A `?` here did both, and it
            // did them every cycle: whatever made the removal fail was
            // still there next time, so nothing ordered after it was ever
            // reclaimed and the registration cleanup never ran at all.
            Err(err) => failure = failure.or(Some(err)),
        }
    }

    // Unconditionally, not only when this cycle removed something. `prune`
    // exists for worktrees that vanished by some other means, an operator's
    // own `rm -rf` or a crash mid-build, and gating it on our own removals
    // stopped it running in exactly that case. A hand-removed release then
    // stayed registered, and redeploying that sha failed with "is a missing
    // but already registered worktree".
    git::worktree_prune(&tree.git())?;

    match failure {
        Some(err) => Err(err),
        None => Ok(removed),
    }
}

/// Whether an entry under `releases/` is a release rather than something else.
///
/// A release is a directory named by a full git object id, because that is
/// the only thing [`crate::paths::Tree::release`] ever creates there. A
/// stray file, an editor's backup, a directory an operator made by hand: none
/// of those are releases, and none may occupy one of the `keep` slots.
///
/// `file_type` does not follow symlinks, deliberately. A symlinked entry is
/// not a worktree either.
fn names_a_release(entry: &fs::DirEntry, name: &str) -> bool {
    entry.file_type().is_ok_and(|kind| kind.is_dir())
        && matches!(name.len(), 40 | 64)
        && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Which of `releases` to remove: everything past the newest `keep`, minus
/// `live`.
///
/// Ordered by directory modification time rather than by commit date. A
/// release's directory is created when its worktree is added and written
/// into by its build, so its mtime is when this host last worked on it,
/// which is the question retention is asking. Commit date is a fact about
/// the repository and would put a redeployed older sha in the wrong place.
fn doomed(releases: &[(String, SystemTime)], keep: usize, live: Option<&str>) -> Vec<String> {
    let mut ordered: Vec<&(String, SystemTime)> = releases.iter().collect();
    ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    ordered
        .into_iter()
        .skip(keep)
        .map(|(name, _)| name.clone())
        .filter(|name| Some(name.as_str()) != live)
        .collect()
}

/// The sha a release path names, which is its last component.
fn sha_of(release: &Path) -> Option<String> {
    release
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use crate::fixtures;

    use super::*;
    use std::process::Command;
    use std::time::Duration;

    /// Pins `path`'s modification time, so an ordering test does not depend
    /// on how far apart two filesystem operations happen to land.
    ///
    /// Both tests below did depend on that, and CI caught it: on macOS the
    /// gaps were wide enough, on ubuntu two entries tied and `doomed`'s
    /// tiebreak is the name ascending, which put a `0`-prefixed stand-in in
    /// the KEPT group instead of the doomed one. The test then passed against
    /// the very bug it was written for.
    fn at_second(path: &Path, seconds: u64) {
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        fs::File::open(path)
            .expect("open for set_times")
            .set_times(fs::FileTimes::new().set_modified(when))
            .expect("pin the mtime");
    }

    /// fails if something under `releases/` that is not a release can take
    /// one of the kept slots.
    ///
    /// `prune` read every directory entry's name and mtime and ordered them
    /// together, with no check that an entry was a release at all. A stray
    /// file written more recently than the releases therefore sorted to the
    /// front, held a `keep` slot, and pushed a real release one place down
    /// into the doomed set.
    ///
    /// The one it pushes out is the second newest, which is the release a
    /// failed deploy rolls back to. `retention` refuses a `keep` below 2 to
    /// guarantee that release exists; this deleted it anyway.
    #[test]
    fn a_stray_file_under_releases_cannot_push_a_release_out() {
        let (tree, shas) = fixture_tree_with_releases(3);
        let stray = tree.releases().join("notes.txt");
        fs::write(&stray, "not a release").expect("stray");

        at_second(&tree.release(&shas[0]), 10);
        at_second(&tree.release(&shas[1]), 20);
        at_second(&tree.release(&shas[2]), 30);
        at_second(&stray, 40);

        prune(&tree, 2).expect("prunes");

        assert!(
            tree.release(&shas[1]).is_dir(),
            "the second newest release is the rollback target and must survive"
        );
    }

    /// fails if one release that will not go stops the rest of the cycle.
    ///
    /// The removal loop used a bare `?`, so the first failure returned
    /// immediately: every later doomed release stayed, and the
    /// `worktree_prune` below it never ran, though the comment there says it
    /// must run unconditionally. Nothing self-corrected either, because
    /// whatever made the removal fail was still there next cycle.
    ///
    /// The stand-in is a sha-named directory that was never a worktree, which
    /// is what a kill during `git worktree add` leaves behind and what an
    /// operator copying a release by hand makes. It passes the
    /// is-this-a-release filter, because it is shaped exactly like one, and
    /// `git worktree remove` then refuses it.
    ///
    /// Its mtime is pinned between the kept releases and the doomed ones so
    /// it is removed FIRST. Ordered last it would strand nothing and the test
    /// would pass against the unfixed code.
    #[test]
    fn one_release_that_will_not_go_does_not_strand_the_others() {
        let (tree, shas) = fixture_tree_with_releases(4);
        let never_a_worktree = tree.releases().join("0".repeat(40));
        fs::create_dir(&never_a_worktree).expect("a release-shaped directory");

        at_second(&tree.release(&shas[0]), 10);
        at_second(&tree.release(&shas[1]), 20);
        at_second(&never_a_worktree, 30);
        at_second(&tree.release(&shas[2]), 40);
        at_second(&tree.release(&shas[3]), 50);

        let err = prune(&tree, 2).expect_err("the failure must still be reported");

        assert!(
            !tree.release(&shas[0]).exists() && !tree.release(&shas[1]).exists(),
            "the rest of the cycle must run past the failure: {err}"
        );
    }

    /// Three releases, newest first by the second field.
    fn releases(names: &[&str]) -> Vec<(String, SystemTime)> {
        let base = SystemTime::UNIX_EPOCH;
        names
            .iter()
            .enumerate()
            .map(|(age, name)| {
                (
                    (*name).to_owned(),
                    base + Duration::from_secs(1000 - age as u64),
                )
            })
            .collect()
    }

    /// A real bare repo with `n` commits, a worktree per commit under
    /// `tree.releases()` in commit order, each written into so it is dirty
    /// the way a built release is, with `current` pointed at the newest.
    ///
    /// Returns the shas in commit order, oldest first, so `shas[n - 1]` is
    /// always the one `current` names.
    fn fixture_tree_with_releases(n: u32) -> (Tree, Vec<String>) {
        let origin = tempfile::tempdir().expect("tempdir");
        fixtures::run_git(origin.path(), &["init", "-q", "-b", "main"]);
        fixtures::run_git(origin.path(), &["config", "user.email", "test@example.com"]);
        fixtures::run_git(origin.path(), &["config", "user.name", "test"]);
        for i in 0..n {
            fs::write(origin.path().join(format!("file-{i}.txt")), "x").expect("write");
            fixtures::run_git(origin.path(), &["add", "."]);
            fixtures::run_git(
                origin.path(),
                &["commit", "-q", "-m", &format!("commit {i}")],
            );
        }

        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "web");
        fs::create_dir_all(tree.git()).expect("create git dir");
        fixtures::run_git(&tree.git(), &["init", "-q", "--bare"]);
        let remote = origin.path().to_str().expect("utf-8 path").to_owned();
        git::fetch(&tree.git(), &remote, fixtures::TEST_BUDGET).expect("fetch");

        let log = Command::new("git")
            .current_dir(tree.git())
            .args(["log", "--reverse", "--format=%H", "main"])
            .output()
            .expect("log");
        let shas: Vec<String> = String::from_utf8(log.stdout)
            .expect("utf-8 log")
            .lines()
            .map(str::to_owned)
            .collect();

        for sha in &shas {
            let release = tree.release(sha);
            git::worktree_add(&tree.git(), &release, sha).expect("worktree add");
            fs::write(release.join("built.txt"), "built").expect("dirty the release");
        }
        swap::point_at(
            &tree.current(),
            &tree.release(shas.last().expect("at least one")),
        )
        .expect("point current");

        // Leaked deliberately: `prune` and its assertions run against real
        // paths on disk after this function returns, and an auto-cleaned
        // `TempDir` would delete them out from under the test.
        let _ = origin.keep();
        let _ = home.keep();

        (tree, shas)
    }

    /// fails if retention stops keeping the newest `keep`. Everything else
    /// in this module is a consequence of getting this ordering right, and
    /// getting it backwards would delete the live release and keep the
    /// ancient ones.
    #[test]
    fn the_newest_releases_survive() {
        let all = releases(&["new", "old", "older", "ancient"]);
        assert_eq!(doomed(&all, 2, None), vec!["older", "ancient"]);
    }

    /// fails if `prune` only clears git's stale worktree bookkeeping on
    /// cycles that removed something themselves. `worktree_prune` exists for
    /// the directory that vanished by some other means, an operator's own
    /// `rm -rf` or a crash mid-build, and gating it on our own removals
    /// stopped it running in exactly that case. The sha then stayed
    /// registered, and redeploying it failed with "is a missing but already
    /// registered worktree".
    #[test]
    fn a_release_removed_by_hand_is_deregistered_even_when_nothing_is_pruned() {
        let (tree, shas) = fixture_tree_with_releases(2);
        let orphan = tree.release(&shas[0]);
        fs::remove_dir_all(&orphan).expect("remove the release by hand");

        // keep is larger than the release count, so nothing is past it and
        // `removed` comes back empty. That is the case the gate broke.
        let removed = prune(&tree, 5).expect("prune");
        assert!(removed.is_empty(), "nothing was past the keep count");

        git::worktree_add(&tree.git(), &orphan, &shas[0])
            .expect("the sha redeploys after prune cleared its registration");
        assert!(orphan.is_dir(), "the release is back on disk");
    }

    /// fails if a release still named by `current` can be pruned. Removing
    /// it leaves the sheep with a `cwd` that resolves to nothing, so the
    /// next restart cannot start it at all, and the deploy that caused it
    /// reported success minutes earlier.
    #[test]
    fn the_live_release_is_never_pruned_whatever_its_age() {
        let all = releases(&["new", "old", "older", "ancient"]);
        assert_eq!(doomed(&all, 2, Some("ancient")), vec!["older"]);
        // At keep = 1 the live release is the ONLY survivor besides the
        // newest, which is the "whatever its age" the name promises. Asserting
        // merely that `ancient` is absent restated `doomed`'s unconditional
        // live filter and would have passed at any keep, including a keep this
        // function does not honour.
        assert_eq!(doomed(&all, 1, Some("ancient")), vec!["old", "older"]);
    }

    /// fails if the rollback target is pruned along with the rest. The
    /// second newest release IS what a failed deploy returns to, and
    /// `config` refuses a retention below two for this reason, so this
    /// pins the other end of the same rule.
    #[test]
    fn keeping_two_leaves_a_rollback_target() {
        let all = releases(&["new", "old", "older"]);
        let survivors: Vec<String> = all
            .iter()
            .map(|(name, _)| name.clone())
            .filter(|name| !doomed(&all, 2, None).contains(name))
            .collect();
        assert_eq!(survivors, vec!["new", "old"]);
    }

    /// fails if a tree with fewer releases than the retention count starts
    /// removing things. The first few deploys of every target are this
    /// case.
    #[test]
    fn a_young_tree_loses_nothing() {
        assert!(doomed(&releases(&["new", "old"]), 5, None).is_empty());
        assert!(doomed(&[], 5, None).is_empty());
    }

    /// fails if `prune` stops actually removing directories, or removes
    /// something it should not. The pure function above pins the decision;
    /// this pins that the decision is carried out against real worktrees,
    /// which is where `--force` matters: a built worktree is always dirty.
    #[test]
    fn prune_removes_real_worktrees_and_leaves_the_live_one() {
        let (tree, shas) = fixture_tree_with_releases(4);
        let removed = prune(&tree, 2).expect("prunes");

        assert_eq!(removed.len(), 2);
        assert!(tree.release(&shas[3]).exists(), "the newest survives");
        assert!(
            tree.release(&shas[2]).exists(),
            "so does the rollback target"
        );
        assert!(!tree.release(&shas[0]).exists(), "the oldest is gone");
    }
}
