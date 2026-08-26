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
//! ├── current -> releases/<sha>   swapped with rename(2) at cutover
//! └── deploy.toml          the sheep's `State` - see `crate::state`
//! ```

use std::path::{Path, PathBuf};

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
}
