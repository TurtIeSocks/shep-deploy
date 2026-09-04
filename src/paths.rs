//! The on-disk layout under `$SHEP_HOME/deploy/<sheep>/`.
//!
//! [`Tree`] is the one place that knows this layout. Every other module asks
//! it for a path rather than joining strings itself, so the layout can only
//! drift in one place if it ever needs to.
//!
//! ```text
//! $SHEP_HOME/deploy/<sheep>/
//! ├── git/                 one bare clone; object store shared by every
//! │                        worktree release
//! ├── releases/<sha>/      a git worktree per built release
//! ├── cache/target/        the dog's build cache, symlinked into every
//! │                        release as `target` so builds stay warm and a
//! │                        hardcoded `./target/release/x` still resolves
//! ├── current -> releases/<sha>   swapped with rename(2) at cutover
//! ├── complete/<sha>       per-release completion marker, kept outside the
//! │                        release on purpose - see `Tree::completion`
//! ├── deploy.lock          exclusive flock so only one deploy runs at once
//! └── deploy.toml          the sheep's `State` - see `crate::state`
//! ```

use std::path::{Component, Path, PathBuf};

use crate::error::Error;

/// Every sheep that is a deploy target, by name.
///
/// A target is a directory under `<shep_home>/deploy` holding a
/// `deploy.toml`. Reading the directory rather than a list held anywhere
/// else is what makes a tree self-describing: it survives the dog being
/// rehomed, re-adopted under a different name, or replaced by a different
/// deploy dog entirely, which is the same reasoning that put per-target
/// state in the tree instead of in `[dog.<name>]`.
///
/// # Errors
/// [`Error::Io`], naming `<shep_home>/deploy`, if it exists but cannot be
/// listed. An absent directory is an empty list, not an error: that is
/// every shepherd with no targets yet. Also [`Error::Io`], naming the
/// offending entry, if a target directory's name is not valid UTF-8: a
/// target the dog cannot name is a broken target, not an absent one.
pub fn targets(shep_home: &Path) -> Result<Vec<String>, Error> {
    let root = shep_home.join("deploy");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::Io { path: root, source }),
    };

    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(Error::at(&root))?;
        if !entry.path().join("deploy.toml").is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(Error::Io {
                path: entry.path(),
                source: std::io::Error::other(
                    "directory name is not valid UTF-8, so it cannot name a sheep",
                ),
            });
        };
        found.push(name);
    }
    found.sort();
    Ok(found)
}

/// The deploy tree for one sheep.
///
/// Holds only the tree's root; every other path is derived from it on
/// demand rather than stored, so there is exactly one fact to keep
/// consistent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    root: PathBuf,
    sheep: String,
}

/// Whether `sheep` names one sheep, rather than a path.
///
/// [`Tree::for_sheep`] joins this onto `$SHEP_HOME/deploy`, and `PathBuf::join`
/// REPLACES the whole path when what it is given is absolute. So a sheep name
/// of `/tmp/anywhere` does not traverse out of the tree, it discards the tree
/// entirely and roots itself wherever it says. `..` traverses out the ordinary
/// way. Either one puts a deploy tree, and everything that later prunes and
/// removes inside it, somewhere the operator did not point it.
///
/// Checked at the one place a name arrives from outside, which is the command
/// line. The poll loop's names come from directory entries under
/// `$SHEP_HOME/deploy` that this crate created, so they are already inside.
///
/// The test is that the string is exactly one ordinary path component: that
/// rejects the empty string, `.`, `..`, anything absolute, and anything with a
/// separator in it, without a charset rule that would have to guess at what an
/// operator may call their app.
#[must_use]
pub fn is_sheep_name(sheep: &str) -> bool {
    let mut components = Path::new(sheep).components();
    let one_ordinary_component =
        matches!(components.next(), Some(Component::Normal(only)) if only == sheep);

    one_ordinary_component && components.next().is_none()
}

impl Tree {
    /// The tree for `sheep`, rooted at `<shep_home>/deploy/<sheep>`.
    ///
    /// Sheep name, not repository, is the key: the same repository can be
    /// deployed under several sheep names (Rin runs `bpm`, `ctm` and `opm`
    /// off one `ReactMap` checkout), and each gets its own tree.
    #[must_use]
    pub fn for_sheep(shep_home: &Path, sheep: &str) -> Self {
        // The real check lives in `main::route`; this keeps the contract
        // next to the constructor instead of trusting every caller to have
        // gone through there first.
        debug_assert!(
            is_sheep_name(sheep),
            "a sheep name is one path component, got {sheep:?}"
        );
        Self {
            root: shep_home.join("deploy").join(sheep),
            sheep: sheep.to_owned(),
        }
    }

