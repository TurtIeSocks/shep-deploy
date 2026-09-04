//! Reading shep's muster roll, which is the only place a dog can see the
//! `AppConfig` a sheep was REGISTERED with.
//!
//! shep states the reason itself, in its own `snapshot.rs`: "nothing in a
//! `ProcessInfo` can reproduce the `AppConfig` a sheep came from, which is
//! exactly what a roll needs". [`ProcessInfo`] carries status, pid, uptime
//! and log paths, and carries neither `cwd` nor `script`. The survey needs
//! the first to tell a git checkout from anything else, and opt-in needs
//! both to record what a removal should restore.
//!
//! # The coupling, named rather than glossed
//!
//! The roll's envelope is `shep-daemon`'s type and this crate does not
//! depend on `shep-daemon`, so the two fields needed here are mirrored and
//! everything else is ignored. That direction is safe: a field added to
//! the envelope later cannot break this. The inner [`AppConfig`] is
//! `shep-core`'s and is shared properly, but it is `deny_unknown_fields`,
//! so a NEWER shepherd writing a field this crate's `shep-core` does not
//! know would be refused. [`read`] turns that into a message naming the
//! file and the likely cause rather than a bare serde complaint, because
//! an operator meeting "unknown field" with no context has nothing to act
//! on.
//!
//! [`ProcessInfo`]: shep_client::shep_core::protocol::ProcessInfo

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;
use shep_client::shep_core::config::AppConfig;

use crate::daemon::Daemon;
use crate::error::Error;

/// The roll's envelope, mirrored down to the one field this crate reads.
#[derive(Deserialize)]
struct Roll {
    apps: Vec<Entry>,
}

/// One sheep's entry. `instances_running` is shep-daemon's and unused here.
#[derive(Deserialize)]
struct Entry {
    app: AppConfig,
}

/// Every registered sheep's config, keyed by name, freshly written.
///
/// Asks the shepherd to write the roll first, so this never reads a
/// debounced snapshot that predates a sheep the caller just started.
///
/// # Errors
/// Whatever [`Daemon::save_roll`] returns, plus [`Error::Io`] naming the
/// roll if it cannot be read and [`Error::Config`] if it cannot be parsed.
pub async fn registered<D: Daemon>(daemon: &D) -> Result<BTreeMap<String, AppConfig>, Error> {
    let path = daemon.save_roll().await?;
    let text = fs::read_to_string(&path).map_err(Error::at(&path))?;
    read(&path, &text)
}

/// [`parse`], with the failure named against `path`.
///
/// # Errors
/// [`Error::Config`] naming `path` and the likeliest cause.
fn read(path: &Path, text: &str) -> Result<BTreeMap<String, AppConfig>, Error> {
    parse(text).map_err(|source| {
        Error::Config(format!(
            "{}: this shepherd's muster roll is not one this dog can read ({source}). The \
             likeliest cause is a newer shepherd than the shep-core this dog was built against: \
             the roll carries each sheep's own app config, and an app field added since refuses \
             to parse rather than being ignored. Rebuilding or reinstalling shep-deploy against \
             the current shep fixes it.",
            path.display()
        ))
    })
}

/// The roll's apps, keyed by name.
///
/// # Errors
/// [`serde_json::Error`] if the text is not a roll this crate understands.
fn parse(text: &str) -> Result<BTreeMap<String, AppConfig>, serde_json::Error> {
    let roll: Roll = serde_json::from_str(text)?;
    Ok(roll
        .apps
        .into_iter()
        .map(|entry| (entry.app.name.clone(), entry.app))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A roll as the daemon writes one, with the envelope fields this
    /// crate ignores present so the test proves they are ignored rather
    /// than merely absent.
    fn roll_json(apps: &str) -> String {
        format!("{{\"version\":1,\"saved_at_ms\":1787756216990,\"apps\":[{apps}]}}")
    }

    /// fails if the roll stops yielding the one thing it exists for: the
    /// cwd and script a sheep was REGISTERED with. `ProcessInfo` carries
    /// neither, and shep's own snapshot module says so, so losing this
    /// leaves the survey unable to tell a git checkout from anything else
    /// and opt-in unable to record what to restore.
    #[test]
    fn a_roll_yields_each_sheeps_registered_cwd_and_script() {
        let text = roll_json(
            "{\"app\":{\"name\":\"bpm\",\"script\":\"bun .\",\"cwd\":\"/srv/reactmap\"},\
             \"instances_running\":2}",
        );
        let apps = parse(&text).expect("parses");
        let bpm = apps.get("bpm").expect("bpm is in the roll");
        assert_eq!(bpm.cwd.as_deref(), Some("/srv/reactmap"));
        assert_eq!(bpm.script, "bun .");
    }

    /// fails if the envelope's own fields are treated as app config, or if
    /// an unknown envelope field breaks the parse. `instances_running` is
    /// shep-daemon's and this crate has no use for it; a field added to
    /// that envelope later must not stop the dog reading the roll.
    #[test]
    fn envelope_fields_this_crate_does_not_use_are_ignored() {
        let text = "{\"version\":1,\"saved_at_ms\":0,\"something_new\":true,\"apps\":\
                    [{\"app\":{\"name\":\"web\",\"script\":\"./s\"},\"instances_running\":1,\
                    \"another_new_one\":[]}]}";
        assert!(parse(text).expect("parses").contains_key("web"));
    }

    /// fails if an empty flock is an error rather than an empty map. A
    /// shepherd with nothing registered writes `"apps":[]`, and the survey
    /// has to report an empty flock rather than a failure.
    #[test]
    fn an_empty_flock_is_an_empty_map_not_an_error() {
        assert!(parse(&roll_json("")).expect("parses").is_empty());
    }

    /// fails if a roll this crate cannot understand produces a bare serde
    /// message. The likeliest cause by far is a newer shepherd writing an
    /// AppConfig field this crate's shep-core does not know, and an
    /// operator meeting "unknown field `foo`" with no context has nothing
    /// to act on. The message names the file and the likely cause.
    #[test]
    fn an_unreadable_roll_names_the_file_and_the_likely_cause() {
        // `AppConfig` is `deny_unknown_fields`, so a field this crate's
        // shep-core has never heard of - the shape a newer shepherd would
        // actually write - is what makes the parse fail. `{"app":{}}`
        // alone parses fine: every field but `name` defaults.
        let text = "{\"apps\":[{\"app\":{\"name\":\"web\",\"a_field_from_the_future\":1}}]}";
        let err = read(Path::new("/srv/shep/flock.json"), text).expect_err("refuses");
        let shown = err.to_string();
        assert!(shown.contains("/srv/shep/flock.json"), "{shown}");
        assert!(shown.contains("newer"), "{shown}");
    }
}
