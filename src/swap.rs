//! Repointing the `current` symlink at a fresh release.
//!
//! `current` is the path every running sheep's `cwd` points through - see
//! [`crate::paths::Tree::current`]. Swapping a release means repointing that
//! one symlink, and the mechanism is deliberately narrow: `rename(2)` a
//! freshly-created link at a temporary name onto `current`, never
//! `remove_file` followed by `symlink`. The two-step version leaves a
//! window where `current` points at nothing at all, and a sheep restarting
//! inside that window fails to start for a reason nobody will reproduce -
//! the failure is real but the repro is not, since the window is a handful
//! of syscalls wide. `rename(2)` onto an existing name is a single
//! directory-entry update on every filesystem this dog runs on, so there is
//! no instant where `current` is absent.
//!
//! Unlike [`crate::shared::link_into`], `point_at` does not canonicalise its
//! target before linking. `link_into` has to, because its targets are
//! operator-supplied and can arrive relative; a relative target would
//! resolve against the symlink's own directory rather than against
//! whatever the caller meant, and dangle silently. `point_at`'s targets are
//! always [`crate::paths::Tree::release`] paths, which are built by joining
//! onto `$SHEP_HOME` and are therefore already absolute in every caller this
//! crate has - there is no relative-path case here to guard against, and
//! canonicalising would also resolve through any symlinks in `$SHEP_HOME`
//! itself, which is not this function's business.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// `current` with `.tmp` appended to its file name, alongside it in the same
/// directory.
///
/// Same directory matters: `rename(2)` is only atomic within one filesystem,
/// and a sibling of `current` is guaranteed to share its filesystem, which a
/// path built any other way would not be.
fn tmp_path(current: &Path) -> PathBuf {
    debug_assert!(
        current.file_name().is_some(),
        "current is always Tree::current, which always ends in a file name"
    );
    let mut name = current.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    current.with_file_name(name)
}

/// Repoints `current` at `release`, replacing whatever `current` pointed at
/// before - including nothing, the first time a sheep deploys - in a single
/// `rename(2)`.
///
/// Creates the new link at `current`'s temporary sibling first, then renames
/// that sibling onto `current`. Never removes `current` and creates it
/// fresh: see the module doc for why that shape leaves a dangling window
/// this one does not.
///
/// A leftover `current.tmp` symlink from a swap that crashed after creating
/// it but before the rename is removed first, with no message: `lock.rs`
/// serialises deploys per tree, so nothing but an earlier crashed swap of
/// this same tree could have put a symlink there, and it is not worth
/// refusing over. If the rename below fails, the symlink this call just
/// created is removed again on a best-effort basis, so one failed swap does
/// not wedge every later one behind a leftover it caused.
///
/// # Errors
/// [`Error::Config`], naming the temporary sibling, if something other than
/// a leftover symlink already occupies it - this dog did not leave it and
/// will not remove it on its own. [`Error::Io`], naming the temporary
/// sibling, if it cannot be inspected, if a leftover symlink there cannot be
/// removed, or if the new symlink cannot be created. [`Error::Io`], naming
/// `current`, if the rename onto it fails.
pub fn point_at(current: &Path, release: &Path) -> Result<(), Error> {
    let tmp = tmp_path(current);

    match fs::symlink_metadata(&tmp) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::remove_file(&tmp).map_err(Error::at(&tmp))?;
        }
        Ok(_) => {
            return Err(Error::Config(format!(
                "{} is in the way of this swap and is not a symlink, so this dog did not leave \
                 it. Move it aside by hand - `rm {}` if it is not wanted.",
                tmp.display(),
                tmp.display()
            )));
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(Error::Io {
                path: tmp.clone(),
                source,
            });
        }
    }

    symlink(release, &tmp).map_err(Error::at(&tmp))?;

    fs::rename(&tmp, current).map_err(|source| {
        let _ = fs::remove_file(&tmp);
        Error::Io {
            path: current.to_owned(),
            source,
        }
    })
}