    /// The sheep this tree belongs to.
    ///
    /// Kept as its own field rather than read back out of the last
    /// component of `root`: the deploy sequence passes this name to the
    /// shepherd, and a name that has been through a `Path` round trip is a
    /// name that can come back lossy.
    #[must_use]
    pub fn sheep(&self) -> &str {
        &self.sheep
    }

    /// The tree's own directory: everything below is inside this.
    ///
    /// Named because an abandoned cutover has to tell an operator which
    /// directory to remove before trying again, and `releases()` is not it.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The bare clone and its object store, shared by every release's
    /// worktree.
    #[must_use]
    pub fn git(&self) -> PathBuf {
        self.root.join("git")
    }

    /// The directory holding every built release, one per sha.
    #[must_use]
    pub fn releases(&self) -> PathBuf {
        self.root.join("releases")
    }

    /// One built release's worktree, by full sha.
    #[must_use]
    pub fn release(&self, sha: &str) -> PathBuf {
        self.releases().join(sha)
    }

    /// The symlink a cutover `rename(2)`s onto the new release. The sheep's
    /// `cwd` is this path, permanently, so swapping a release never leaves
    /// a moment where it points at nothing.
    #[must_use]
    pub fn current(&self) -> PathBuf {
        self.root.join("current")
    }

    /// Where a release's completion marker lives, keyed by sha.
    ///
    /// OUTSIDE the release, and that is the whole point of the path. The
    /// marker says "this checkout finished", and a marker inside the release
    /// is a file the DEPLOYED REPOSITORY can commit: `git worktree add` then
    /// writes it out as part of the checkout it is supposed to vouch for, so
    /// a kill partway leaves a partial release carrying its own certificate.
    /// That is the round-9 blocker again with the adversary holding the pen.
    ///
    /// Under the tree's own root, which only this dog writes.
    #[must_use]
    pub fn completion(&self, sha: &str) -> PathBuf {
        self.completions().join(sha)
    }

    /// The directory every completion marker lives in.
    ///
    /// Named on its own because retention sweeps it. Deriving it from
    /// `completion("")` gave a path with a trailing separator whose
    /// `parent()` was the tree root, and a sweep of the root removed
    /// `current`. Measured by the suite on 2026-09-03, before it shipped.
    #[must_use]
    pub fn completions(&self) -> PathBuf {
        self.root.join("complete")
    }

