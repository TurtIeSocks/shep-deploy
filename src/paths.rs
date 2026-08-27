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
//! └── deploy.toml          the sheep's `State` - see `crate::state`
//! ```

use std::path::{Path, PathBuf};

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
/// every shepherd with no targets yet.
pub fn targets(shep_home: &Path) -> Result<Vec<String>, Error> {
    let root = shep_home.join("deploy");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::Io { path: root, source }),
    };

    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: root.clone(),
            source,
        })?;
        if !entry.path().join("deploy.toml").is_file() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            found.push(name.to_owned());
        }
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

impl Tree {
    /// The tree for `sheep`, rooted at `<shep_home>/deploy/<sheep>`.
    ///
    /// Sheep name, not repository, is the key: the same repository can be
    /// deployed under several sheep names (Rin runs `bpm`, `ctm` and `opm`
    /// off one `ReactMap` checkout), and each gets its own tree.
    #[must_use]
    pub fn for_sheep(shep_home: &Path, sheep: &str) -> Self {
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
            tree.state_file(),
            Path::new("/srv/shep/deploy/bpm/deploy.toml")
        );
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
