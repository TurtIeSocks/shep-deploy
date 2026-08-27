//! `shep-deploy survey`: where every registered sheep stands.
//!
//! Read-only, end to end. Discovery reports; it never starts, registers, or
//! writes anything, because turning a directory into a deploy target is the
//! operator's decision - a dog that made it unasked would be acting on a
//! checkout it was only ever pointed at, not handed.

use std::path::Path;

use shep_client::shep_core::config::AppConfig;

use crate::daemon::Daemon;
use crate::error::Error;
use crate::paths::Tree;
use crate::roll;
use crate::state::{State, Watch};

/// Where one sheep stands with respect to this dog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Already a target, polled for new commits.
    Watched {
        /// The branch it tracks.
        branch: String,
        /// The sha it is deployed at, or `None` before its first deploy.
        sha: Option<String>,
    },
    /// Already a target, deployed only when asked.
    Manual {
        /// The branch it tracks.
        branch: String,
        /// As [`Self::Watched::sha`].
        sha: Option<String>,
    },
    /// A git checkout whose repository ships a `Flockfile.toml`, so
    /// upstream has said how to run it and nothing has taken it over yet.
    NeedsSetup,
    /// A git checkout that could be taken over, where nothing declares a
    /// deploy.
    Eligible,
    /// Left alone, and why.
    NotEligible(String),
}

/// Where `app` stands, without touching anything.
///
/// Order matters and is not arbitrary. An existing target is answered
/// first, because a sheep already being deployed is not "eligible" and
/// offering it again invites a second opt-in that would clone over a live
/// tree. Then a missing `cwd`, then a `cwd` that is not a checkout, and
/// only then the Flockfile question, which is the only one that needs the
/// checkout to be a checkout.
#[must_use]
pub fn classify(shep_home: &Path, app: &AppConfig) -> Standing {
    let tree = Tree::for_sheep(shep_home, &app.name);
    if let Ok(state) = State::read(&tree.state_file()) {
        let (branch, sha) = (state.branch.clone(), state.deployed.clone());
        return match state.watch {
            Watch::Auto => Standing::Watched { branch, sha },
            Watch::Manual => Standing::Manual { branch, sha },
        };
    }

    let Some(cwd) = app.cwd.as_deref() else {
        return Standing::NotEligible(
            "shep records no working directory for it, so there is nothing to inspect".to_owned(),
        );
    };

    let checkout = Path::new(cwd);
    if !checkout.join(".git").exists() {
        return Standing::NotEligible(format!("{cwd} is not a git repository"));
    }

    if checkout.join("Flockfile.toml").is_file() {
        Standing::NeedsSetup
    } else {
        Standing::Eligible
    }
}

/// Three columns: name, standing, reason.
///
/// Padded to the widest of each of the first two rather than to fixed
/// widths, because a flock of `bpm` and one of
/// `reactmap-staging-europe` should both read as columns.
#[must_use]
pub fn render(rows: &[(String, Standing)]) -> String {
    if rows.is_empty() {
        return "no sheep are registered, so there is nothing to survey\n".to_owned();
    }

    let name_width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0) + 2;
    let label_width = rows
        .iter()
        .map(|(_, standing)| standing.label().len())
        .max()
        .unwrap_or(0)
        + 2;

    rows.iter()
        .map(|(name, standing)| {
            format!(
                "{name:name_width$}{:label_width$}{}\n",
                standing.label(),
                standing.reason()
            )
        })
        .collect()
}

impl Standing {
    /// The one or two words in the second column.
    fn label(&self) -> &'static str {
        match self {
            Self::Watched { .. } => "watched",
            Self::Manual { .. } => "manual",
            Self::NeedsSetup => "needs setup",
            Self::Eligible => "eligible",
            Self::NotEligible(_) => "not eligible",
        }
    }

    /// The third column, which is the half that says what to do next.
    fn reason(&self) -> String {
        match self {
            Self::Watched { branch, sha } => {
                format!(
                    "{}, deploys on every new commit",
                    at(branch, sha.as_deref())
                )
            }
            Self::Manual { branch, sha } => {
                format!("{}, deploys only when asked", at(branch, sha.as_deref()))
            }
            Self::NeedsSetup => "a git checkout that ships a Flockfile".to_owned(),
            Self::Eligible => "a git checkout, nothing declares a deploy".to_owned(),
            Self::NotEligible(why) => why.clone(),
        }
    }
}

