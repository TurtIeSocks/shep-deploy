//! The short string this dog attaches to a sheep, which `shep flock` paints
//! without understanding it.
//!
//! ```text
//! ▲ main@a1b2c3      watched
//! ⏸ main@a1b2c3      manual
//! ```
//!
//! The two marks differ so `shep flock` answers "which of these is actually
//! being watched" without a second command, which was the requirement
//! smits were designed for.
//!
//! # Ephemeral, and that is a lifecycle decision
//!
//! The daemon holds smits in memory and never persists them. When a dog
//! stops for any reason, disabled, rehomed, crashed, or lost to a daemon
//! restart, its smits go with it and it republishes on its next poll. The
//! alternative, persisting them, leaves `shep flock` showing a mark
//! attributed to a dog that no longer exists, and removes that orphan class
//! only by adding cleanup to every path that can stop a dog.
//!
//! # Width is shep's half, not this one
//!
//! The smit may be dropped on a narrow terminal, and may never be dropped
//! at a full-width one, because being seen regularly at full width is what
//! makes dropping it acceptable at all. That is a property of `shep flock`'s
//! adaptive column dropping and is pinned there, not here: this module has
//! no terminal and no table.

use shep_client::shep_core::protocol::Smit;

use crate::daemon::Daemon;
use crate::error::Error;
use crate::state::{State, Watch};

/// How many characters of a sha the smit shows.
///
/// Six, matching what git itself abbreviates to by default at this
/// repository size, and short enough that the whole smit stays inside a
/// column that has to compete with nine others.
const ABBREVIATED: usize = 6;

/// The smit for one target.
///
/// `none` rather than an empty sha for a target between opt-in and its
/// first verified deploy: `main@` reads as deployed at nothing, where
/// `main@none` reads as not deployed yet.
#[must_use]
pub fn text(state: &State) -> String {
    let mark = match state.watch {
        Watch::Auto => '▲',
        Watch::Manual => '⏸',
    };
    let sha = state.deployed.as_deref().map_or("none", |sha| {
        // `get`, not a slice: a hand-edited deploy.toml carrying a short
        // sha must degrade to a shorter smit, never take the dog down on
        // its next poll.
        sha.get(..ABBREVIATED).unwrap_or(sha)
    });
    format!("{mark} {}@{sha}", state.branch)
}

/// Shorten `text` to [`Smit::MAX_CHARS`], if it is already not that short.
///
/// A branch name is the only variable-length part of a smit, and a branch
/// name long enough to matter is a real thing an operator can name, not a
/// mistake to refuse over. Publishing is cosmetic: showing a shortened mark
/// beats showing nothing at all because [`Smit::from_str`](core::str::FromStr::from_str)
/// would have refused the full one.
fn fit(text: String) -> String {
    if text.chars().count() <= Smit::MAX_CHARS {
        return text;
    }
    text.chars().take(Smit::MAX_CHARS).collect()
}

