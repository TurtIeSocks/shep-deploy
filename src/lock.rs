//! One deploy at a time, per sheep.
//!
//! # Why this exists
//!
//! The poll loop guarantees a tick never overlaps another tick, and that is
//! all it guarantees, because it is one `for` loop in one process. Nothing
//! stopped a second PROCESS working on the same tree, and README.md tells an
//! operator to start one: `shep-deploy deploy <sheep>` is the documented way
//! to retry a held commit, while the dog is polling the whole time.
//!
//! Round 10 of the founder's review ran both at once and reproduced two
//! collisions with real processes. `git worktree add` lost to git's own index
//! lock (`fatal: Unable to create ... index.lock: File exists`), and `git
//! fetch` refused with `cannot lock ref 'refs/heads/main': is at <new> but
//! expected <old>`. Git refusing is the good half: neither process wrote a
//! half-finished checkout. The bad half is that the operator saw raw git
//! plumbing with nothing saying a second shep-deploy had caused it, and the
//! loser then recorded a failure for a sha the winner had just deployed.
//!
//! # Why `flock`
//!
//! The kernel releases it when the holder dies. That matters more here than
//! it usually would: round 9 of the same review was entirely about state a
//! killed process leaves behind, and a lock file that outlived its holder
//! would strand every later deploy of that sheep behind a lock nobody holds.
//! Trading a rare race for a routine outage is the wrong direction.
//!
//! `nix` is already in this crate's tree, as a dependency of `shep-core`, so
//! this is a direct edge on something every build already compiles.
//!
//! # The second lock
//!
//! [`hold_record`] is the other one, on `deploy.toml.lock`, and it guards
//! something the tree lock cannot: the record. `set_watch` writes the record
//! without the tree lock on purpose, because a deploy holds that lock for a
//! whole build and `--watch manual` is what an operator types during an
//! incident. Without a lock of its own, a deploy and a `set_watch` could
//! each read the record, each rename their own snapshot over it, and the
//! later rename would drop the earlier update: a `deployed` that no longer
//! matched `current`, or a `manual` that never landed. So every
//! read-modify-write of the record takes this one, and holds it for exactly
//! that long. Blocking, unlike the tree lock, because a holder keeps it for
//! milliseconds and the caller wants the write to land, not a refusal. A
//! deploy takes the tree lock first and this one inside it; `set_watch`
//! takes only this one; nothing holding this one ever waits for the tree's.

use std::fs::{File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

use crate::error::Error;
use crate::paths::Tree;

/// The lock file's mode: owner only.
///
/// `flock(2)` needs nothing more than an open descriptor, so a lock file
/// readable by other accounts is one any local user can take out from under
/// the dog. Left at the default `0o644`, an unprivileged process could hold
/// it forever and every deploy of that sheep would report
/// [`Error::AlreadyDeploying`] against a rival that is not a deploy at all.
const LOCK_MODE: u32 = 0o600;

/// An exclusive hold on one sheep's deploy tree.
///
/// Released when this is dropped, and by the kernel if the process holding it
/// dies. Keep it alive for as long as the tree is being written to: binding it
/// to `_` drops it immediately and locks nothing.
#[derive(Debug)]
pub struct Deploying {
    /// Held for its `Drop`. The lock is the point, not the handle.
    _held: Flock<File>,
}

/// Takes the exclusive hold, or reports who has it.
///
/// Non-blocking on purpose. A deploy that queued behind another would run
/// against a tree that changed underneath the reasons it was started for: the
/// poll loop's next tick will pick up anything still outstanding, and an
/// operator asking by hand would rather be told than wait.
///
/// # Errors
/// [`Error::AlreadyDeploying`] if another process holds it. [`Error::Io`] if
/// the lock file cannot be created, naming it.
pub fn hold(tree: &Tree) -> Result<Deploying, Error> {
    let path = tree.lock_file();
    let file = open_lock_file(&path)?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(held) => Ok(Deploying { _held: held }),
        Err((_, errno)) => Err(refusal(tree.sheep(), path, errno)),
    }
}