/// Where `current` points right now, or `None` if it does not exist yet -
/// the zero-configuration case for a sheep that has never deployed.
///
/// Returns the symlink's target text exactly as `point_at` wrote it, with no
/// resolution of its own - the same convention [`crate::shared::link_into`]
/// documents for its own symlinks.
///
/// # Errors
/// [`Error::Io`], naming `current`, if it exists but cannot be read as a
/// symlink - permission denied, or something other than a symlink already
/// occupying that name.
pub fn resolve(current: &Path) -> Result<Option<PathBuf>, Error> {
    match fs::read_link(current) {
        Ok(target) => Ok(Some(target)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            path: current.to_owned(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    /// fails if a leftover `current.tmp` that is not a symlink stops being a
    /// refusal. A `current.tmp` that exists when `point_at` runs can only be
    /// a leftover - `lock.rs` serialises deploys per tree - but one that is
    /// not a symlink is not something this dog left behind, so it must be
    /// refused rather than silently removed: an operator's own file could be
    /// sitting there for a reason this dog cannot know.
    ///
    /// Its doc names this failure mode; until now the only test in this module
    /// exercised the success path, and the refusal was reached only
    /// transitively through `deploy`'s rollback tests and their fake daemon.
    /// A resequencing of the rollback would have dropped the coverage
    /// silently.
    #[test]
    fn a_leftover_tmp_path_refuses_the_swap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let release = dir.path().join("releases/abc");
        std::fs::create_dir_all(&release).expect("release");
        let current = dir.path().join("current");
        std::fs::write(dir.path().join("current.tmp"), b"in the way").expect("blocker");

        let err = point_at(&current, &release).expect_err("an occupied tmp path must refuse");
        assert!(
            matches!(&err, Error::Config(message)
                if message.contains("current.tmp") && message.contains("rm")),
            "the refusal must name the path in the way and say `rm`, got: {err}"
        );
        assert!(
            !current.exists(),
            "a refused swap must leave `current` exactly as it found it"
        );
    }

    /// fails if a leftover `current.tmp` SYMLINK from a crashed swap stops
    /// being self-healed. `lock.rs` serialises deploys per tree, so a
    /// symlink already at the temporary path can only be this dog's own
    /// leftover from an earlier swap that created it and then crashed
    /// before the rename - nothing else could have put a symlink there
    /// while a later swap of the same tree runs.
    #[test]
    fn a_leftover_tmp_symlink_is_replaced_and_the_swap_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old_release = dir.path().join("releases/old");
        let release = dir.path().join("releases/new");
        std::fs::create_dir_all(&old_release).expect("old release");
        std::fs::create_dir_all(&release).expect("release");
        let current = dir.path().join("current");
        let tmp = dir.path().join("current.tmp");
        symlink(&old_release, &tmp).expect("leftover symlink");

        point_at(&current, &release).expect("a leftover symlink must self-heal");
        assert_eq!(
            resolve(&current).unwrap().as_deref(),
            Some(release.as_path())
        );
        assert!(!tmp.exists(), "the swap must not leave current.tmp behind");
    }

    /// fails if a rename that fails leaves `current.tmp` behind to wedge
    /// every later swap. `current` here is a non-empty real directory,
    /// which `rename(2)` refuses to replace with a symlink - the same
    /// failure shape a permissions problem or a full filesystem produces on
    /// the real path. Without the cleanup this exercises, the very first
    /// failed swap of a tree would leave a `current.tmp` that every later
    /// deploy of the same sheep then refuses to run at all.
    #[test]
    fn a_failed_rename_leaves_no_tmp_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let release = dir.path().join("releases/abc");
        std::fs::create_dir_all(&release).expect("release");
        let current = dir.path().join("current");
        std::fs::create_dir_all(&current).expect("current as a directory");
        std::fs::write(current.join("keep"), b"not empty").expect("make it non-empty");

        let err = point_at(&current, &release)
            .expect_err("renaming a symlink onto a non-empty directory must fail");
        assert!(
            matches!(&err, Error::Io { path, .. } if path == &current),
            "the failure must name current, got: {err}"
        );
        let tmp = dir.path().join("current.tmp");
        assert!(
            !tmp.exists(),
            "a failed rename must not leave current.tmp behind"
        );
    }

    use super::*;
    use tempfile::tempdir;

    /// The name says "never leaves current dangling", which is the property
    /// this whole module exists for - but a single-threaded test cannot
    /// observe that property directly. Observing a dangling window needs a
    /// second thread positioned to read `current` in the instant between
    /// removing the old link and creating the new one, and there is no such
    /// thread here. What this test actually pins is everything that IS
    /// observable from one thread: the swap succeeds over an
    /// already-existing link, `current` resolves to the new release
    /// afterwards, and no `current.tmp` survives a successful swap. Replace
    /// `point_at`'s `rename(2)` with remove-then-create and every assertion
    /// below still passes. That was verified by mutation during review, not
    /// assumed, and it is exactly the point: none of these assertions were
    /// ever the atomicity guarantee. That guarantee rests on
    /// `point_at`'s implementation shape (rename over a temporary link,
    /// never remove-then-create) and on review, not on this assertion list.
    #[test]
    fn the_swap_never_leaves_current_dangling() {
        let root = tempdir().unwrap();
        let (a, b) = (root.path().join("a"), root.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let current = root.path().join("current");

        point_at(&current, &a).expect("first");
        assert_eq!(resolve(&current).unwrap().as_deref(), Some(a.as_path()));

        point_at(&current, &b).expect("swap over an existing link");
        assert_eq!(resolve(&current).unwrap().as_deref(), Some(b.as_path()));

        let tmp = root.path().join("current.tmp");
        assert!(
            !tmp.exists(),
            "a successful swap must not leave current.tmp behind"
        );
    }

    /// fails if `resolve` reports a release for a `current` that has never
    /// been pointed anywhere. `None` is the zero-configuration case: a
    /// sheep that has not deployed yet has no `current` on disk at all.
    #[test]
    fn resolve_of_a_missing_current_is_none() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        assert_eq!(resolve(&current).unwrap(), None);
    }

    /// fails if `resolve` collapses "exists but is not a symlink" into the
    /// same `None` as "does not exist at all" - the reject side of the same
    /// `NotFound` match `resolve_of_a_missing_current_is_none` exercises the
    /// accept side of. The two cases mean different things to a caller: one
    /// is "no deploy yet", the other is something has clobbered `current`
    /// and the mistake needs to surface loudly, not read as a fresh sheep.
    #[test]
    fn resolve_of_a_plain_file_is_an_error() {
        let root = tempdir().unwrap();
        let current = root.path().join("current");
        std::fs::write(&current, b"not a symlink").unwrap();

        let err = resolve(&current).expect_err("a plain file is not a symlink to read");
        assert!(matches!(err, Error::Io { .. }));
    }

    /// fails if `point_at` stops working the first time a sheep deploys,
    /// when `current` does not exist yet at all. This is the degenerate
    /// case of "replacing whatever current pointed at before" - there is
    /// nothing to replace, and `rename(2)` onto a name that does not yet
    /// exist is still a single atomic directory-entry creation, not a
    /// special case `point_at` needs to branch on.
    #[test]
    fn point_at_works_the_first_time_current_does_not_exist() {
        let root = tempdir().unwrap();
        let release = root.path().join("release");
        std::fs::create_dir_all(&release).unwrap();
        let current = root.path().join("current");

        point_at(&current, &release).expect("first deploy, no prior current");
        assert_eq!(
            resolve(&current).unwrap().as_deref(),
            Some(release.as_path())
        );
    }
}