    /// The file a deploy takes an exclusive `flock` on, so only one process
    /// at a time works on this tree.
    ///
    /// Its own file rather than the record or the root: `flock` is held for
    /// the life of an open handle, and taking it on `deploy.toml` would tie
    /// the lock's lifetime to a file the deploy rewrites underneath itself.
    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        self.root.join("deploy.lock")
    }

    /// Where this sheep's [`crate::state::State`] lives.
    #[must_use]
    pub fn state_file(&self) -> PathBuf {
        self.root.join("deploy.toml")
    }

    /// The dog's own build cache for this sheep, shared by every release.
    ///
    /// Outside `releases/` deliberately: retention removes worktrees under
    /// there, and a cache swept up with one would turn the next deploy into
    /// a from-scratch build.
    #[must_use]
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// The directory every release's `target` symlink points at.
    ///
    /// Named `target` rather than pointed at by `CARGO_TARGET_DIR` because
    /// setting that variable means `./target` is never created, and a build
    /// command ending in `cp ./target/release/koji koji` then exits 1.
    /// Measured, and the reason this is a symlink at all.
    #[must_use]
    pub fn cache_target(&self) -> PathBuf {
        self.cache().join("target")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if `Tree` builds a path by joining the wrong segments, or in
    /// the wrong order. `Tree` is the one place allowed to know this
    /// layout, so a mistake here is a mistake everywhere.
    #[test]
    fn every_path_matches_the_documented_layout() {
        let tree = Tree::for_sheep(Path::new("/srv/shep"), "bpm");
        assert_eq!(tree.git(), Path::new("/srv/shep/deploy/bpm/git"));
        assert_eq!(tree.releases(), Path::new("/srv/shep/deploy/bpm/releases"));
        assert_eq!(
            tree.release("a1b2c3d"),
            Path::new("/srv/shep/deploy/bpm/releases/a1b2c3d")
        );
        assert_eq!(tree.current(), Path::new("/srv/shep/deploy/bpm/current"));
        assert_eq!(
            tree.completion("a1b2c3d"),
            Path::new("/srv/shep/deploy/bpm/complete/a1b2c3d")
        );
        assert_eq!(
            tree.lock_file(),
            Path::new("/srv/shep/deploy/bpm/deploy.lock")
        );
        assert_eq!(
            tree.state_file(),
            Path::new("/srv/shep/deploy/bpm/deploy.toml")
        );
    }

    /// fails if `is_sheep_name` accepts a name that would traverse out of
    /// the tree, or rejects an ordinary one. This is the one check standing
    /// between an operator-typed name and a tree rooted somewhere nobody
    /// pointed it.
    #[test]
    fn is_sheep_name_accepts_one_component_and_rejects_everything_else() {
        for accepted in ["bpm", "reactmap-staging", "web.2"] {
            assert!(is_sheep_name(accepted), "should accept {accepted:?}");
        }
        for rejected in ["", ".", "..", "/tmp/x", "../x", "a/b", "a/"] {
            assert!(!is_sheep_name(rejected), "should reject {rejected:?}");
        }
    }

    /// fails if a directory whose name is not valid UTF-8 is silently
    /// dropped from the target list. A target the dog cannot name never
    /// polls and never restores, with nothing said anywhere, so this has
    /// to be a loud error instead of a skip.
    #[test]
    fn a_non_utf8_target_name_is_an_error_naming_the_entry() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let home = tempfile::tempdir().expect("tempdir");
        let deploy = home.path().join("deploy");
        let bad_dir = deploy.join(OsStr::from_bytes(&[0xff, 0xfe]));
        if let Err(err) = std::fs::create_dir_all(&bad_dir) {
            // APFS refuses a name that is not valid UTF-8 outright, so on
            // macOS the entry this pins cannot exist. Linux filesystems
            // accept it, and CI runs there.
            eprintln!("skipped: this filesystem refuses the name ({err})");
            return;
        }
        std::fs::write(bad_dir.join("deploy.toml"), "").expect("write deploy.toml");

        let err = targets(home.path()).expect_err("non-UTF-8 name");
        assert!(matches!(err, Error::Io { .. }));
        assert!(format!("{err}").contains("UTF-8"));
    }

    /// fails if a tree stops knowing which sheep it belongs to. The
    /// deploy sequence sends this name to the shepherd, so a tree that
    /// answered with the wrong one would reload somebody else's app.
    #[test]
    fn a_tree_knows_its_own_sheep() {
        assert_eq!(
            Tree::for_sheep(Path::new("/srv/shep"), "bpm").sheep(),
            "bpm"
        );
    }

    /// fails if two sheep names collide onto the same tree. The layout is
    /// keyed by sheep name specifically so one repository can be deployed
    /// under several names, each with its own state.
    #[test]
    fn different_sheep_get_different_trees() {
        let a = Tree::for_sheep(Path::new("/srv/shep"), "bpm");
        let b = Tree::for_sheep(Path::new("/srv/shep"), "ctm");
        assert_ne!(a.state_file(), b.state_file());
    }

    /// fails if the cache moves out of the tree or changes name. Every
    /// release symlinks `target` at this one path, so a release built
    /// against a cache at one location and a later release linking another
    /// would silently lose every incremental artifact between them, which
    /// reads as "the build is just slow today".
    #[test]
    fn the_build_cache_lives_in_the_tree() {
        let tree = Tree::for_sheep(Path::new("/srv/shep"), "koji");
        assert_eq!(tree.cache(), Path::new("/srv/shep/deploy/koji/cache"));
        assert_eq!(
            tree.cache_target(),
            Path::new("/srv/shep/deploy/koji/cache/target")
        );
    }

    /// fails if the cache is ever placed inside `releases/`. It has to
    /// outlive every release it serves: retention removes worktrees under
    /// `releases/`, and a cache swept up with one would make the next
    /// deploy a from-scratch build, which for Koji is the exact outcome
    /// this whole mechanism exists to avoid.
    #[test]
    fn the_cache_is_not_inside_releases() {
        let tree = Tree::for_sheep(Path::new("/srv/shep"), "koji");
        assert!(!tree.cache().starts_with(tree.releases()));
    }
}