/// An exclusive hold on one sheep's record, for one read-modify-write.
///
/// Released when dropped, and by the kernel if the holder dies. See the
/// module doc for why it is separate from [`Deploying`].
#[derive(Debug)]
pub struct Recording {
    /// Held for its `Drop`.
    _held: Flock<File>,
}

/// Takes the record lock, waiting for whoever has it.
///
/// Blocking, because every holder is inside one read and one write of a
/// file a few hundred bytes long, and the caller wants its write to land
/// rather than to be told to try again.
///
/// # Errors
/// [`Error::Io`], naming the lock file, if it cannot be created or the
/// lock cannot be taken. Never [`Error::AlreadyDeploying`]: contention here
/// is waited out, not reported.
pub fn hold_record(tree: &Tree) -> Result<Recording, Error> {
    let path = tree.record_lock();
    let file = open_lock_file(&path)?;
    match Flock::lock(file, FlockArg::LockExclusive) {
        Ok(held) => Ok(Recording { _held: held }),
        Err((_, errno)) => Err(Error::Io {
            path,
            source: io::Error::from_raw_os_error(errno as i32),
        }),
    }
}

/// Opens (creating if need be) the lock file at `path`, owner-only, never
/// through a link.
fn open_lock_file(path: &std::path::Path) -> Result<File, Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::at(parent))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(LOCK_MODE)
        // Never through a link. The tree is this dog's own, so nothing else
        // should have put one here; if something has, the lock would land on
        // whatever it points at, and that is not this tree.
        .custom_flags(crate::shared::o_nofollow())
        .open(path)
        .map_err(Error::at(path))?;
    // `truncate(false)` above, deliberately. The file's contents are never
    // read and never written, so truncating would be a write to a file another
    // process is at that moment holding a lock on, for no reason.
    //
    // `mode` only applies when the file is created, so a lock file left at the
    // default mode by an earlier version is tightened here as well. Best
    // effort: a file this process cannot chmod is one it does not own, and
    // the lock still works on it.
    let _ = file.set_permissions(Permissions::from_mode(LOCK_MODE));
    Ok(file)
}

