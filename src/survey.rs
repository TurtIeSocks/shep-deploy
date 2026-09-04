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
use crate::paths::{self, Tree};
use crate::roll;
use crate::state::{State, Watch};

/// Where one sheep stands with respect to this dog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Already a target, polled for new commits, with nothing held back.
    Watched {
        /// The branch it tracks.
        branch: String,
        /// The sha it is deployed at, or `None` before its first deploy.
        sha: Option<String>,
    },
    /// Already a target and watched, holding one commit that did not land.
    ///
    /// Not broken and not paused by anybody. `watch` is still `auto` and a
    /// newer commit deploys as usual; what the loop is declining to do is
    /// attempt THAT sha again, having already built it, swapped to it and
    /// rolled it back once. See [`crate::state::State::failed`].
    ///
    /// Watched-with-a-hold rather than a variant of watched, because those
    /// two are the rows an operator most needs to tell apart: one has
    /// nothing to do and the other has been stuck since somebody pushed.
    ///
    /// Read from the record alone. This module never fetches - it is
    /// read-only end to end - so a hold that a push has already cleared
    /// still shows here until the tick that deploys the newer commit.
    Held {
        /// The branch it tracks.
        branch: String,
        /// The sha it is deployed at - what is still serving - or `None`
        /// before its first deploy.
        sha: Option<String>,
        /// The sha that did not land, which the loop is leaving alone.
        failed: String,
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
    /// A deploy tree with no registered sheep behind it.
    ///
    /// The one row the roll cannot produce, because the roll is what it is
    /// missing from: a sheep deleted from shep while its tree stayed on disk
    /// appeared in no row of the one command meant to say where everything
    /// stands.
    Orphaned,
    /// A deploy tree whose record cannot be read, and what reading it said.
    ///
    /// Its own standing rather than a fall-through, because the fall-through
    /// was dangerous: a live target's `cwd` is `current`, a git worktree
    /// shipping a `Flockfile.toml`, so an unreadable record classified it as
    /// `NeedsSetup` and told the operator to run `setup` against a running
    /// service, which clones over its tree. Reachable without corruption:
    /// the record refuses unknown fields, so one written by a newer
    /// shep-deploy reads this way to an older binary.
    Unreadable(String),
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
    let state = match State::read(&tree.state_file()) {
        Ok(state) => Some(state),
        // No record is no target, which is the ordinary case for every
        // sheep the dog has not taken over.
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Standing::Unreadable(err.to_string()),
    };
    if let Some(state) = state {
        let (branch, sha) = (state.branch, state.deployed);
        // A hold is only reported for a target something polls. `failed`
        // is written by an operator's own deploy too, and for a manual
        // target it changes nothing at all - the next `shep deploy` retries
        // that sha deliberately - so calling it "held" would name a
        // restraint that is not there.
        return match (state.watch, state.failed) {
            (Watch::Auto, Some(failed)) => Standing::Held {
                branch,
                sha,
                failed,
            },
            (Watch::Auto, None) => Standing::Watched { branch, sha },
            (Watch::Manual, _) => Standing::Manual { branch, sha },
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

    // Characters, not bytes: `{name:width$}` pads by character, and a name
    // with a multi-byte character in it would otherwise pull its row's
    // columns left of every other row's.
    let name_width = rows
        .iter()
        .map(|(name, _)| name.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
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
            Self::Held { .. } => "held",
            Self::Manual { .. } => "manual",
            Self::NeedsSetup => "needs setup",
            Self::Eligible => "eligible",
            Self::NotEligible(_) => "not eligible",
            Self::Orphaned => "orphaned",
            Self::Unreadable(_) => "unreadable",
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
            // Says all four things, because leaving any of them out is how
            // this row gets read as "the dog has stopped": what is still
            // serving, which commit is held, that the commit is the reason
            // rather than the dog, and that a push fixes it.
            Self::Held {
                branch,
                sha,
                failed,
            } => format!(
                "{}, holding {} after it did not land. A newer commit or a landing deploy \
                 clears it",
                at(branch, sha.as_deref()),
                short(failed)
            ),
            Self::Manual { branch, sha } => {
                format!("{}, deploys only when asked", at(branch, sha.as_deref()))
            }
            Self::NeedsSetup => "a git checkout that ships a Flockfile".to_owned(),
            Self::Eligible => "a git checkout, nothing declares a deploy".to_owned(),
            Self::NotEligible(why) => why.clone(),
            Self::Orphaned => "has a deploy tree, and the shepherd has no sheep by that name. \
                               Re-register the app from its Flockfile with `cwd` set to the \
                               tree's `current`, or remove the tree if the app is gone"
                .to_owned(),
            // Says what to do and, more to the point, what NOT to do: the
            // one wrong move here is treating it as a sheep with no tree.
            Self::Unreadable(why) => format!(
                "has a deploy tree whose record cannot be read: {why}. Repair the record \
                 before deploying; do not run setup against it, the sheep may be running from \
                 inside that tree"
            ),
        }
    }
}

/// `main@a1b2c3`, or just the branch for a target with no deploy yet.
fn at(branch: &str, sha: Option<&str>) -> String {
    sha.map_or_else(
        || format!("{branch}, not deployed yet"),
        |sha| format!("{branch}@{}", short(sha)),
    )
}

/// A sha as a listing shows it: the first six characters.
///
/// `get`, not a slice: a hand-edited `deploy.toml` carrying a sha shorter
/// than six characters must degrade to that shorter string rather than
/// panic in the middle of a listing.
fn short(sha: &str) -> &str {
    sha.get(..6).unwrap_or(sha)
}

/// The whole flock, classified and rendered.
///
/// # Errors
/// Whatever [`crate::roll::registered`] returns.
pub async fn survey<D: Daemon>(daemon: &D, shep_home: &Path) -> Result<String, Error> {
    let apps = roll::registered(daemon).await?;
    let targets = paths::targets(shep_home)?;
    Ok(render(&rows(shep_home, &apps, &targets)))
}

/// One row per registered sheep, then one per deploy tree no registered
/// sheep accounts for.
///
/// Split from [`survey`] so the join is testable without a daemon: the
/// roll comes from one, the target list from the filesystem, and the row a
/// tree gets when it is in the second and not the first is the whole of
/// what this adds.
fn rows(
    shep_home: &Path,
    apps: &std::collections::BTreeMap<String, AppConfig>,
    targets: &[String],
) -> Vec<(String, Standing)> {
    let mut rows: Vec<(String, Standing)> = apps
        .values()
        .map(|app| (app.name.clone(), classify(shep_home, app)))
        .collect();
    rows.extend(
        targets
            .iter()
            .filter(|name| !apps.contains_key(*name))
            .map(|name| (name.clone(), Standing::Orphaned)),
    );
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    /// fails if a target whose record cannot be read is offered `setup`.
    ///
    /// A live target's `cwd` is `current`, a git worktree that ships a
    /// `Flockfile.toml`, so falling through to the checkout questions
    /// classified it as `NeedsSetup` and the row told an operator to run
    /// `shep-deploy setup` against a running service, which clones over its
    /// tree. A record with a key this binary does not know is enough to get
    /// there: one written by a newer shep-deploy reads this way to an older
    /// one.
    #[test]
    fn a_target_with_an_unreadable_record_is_not_offered_setup() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "web");
        std::fs::create_dir_all(tree.root()).expect("tree");
        std::fs::write(
            tree.state_file(),
            "remote = \"https://example.com/x\"\nbranch = \"main\"\ncheckout = \"/srv/x\"\n\
             newer_key = true\n",
        )
        .expect("a record from the future");
        let checkout = checkout_fixture(&[("Flockfile.toml", "[[app]]\nname = \"web\"\n")]);

        let standing = classify(home.path(), &app("web", checkout.path().to_str()));

        assert!(
            matches!(&standing, Standing::Unreadable(why) if why.contains("newer_key")),
            "must name what was wrong with the record: {standing:?}"
        );
        let shown = render(&[("web".to_owned(), standing)]);
        assert!(!shown.contains("needs setup"), "{shown}");
        assert!(shown.contains("do not run setup"), "{shown}");
    }

    /// fails if a deploy tree whose sheep is no longer registered appears in
    /// no row. The roll cannot name it, because the roll is what it is
    /// missing from, and a tree on disk with a service that may still be
    /// running out of it is exactly what an operator surveys for.
    #[test]
    fn a_tree_with_no_registered_sheep_is_a_row_of_its_own() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target(home.path(), "gone", Watch::Auto, None, None);
        let registered: std::collections::BTreeMap<String, AppConfig> =
            [("web".to_owned(), app("web", None))].into_iter().collect();
        let targets = paths::targets(home.path()).expect("lists");

        let rows = rows(home.path(), &registered, &targets);

        assert!(
            rows.iter()
                .any(|(name, standing)| name == "gone" && *standing == Standing::Orphaned),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|(name, _)| name == "web"),
            "the registered sheep is still listed: {rows:?}"
        );
        let shown = render(&rows);
        assert!(shown.contains("orphaned"), "{shown}");
    }

    /// An `AppConfig` with just the two fields this module reads.
    fn app(name: &str, cwd: Option<&str>) -> AppConfig {
        let mut app: AppConfig =
            toml::from_str(&format!("name = {name:?}\nscript = \"./run.sh\"")).expect("parses");
        app.cwd = cwd.map(str::to_owned);
        app
    }

    /// A git checkout holding `files` and NOT committing them.
    ///
    /// Uncommitted on purpose: two tests here pass no files at all, and a
    /// repository with nothing staged cannot be committed. Nothing this
    /// module classifies reads a commit, only whether `.git` is there and
    /// what the working tree holds.
    fn checkout_fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = fixtures::empty_checkout();
        fixtures::write_files(dir.path(), files);
        dir
    }

    /// Writes `<home>/deploy/<sheep>/deploy.toml` through `State::write`, so
    /// the fixture and the real reader agree by construction rather than by
    /// a hand-written string that can drift.
    fn write_target(
        home: &Path,
        sheep: &str,
        watch: Watch,
        sha: Option<&str>,
        failed: Option<&str>,
    ) {
        let tree = Tree::for_sheep(home, sheep);
        std::fs::create_dir_all(tree.state_file().parent().expect("has a parent"))
            .expect("create target dir");
        let state = State {
            deployed: sha.map(str::to_owned),
            failed: failed.map(str::to_owned),
            watch,
            ..crate::fixtures::state()
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
        write_target(
            home.path(),
            "bpm",
            Watch::Manual,
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"),
            None,
        );

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

    /// fails if a target holding a commit that did not land reads as one
    /// that is simply up to date. It is the same row today: the sha shown
    /// is the one still serving, and the reason says it deploys on every
    /// new commit, so a target that has been stuck since yesterday and one
    /// that has nothing to do are the same three columns.
    ///
    /// Asserted on the words rather than on the variant, because the words
    /// are what an operator reads and the variant is not.
    #[test]
    fn a_held_target_says_what_it_is_holding_and_what_clears_it() {
        let home = tempfile::tempdir().expect("tempdir");
        let current = tempfile::tempdir().expect("tempdir");
        write_target(
            home.path(),
            "bpm",
            Watch::Auto,
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"),
            Some("d4e5f6a7b8c9d4e5f6a7b8c9d4e5f6a7b8c9d4e5"),
        );

        let row = render(&[(
            "bpm".to_owned(),
            classify(home.path(), &app("bpm", current.path().to_str())),
        )]);

        assert!(row.contains("held"), "{row}");
        assert!(row.contains("a1b2c3"), "what is still serving: {row}");
        assert!(row.contains("d4e5f6"), "what it is holding: {row}");
        assert!(row.contains("did not land"), "why it is holding: {row}");
        assert!(row.contains("newer commit"), "what clears it: {row}");
        assert!(
            !row.contains("deploys on every new commit"),
            "that is the line this row is not: {row}"
        );
    }

    /// fails if a manual target with a failed sha is reported as holding.
    /// The hold is the poll loop's and nothing polls a manual target, so
    /// there is no restraint to describe: an operator asking by name
    /// retries that same commit deliberately. "Holding" would name a thing
    /// that is not happening to them.
    #[test]
    fn a_manual_target_with_a_failed_sha_is_still_manual() {
        let home = tempfile::tempdir().expect("tempdir");
        let current = tempfile::tempdir().expect("tempdir");
        write_target(
            home.path(),
            "bpm",
            Watch::Manual,
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"),
            Some("d4e5f6a7b8c9d4e5f6a7b8c9d4e5f6a7b8c9d4e5"),
        );

        let row = render(&[(
            "bpm".to_owned(),
            classify(home.path(), &app("bpm", current.path().to_str())),
        )]);

        assert!(row.contains("only when asked"), "{row}");
        assert!(!row.contains("held"), "{row}");
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
                    sha: Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_owned()),
                },
            ),
            (
                "koji-staging".to_owned(),
                Standing::Manual {
                    branch: "main".to_owned(),
                    sha: Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_owned()),
                },
            ),
            (
                "reactmap-eu".to_owned(),
                Standing::Held {
                    branch: "main".to_owned(),
                    sha: Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_owned()),
                    failed: "d4e5f6a7b8c9d4e5f6a7b8c9d4e5f6a7b8c9d4e5".to_owned(),
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
            "bpm           watched       main@a1b2c3, deploys on every new commit\n\
             koji-staging  manual        main@a1b2c3, deploys only when asked\n\
             reactmap-eu   held          main@a1b2c3, holding d4e5f6 after it did not land. A \
             newer commit or a landing deploy clears it\n\
             reactmap      needs setup   a git checkout that ships a Flockfile\n\
             koji          eligible      a git checkout, nothing declares a deploy\n\
             legacy        not eligible  /opt/legacy is not a git repository\n"
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