/// `main@a1b2c3`, or just the branch for a target with no deploy yet.
///
/// `get`, not a slice: a hand-edited `deploy.toml` carrying a sha shorter
/// than six characters must degrade to that shorter string rather than
/// panic in the middle of a listing.
fn at(branch: &str, sha: Option<&str>) -> String {
    sha.map_or_else(
        || format!("{branch}, not deployed yet"),
        |sha| format!("{branch}@{}", sha.get(..6).unwrap_or(sha)),
    )
}

/// The whole flock, classified and rendered.
///
/// # Errors
/// Whatever [`crate::roll::registered`] returns.
pub async fn survey<D: Daemon>(daemon: &D, shep_home: &Path) -> Result<String, Error> {
    let apps = roll::registered(daemon).await?;
    let rows: Vec<(String, Standing)> = apps
        .values()
        .map(|app| (app.name.clone(), classify(shep_home, app)))
        .collect();
    Ok(render(&rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `AppConfig` with just the two fields this module reads.
    fn app(name: &str, cwd: Option<&str>) -> AppConfig {
        let mut app: AppConfig =
            toml::from_str(&format!("name = {name:?}\nscript = \"./run.sh\"")).expect("parses");
        app.cwd = cwd.map(str::to_owned);
        app
    }

    /// A tempdir with `git init -q` already run in it, plus the given files.
    fn checkout_fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(dir.path())
            .status()
            .expect("git is on PATH");
        assert!(status.success(), "git init failed");
        for (name, contents) in files {
            std::fs::write(dir.path().join(name), contents).expect("write fixture file");
        }
        dir
    }

    /// Writes `<home>/deploy/<sheep>/deploy.toml` through `State::write`, so
    /// the fixture and the real reader agree by construction rather than by
    /// a hand-written string that can drift.
    fn write_target(home: &Path, sheep: &str, watch: Watch, sha: Option<&str>) {
        let tree = Tree::for_sheep(home, sheep);
        std::fs::create_dir_all(tree.state_file().parent().expect("has a parent"))
            .expect("create target dir");
        let state = State {
            remote: "https://example.com/x".to_owned(),
            branch: "main".to_owned(),
            deployed: sha.map(str::to_owned),
            failed: None,
            verify: crate::state::Verify::default(),
            watch,
            origin_cwd: None,
            origin_script: None,
            checkout: std::path::PathBuf::from("/srv/x"),
        };
        state.write(&tree.state_file()).expect("write state");
    }

    /// fails if a sheep whose cwd is not a git checkout is offered for
    /// deployment. Turning a directory into a checkout is the operator's
    /// decision, and a dog that started doing it for them would be acting
    /// on a directory it was never pointed at.
    #[test]
    fn a_cwd_that_is_not_a_checkout_is_not_eligible_and_says_why() {
        let home = tempfile::tempdir().expect("tempdir");
        let plain = tempfile::tempdir().expect("tempdir");
        let standing = classify(home.path(), &app("legacy", plain.path().to_str()));
        let Standing::NotEligible(why) = standing else {
            panic!("expected NotEligible, got {standing:?}");
        };
        assert!(why.contains(plain.path().to_str().expect("utf-8")), "{why}");
        assert!(why.contains("git"), "{why}");
    }

    /// fails if a sheep shep records no working directory for is reported
    /// as anything but ineligible. There is nothing to inspect, so there is
    /// nothing to offer.
    #[test]
    fn a_sheep_with_no_recorded_cwd_is_not_eligible() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            classify(home.path(), &app("odd", None)),
            Standing::NotEligible(_)
        ));
    }

    /// fails if a checkout that ships its own Flockfile stops being
    /// distinguished from one that does not. That distinction is the whole
    /// argument for building this: upstream shipping the app definition is
    /// what turns "how do I run this" from a README section people misread
    /// into a file.
    #[test]
    fn a_checkout_shipping_a_flockfile_needs_setup_and_one_without_is_merely_eligible() {
        let home = tempfile::tempdir().expect("tempdir");

        let declared = checkout_fixture(&[("Flockfile.toml", "[[app]]\nname='x'\nscript='y'\n")]);
        assert!(matches!(
            classify(home.path(), &app("reactmap", declared.path().to_str())),
            Standing::NeedsSetup
        ));

        let bare = checkout_fixture(&[]);
        assert!(matches!(
            classify(home.path(), &app("koji", bare.path().to_str())),
            Standing::Eligible
        ));
    }

    /// fails if a sheep that is ALREADY a deploy target is offered as
    /// eligible. Opting in twice would clone over a live tree, and the row
    /// an operator most wants from this command is which of their sheep are
    /// already being watched.
    ///
    /// The registered `cwd` is deliberately NOT a git checkout: a real
    /// target's `cwd` is `current`, a plain directory once the dog has
    /// taken it over (see the README's own note on this), so a valid
    /// checkout here would let the `.git` check pass anyway and hide a bug
    /// where it runs before the existing-target check. Only a `cwd` that
    /// would fail every later check proves the existing-target answer wins
    /// regardless of ordering.
    #[test]
    fn an_existing_target_reports_its_watch_mode_not_its_eligibility() {
        let home = tempfile::tempdir().expect("tempdir");
        let current = tempfile::tempdir().expect("tempdir");
        write_target(home.path(), "bpm", Watch::Manual, Some("a1b2c3d4e5f6"));

        let standing = classify(home.path(), &app("bpm", current.path().to_str()));
        assert!(matches!(standing, Standing::Manual { .. }), "{standing:?}");
    }

    /// fails if `.git` is checked with `is_dir()` rather than `exists()`. A
    /// git worktree's `.git` is a file, not a directory, and an operator
    /// running a sheep out of a worktree is running it out of a checkout.
    #[test]
    fn a_worktree_checkout_is_still_a_checkout() {
        let home = tempfile::tempdir().expect("tempdir");
        let origin = checkout_fixture(&[]);
        let worktree = tempfile::tempdir().expect("tempdir");
        // An empty repo has no commits to base a worktree on yet.
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(origin.path())
            .arg("commit")
            .arg("--allow-empty")
            .arg("-q")
            .arg("-m")
            .arg("init")
            .status()
            .expect("git is on PATH");
        assert!(status.success(), "git commit failed");
        std::fs::remove_dir(worktree.path()).expect("remove tempdir stand-in");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(origin.path())
            .arg("worktree")
            .arg("add")
            .arg("--detach")
            .arg(worktree.path())
            .status()
            .expect("git is on PATH");
        assert!(status.success(), "git worktree add failed");
        assert!(worktree.path().join(".git").is_file(), "not a worktree");

        assert!(matches!(
            classify(home.path(), &app("bpm", worktree.path().to_str())),
            Standing::Eligible
        ));
    }

    /// fails if the rendered table stops naming every standing, or stops
    /// reading as columns. This is the entire output of a command whose
    /// only job is to be read, and an exact string is what keeps a later
    /// change from quietly dropping the reason column, which is the half
    /// that tells an operator what to do next.
    #[test]
    fn the_rendered_survey_is_three_aligned_columns() {
        let rows = vec![
            (
                "bpm".to_owned(),
                Standing::Watched {
                    branch: "main".to_owned(),
                    sha: Some("a1b2c3d4e5f6".to_owned()),
                },
            ),
            ("reactmap".to_owned(), Standing::NeedsSetup),
            ("koji".to_owned(), Standing::Eligible),
            (
                "legacy".to_owned(),
                Standing::NotEligible("/opt/legacy is not a git repository".to_owned()),
            ),
        ];
        assert_eq!(
            render(&rows),
            "bpm       watched       main@a1b2c3, deploys on every new commit\n\
             reactmap  needs setup   a git checkout that ships a Flockfile\n\
             koji      eligible      a git checkout, nothing declares a deploy\n\
             legacy    not eligible  /opt/legacy is not a git repository\n"
        );
    }

    /// fails if an empty flock renders as an empty string. A command that
    /// prints nothing is indistinguishable from one that failed silently,
    /// and an empty flock is the ordinary state of a fresh shepherd.
    #[test]
    fn an_empty_flock_says_so() {
        assert!(render(&[]).contains("no sheep"));
    }
}