/// Why the lock could not be taken, told apart by the errno.
///
/// Only `EWOULDBLOCK` means somebody else holds it, and it is the only one
/// `LockExclusiveNonblock` returns for contention. Reporting every failure as
/// contention is the false refusal this whole module is supposed to prevent:
/// `ENOLCK` from an exhausted lock table, `EIO`, or a mount that does not
/// implement `flock` would each stop the dog deploying that sheep and send the
/// operator looking for a second process that does not exist, with the one
/// piece of information that would have told them otherwise discarded.
///
/// Found by round 12 of the founder's review, in code written the same day.
///
/// `EAGAIN` is not matched separately: `nix` gives it and `EWOULDBLOCK` the
/// same discriminant, so naming both is an unreachable arm rather than the
/// belt-and-braces it looks like. Clippy said so.
fn refusal(sheep: &str, path: PathBuf, errno: Errno) -> Error {
    match errno {
        Errno::EWOULDBLOCK => Error::AlreadyDeploying {
            sheep: sheep.to_owned(),
        },
        other => Error::Io {
            path,
            source: io::Error::from_raw_os_error(other as i32),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if the record lock stops being exclusive, or stops waiting.
    /// A holder keeps it for one read and one write; a second taker has
    /// to wait that out rather than be refused, and then get it.
    #[test]
    fn the_record_lock_waits_for_the_holder_and_then_takes_over() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "web");
        let held = hold_record(&tree).expect("first hold");

        let (tx, rx) = std::sync::mpsc::channel();
        let waiting = {
            let tree = Tree::for_sheep(home.path(), "web");
            std::thread::spawn(move || {
                let second = hold_record(&tree).expect("second hold");
                tx.send(()).expect("report");
                drop(second);
            })
        };
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "the second taker must wait while the first holds"
        );

        drop(held);
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the second taker gets the lock once the first lets go");
        waiting.join().expect("thread");
    }
    use crate::paths::Tree;

    /// fails if two holds on one tree can be taken at once.
    #[test]
    fn a_second_hold_on_the_same_tree_is_refused() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "web");

        let first = hold(&tree).expect("the first hold");
        let err = hold(&tree).expect_err("the second must be refused");

        assert!(
            matches!(&err, Error::AlreadyDeploying { sheep } if sheep == "web"),
            "must name the sheep: {err:?}"
        );
        drop(first);
    }

    /// fails if a hold outlives the guard that took it.
    ///
    /// This is what makes a killed dog recoverable rather than fatal: the same
    /// release happens in the kernel when the holder dies, so a lock file is
    /// never left behind holding a sheep hostage.
    #[test]
    fn a_dropped_hold_frees_the_tree() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "web");

        drop(hold(&tree).expect("the first hold"));

        hold(&tree).expect("the tree must be free once the first is dropped");
    }

    /// fails if a lock failure that is NOT contention is reported as
    /// contention.
    ///
    /// `hold` mapped every `flock` errno to "another deploy is already
    /// running". Only `EWOULDBLOCK` means that. `ENOLCK` from an exhausted
    /// kernel lock table, `EIO`, or a mount with no `flock` support would each
    /// stop the dog deploying that sheep and send the operator hunting a
    /// second process that does not exist, with the errno that would have told
    /// them otherwise thrown away. That is the false refusal this module
    /// exists to avoid, produced by the module itself.
    #[test]
    fn a_lock_failure_that_is_not_contention_says_what_it_was() {
        let path = std::path::PathBuf::from("/x/deploy.lock");

        let contended = refusal("web", path.clone(), Errno::EWOULDBLOCK);
        assert!(
            matches!(&contended, Error::AlreadyDeploying { sheep } if sheep == "web"),
            "contention is the one case that claim is true for: {contended:?}"
        );

        for errno in [Errno::ENOLCK, Errno::EIO, Errno::EOPNOTSUPP] {
            let err = refusal("web", path.clone(), errno);
            assert!(
                matches!(&err, Error::Io { path: named, .. } if named == &path),
                "{errno:?} must name the lock file, not a rival process: {err:?}"
            );
            assert!(
                !format!("{err}").contains("already running"),
                "{errno:?} must not claim contention: {err}"
            );
        }
    }

    /// fails if the lock file can be opened by another account.
    ///
    /// `flock` needs only an open descriptor, so a file at the default mode
    /// lets any local user hold the lock and turn every deploy of that sheep
    /// into a false `AlreadyDeploying`, indefinitely. Both paths are pinned:
    /// a file this call creates, and one left wide open by an earlier version.
    #[test]
    fn the_lock_file_is_owner_only() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "web");
        let mode = |path: &std::path::Path| {
            std::fs::metadata(path)
                .expect("lock file")
                .permissions()
                .mode()
                & 0o777
        };

        drop(hold(&tree).expect("creates the lock file"));
        assert_eq!(mode(&tree.lock_file()), LOCK_MODE, "as created");

        std::fs::set_permissions(tree.lock_file(), Permissions::from_mode(0o644))
            .expect("loosen, as an older version left it");
        drop(hold(&tree).expect("reopens the lock file"));
        assert_eq!(mode(&tree.lock_file()), LOCK_MODE, "tightened on reopen");
    }

    /// fails if two sheep contend with each other. The lock is per tree, and
    /// the poll loop deploys every target in turn under one process.
    #[test]
    fn two_sheep_do_not_contend() {
        let home = tempfile::tempdir().expect("tempdir");
        let web = hold(&Tree::for_sheep(home.path(), "web")).expect("web");
        let worker = hold(&Tree::for_sheep(home.path(), "worker")).expect("worker");
        drop((web, worker));
    }
}