/// Publish `sheep`'s smit, built from `state`, to the shepherd.
///
/// Called every tick regardless of whether the text changed - see
/// `poll::tick`'s own doc for why - so a caller publishing on a real
/// interval should expect this to be cheap and to run often.
///
/// # Errors
/// As [`Daemon::set_smit`]. The caller is expected to log and otherwise
/// ignore a failure here: a smit is cosmetic, and a daemon that refuses one
/// is a daemon this dog can still deploy through.
pub async fn publish<D: Daemon>(daemon: &D, sheep: &str, state: &State) -> Result<(), Error> {
    daemon.set_smit(sheep, &fit(text(state))).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(watch: Watch, deployed: Option<&str>) -> State {
        State {
            remote: "https://example.com/x".to_owned(),
            branch: "main".to_owned(),
            deployed: deployed.map(str::to_owned),
            failed: None,
            verify: crate::state::Verify::default(),
            watch,
            origin_cwd: None,
            origin_script: None,
            checkout: std::path::PathBuf::from("/srv/x"),
        }
    }

    /// fails if a watched and a manual target render the same. The smit
    /// exists so `shep flock` answers "which of these is actually being
    /// watched" without a second command, and two targets that look
    /// identical answer nothing.
    #[test]
    fn watched_and_manual_are_told_apart_at_a_glance() {
        assert_eq!(
            text(&target(Watch::Auto, Some("a1b2c3d4e5f6"))),
            "▲ main@a1b2c3"
        );
        assert_eq!(
            text(&target(Watch::Manual, Some("a1b2c3d4e5f6"))),
            "⏸ main@a1b2c3"
        );
    }

    /// fails if the branch stops being named. It is the fact an operator
    /// most often wants and cannot get anywhere else in `shep flock`: three
    /// sheep deployed from one repository differ by branch, and Rin runs
    /// exactly that arrangement with bpm, ctm and opm.
    #[test]
    fn the_branch_is_named_not_assumed() {
        let mut state = target(Watch::Auto, Some("a1b2c3d4e5f6"));
        state.branch = "stable".to_owned();
        assert_eq!(text(&state), "▲ stable@a1b2c3");
    }

    /// fails if a target with no deploy yet renders as a lie. `deployed` is
    /// None between opt-in and the first verified deploy, and printing an
    /// empty sha there would read as "deployed at nothing" rather than
    /// "not deployed yet".
    #[test]
    fn a_target_with_no_deploy_yet_says_so() {
        assert_eq!(text(&target(Watch::Auto, None)), "▲ main@none");
    }

    /// fails if a sha shorter than the abbreviation panics or is padded.
    /// Nothing this crate writes produces one, so this is about a
    /// hand-edited deploy.toml degrading rather than taking the dog down on
    /// its next poll.
    #[test]
    fn a_short_sha_degrades_rather_than_panicking() {
        assert_eq!(text(&target(Watch::Auto, Some("abc"))), "▲ main@abc");
    }

    /// fails if a long branch name produces something `Smit::from_str`
    /// refuses. A branch name is the only variable-length part of a smit,
    /// and this is the case that would otherwise cost a deploy nothing
    /// merely for having a long one.
    #[test]
    fn a_long_branch_name_is_shortened_to_fit() {
        let mut state = target(Watch::Auto, Some("a1b2c3d4e5f6"));
        state.branch = "x".repeat(100);
        let shortened = fit(text(&state));
        assert_eq!(shortened.chars().count(), Smit::MAX_CHARS);
        assert!(shortened.parse::<Smit>().is_ok(), "{shortened}");
    }

    /// A [`Daemon`] whose only working method is `set_smit`, recording
    /// what it was asked to paint.
    struct Recording(std::cell::RefCell<Vec<(String, String)>>);

    impl Daemon for Recording {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            unimplemented!()
        }
        async fn list_flock(
            &self,
        ) -> Result<Vec<shep_client::shep_core::protocol::ProcessInfo>, Error> {
            unimplemented!()
        }
        async fn describe(
            &self,
            _sheep: &str,
        ) -> Result<Vec<shep_client::shep_core::protocol::ProcessInfo>, Error> {
            unimplemented!()
        }
        async fn start(
            &self,
            _apps: Vec<shep_client::shep_core::config::AppConfig>,
        ) -> Result<(), Error> {
            unimplemented!()
        }
        async fn delete(&self, _id: u32) -> Result<(), Error> {
            unimplemented!()
        }
        async fn reload(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
        async fn restart(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
        async fn save_roll(&self) -> Result<std::path::PathBuf, Error> {
            unimplemented!()
        }
        async fn set_smit(&self, sheep: &str, text: &str) -> Result<(), Error> {
            self.0
                .borrow_mut()
                .push((sheep.to_owned(), text.to_owned()));
            Ok(())
        }
    }

    /// fails if `publish` asks the daemon for the wrong sheep, or for text
    /// other than what `text` renders.
    #[tokio::test]
    async fn publish_sends_the_named_sheeps_own_text() {
        let daemon = Recording(std::cell::RefCell::new(Vec::new()));
        let state = target(Watch::Auto, Some("a1b2c3d4e5f6"));
        publish(&daemon, "web", &state).await.expect("publishes");
        assert_eq!(
            daemon.0.into_inner(),
            vec![("web".to_owned(), "▲ main@a1b2c3".to_owned())]
        );
    }
}
