//! The abstraction every later task uses to talk to the shepherd.
//!
//! One trait, [`Daemon`], names every request this dog makes across the
//! whole engine: reading its own config section, listing and describing the
//! flock, and starting, reloading or restarting a sheep. Everything from
//! Task 3 onward is written against this trait, never against
//! [`Client`] directly, so the whole deploy engine is unit-testable with no
//! shepherd running - a task can hand it a fake and assert on what the fake
//! recorded.
//!
//! There is one real implementation, [`Live`], a thin wrapper over
//! [`Client`] that turns each method into one [`Request`] and matches the
//! [`Response`] it expects back. Test doubles live in each module's own
//! `#[cfg(test)]` block, this one included.
//!
//! # Why a trait at all
//!
//! The `async fn` here is used through a generic bound only, never behind
//! `dyn`. That is a decision rather than an accident: an `async fn` in a
//! trait cannot spell an auto-trait bound on the future it returns, so a
//! caller needing `Send` could not ask for one. This binary runs a single
//! current-thread runtime and has no such caller. The `async_fn_in_trait`
//! lint stays quiet here because a binary's trait is not a public surface;
//! lift this into a library and it starts firing, at which point the
//! trade-off wants deciding rather than silencing.
//!
//! Do not add trait methods speculatively. These eight are the surface plan
//! one and plan two need between them, not eight that looked useful:
//! `list_flock`, `describe` and `reload` are exercised inside plan one
//! (`adopted_name` below, probe-based verification, and the deploy
//! sequence). `dog_config` reads the `[dog.<name>]` section that names the
//! poll loop's interval and retention, via `crate::config::read` and
//! `adopted_name` below. `start` and `delete` are the cutover's, in
//! `crate::optin`: it registers the sheep against `current` and then
//! removes the instances that `Start` was added beside. `restart` is the
//! one with no caller at all; Task 10 chose `Reload` for the whole deploy
//! sequence, and it stays because plan one's own brief named it up front.
//!
//! # Self-identification
//!
//! [`adopted_name`] is the documented workaround for a real gap: an adopted
//! dog is spawned with no argv at all and exactly one environment entry,
//! `SHEP_HOME`, so there is nothing in the process's own environment that
//! says what it was adopted as. What there is instead is the flock listing:
//! it reports a pid per entry and a dog marker per dog, and this process
//! knows its own pid. Getting this wrong means every config lookup silently
//! reads an empty section, which looks exactly like a dog correctly running
//! on defaults - there is no error to notice.

use std::path::PathBuf;

use shep_client::{
    Client,
    shep_core::config::AppConfig,
    shep_core::protocol::{ProcessInfo, Request, Response, SelectorSpec, Smit},
};

use crate::error::Error;

/// Everything this dog asks the shepherd for.
///
/// Narrow on purpose: eight methods cover the whole plan, and each is written
/// against whatever the shepherd itself calls the operation (`shep reload`,
/// `shep restart`, ...) rather than against this crate's own vocabulary for
/// it, so a reader who knows shep already knows what each one does.
pub trait Daemon {
    /// The dog's own `[dog.<name>]` section, as TOML text.
    ///
    /// Empty when `shep.toml` has no such section, which is the ordinary
    /// case for a dog running on its defaults.
    ///
    /// # Errors
    /// [`Error::Connect`] or [`Error::Request`] if the shepherd cannot be
    /// reached or refuses, and [`Error::Protocol`] if it answers with
    /// something other than a dog section.
    // Called by `crate::config::read`, which reads the `[dog.<name>]`
    // section the poll loop needs.
    async fn dog_config(&self, name: &str) -> Result<String, Error>;

    /// Every supervised entry, dogs included.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    // Called only by `adopted_name`, which `crate::config::read` needs to
    // find its own `[dog.<name>]` section.
    async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error>;

    /// Detailed info for one sheep, by exact name.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error>;

    /// Register and start every app in `apps`.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    async fn start(&self, apps: Vec<AppConfig>) -> Result<(), Error>;

    /// Stop and deregister one instance, by its stable numeric id.
    ///
    /// By id and never by name, because the cutover deliberately runs two
    /// instances of one app at once: `Request::Start` on a registered name
    /// ADDS an instance rather than re-registering, so a name selector here
    /// would delete the replacement along with the instance it replaced.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    async fn delete(&self, id: u32) -> Result<(), Error>;

    /// Replace one sheep, by exact name, with a fresh instance of the same
    /// app.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    async fn reload(&self, sheep: &str) -> Result<(), Error>;

    /// Restart one sheep, by exact name.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    // The deploy sequence reloads rather than restarts, so this has no
    // caller at all - see `crate::deploy`'s own doc for the reasoning.
    #[expect(dead_code)]
    async fn restart(&self, sheep: &str) -> Result<(), Error>;

    /// Ask the shepherd to write its muster roll now, and answer with the
    /// path it wrote.
    ///
    /// The snapshot writer debounces, so the roll on disk can lag reality;
    /// `SaveRoll` is documented as bypassing that. The path comes back from
    /// the daemon rather than being rebuilt from
    /// [`ShepPaths`](shep_client::shep_core::paths::ShepPaths) so the dog
    /// agrees with the daemon about where the file is even when the two
    /// resolved `$SHEP_HOME` differently.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    // Called by `crate::roll::registered`, which `crate::survey::survey`
    // calls in turn.
    async fn save_roll(&self) -> Result<PathBuf, Error>;

    /// Attach this dog's short string to `sheep`, for `shep flock` to
    /// paint.
    ///
    /// Ephemeral and owned by this dog: the daemon holds it in memory and
    /// drops it when this process stops, for any reason, which is why the
    /// poll loop republishes on every tick rather than only on change.
    ///
    /// # Errors
    /// As [`Self::dog_config`]. [`Live`]'s own implementation can also fail
    /// before ever asking the shepherd, if `text` cannot become a
    /// [`Smit`] at all - see
    /// [`crate::smit::publish`] for why that is not expected to happen in
    /// the ordinary case.
    async fn set_smit(&self, sheep: &str, text: &str) -> Result<(), Error>;
}

/// A [`Client`] behind the [`Daemon`] trait - the only implementation this
/// crate ships that speaks to a real shepherd.
///
/// `Debug` is derived: the only thing in here is a [`Client`], whose own
/// `Debug` prints its socket path and handshake ack and nothing else.
#[derive(Debug)]
pub struct Live(Client);

impl Live {
    /// Wrap a connected client.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self(client)
    }
}

impl Daemon for Live {
    async fn dog_config(&self, name: &str) -> Result<String, Error> {
        let asked = Request::DogConfig {
            name: name.to_owned(),
        };
        match self.0.request(asked).await? {
            Response::DogSection { toml } => Ok(toml.as_str().to_owned()),
            other => Err(unexpected("DogConfig", &other)),
        }
    }

    async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
        match self.0.request(Request::ListFlock).await? {
            Response::Flock(flock) => Ok(flock),
            other => Err(unexpected("ListFlock", &other)),
        }
    }

    async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
        let asked = Request::Describe {
            selector: SelectorSpec::Name(sheep.to_owned()),
        };
        match self.0.request(asked).await? {
            Response::Described(flock) => Ok(flock),
            other => Err(unexpected("Describe", &other)),
        }
    }

    async fn start(&self, apps: Vec<AppConfig>) -> Result<(), Error> {
        match self.0.request(Request::Start { apps }).await? {
            Response::Started(_) => Ok(()),
            other => Err(unexpected("Start", &other)),
        }
    }

    async fn delete(&self, id: u32) -> Result<(), Error> {
        let asked = Request::Delete {
            selector: SelectorSpec::Id(id),
        };
        match self.0.request(asked).await? {
            Response::Deleted(_) => Ok(()),
            other => Err(unexpected("Delete", &other)),
        }
    }

    async fn reload(&self, sheep: &str) -> Result<(), Error> {
        let asked = Request::Reload {
            selector: SelectorSpec::Name(sheep.to_owned()),
        };
        match self.0.request(asked).await? {
            Response::Reloading(_) => Ok(()),
            other => Err(unexpected("Reload", &other)),
        }
    }

    async fn restart(&self, sheep: &str) -> Result<(), Error> {
        let asked = Request::Restart {
            selector: SelectorSpec::Name(sheep.to_owned()),
        };
        match self.0.request(asked).await? {
            Response::Restarted(_) => Ok(()),
            other => Err(unexpected("Restart", &other)),
        }
    }

    async fn save_roll(&self) -> Result<PathBuf, Error> {
        match self.0.request(Request::SaveRoll).await? {
            Response::RollSaved { path, .. } => Ok(PathBuf::from(path)),
            other => Err(unexpected("SaveRoll", &other)),
        }
    }

    async fn set_smit(&self, sheep: &str, text: &str) -> Result<(), Error> {
        let smit: Smit = text.parse().map_err(Error::Smit)?;
        let asked = Request::SetSmit {
            sheep: sheep.to_owned(),
            smit: Some(smit),
        };
        match self.0.request(asked).await? {
            Response::SmitPainted(_) => Ok(()),
            other => Err(unexpected("SetSmit", &other)),
        }
    }
}

/// The shepherd answered something this dog cannot use.
fn unexpected(asked: &str, got: &Response) -> Error {
    Error::Protocol(format!("{} in answer to {asked}", named(got)))
}

/// Name a response without printing its body.
///
/// `Debug` alone would do for most of these, but `DogSection` routinely
/// carries webhook credentials in the section it wraps, so it is named by
/// hand rather than ever formatted - the same treatment shep-log-rotate
/// gives its own protocol errors.
fn named(response: &Response) -> String {
    match response {
        Response::Flock(flock) => format!("a Flock of {}", flock.len()),
        Response::Described(flock) => format!("a Described of {}", flock.len()),
        Response::Started(flock) => format!("a Started of {}", flock.len()),
        Response::Deleted(ids) => format!("a Deleted of {}", ids.len()),
        Response::Reloading(flock) => format!("a Reloading of {}", flock.len()),
        Response::Restarted(flock) => format!("a Restarted of {}", flock.len()),
        Response::DogSection { .. } => "a DogSection".to_owned(),
        Response::RollSaved { .. } => "a RollSaved".to_owned(),
        Response::SmitPainted(flock) => format!("a SmitPainted of {}", flock.len()),
        // `Response` is `#[non_exhaustive]`, so this arm is not optional. A
        // variant added to the protocol after this dog was written is
        // exactly the one worth naming, and only `Debug` can name it.
        // Truncated, because some of them are listings.
        other => format!("{other:?}").chars().take(60).collect(),
    }
}

/// The name this process was adopted under, or `None` when the flock
/// listing does not name it.
///
/// The pid is the whole of the identification. It is sound because the
/// shepherd spawns an adopted dog directly, so the pid it recorded is this
/// process, and because a pid is unique among running processes. Errors are
/// folded into `None` on purpose: a caller that cannot learn its own name
/// because the shepherd would not answer is in the same position as one
/// adopted under a name the listing does not carry, and has nothing more
/// useful to do with the distinction.
// Called by `crate::config::read`, to find its own `[dog.<name>]` section.
pub async fn adopted_name<D: Daemon>(daemon: &D) -> Option<String> {
    let me = std::process::id();
    daemon
        .list_flock()
        .await
        .ok()?
        .into_iter()
        .find(|info| info.dog.is_some() && info.pid == Some(me))
        .map(|info| info.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_client::shep_core::protocol::DogSource;
    use shep_client::shep_core::status::ProcStatus;

    fn sheep_named(name: &str, pid: Option<u32>) -> ProcessInfo {
        ProcessInfo::builder(0, name, ProcStatus::Online)
            .pid(pid)
            .build()
    }

    fn dog_named(name: &str, pid: Option<u32>) -> ProcessInfo {
        ProcessInfo::builder(0, name, ProcStatus::Online)
            .pid(pid)
            .dog(Some(DogSource::Adopted {
                path: "/usr/local/bin/shep-deploy".to_owned(),
            }))
            .build()
    }

    struct Flock(Vec<ProcessInfo>);

    impl Daemon for Flock {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            unimplemented!()
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            Ok(self.0.clone())
        }
        async fn describe(&self, _sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            unimplemented!()
        }
        async fn start(&self, _apps: Vec<AppConfig>) -> Result<(), Error> {
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
        async fn save_roll(&self) -> Result<PathBuf, Error> {
            unimplemented!()
        }
        async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), Error> {
            unimplemented!()
        }
    }

    /// A [`Daemon`] that cannot be reached at all - every method answers
    /// the same connection-shaped error, matching what a dropped socket
    /// looks like from the caller's side.
    struct Unreachable;

    impl Daemon for Unreachable {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            Err(Error::Protocol("no session".to_owned()))
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            Err(Error::Protocol("no session".to_owned()))
        }
        async fn describe(&self, _sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            Err(Error::Protocol("no session".to_owned()))
        }
        async fn start(&self, _apps: Vec<AppConfig>) -> Result<(), Error> {
            Err(Error::Protocol("no session".to_owned()))
        }
        async fn delete(&self, _id: u32) -> Result<(), Error> {
            Err(Error::Protocol("no session".to_owned()))
        }
        async fn reload(&self, _sheep: &str) -> Result<(), Error> {
            Err(Error::Protocol("no session".to_owned()))
        }
        async fn restart(&self, _sheep: &str) -> Result<(), Error> {
            Err(Error::Protocol("no session".to_owned()))
        }
        async fn save_roll(&self) -> Result<PathBuf, Error> {
            Err(Error::Protocol("no session".to_owned()))
        }
        async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), Error> {
            Err(Error::Protocol("no session".to_owned()))
        }
    }

    /// fails if `named` starts printing a `DogSection`'s body. That
    /// section routinely carries webhook credentials - a bark dog's URL is
    /// the ordinary case - and this error text goes wherever the dog's
    /// stderr goes. `Debug` would print the whole table, which is why this
    /// arm is written by hand while the rest could have been derived.
    ///
    /// The assertion is exact rather than "does not contain the secret"
    /// (IR-41): a formatting change that leaked half a line would still
    /// pass a contains-check written against one fixture.
    #[test]
    fn a_dog_section_is_named_and_never_printed() {
        let secret = "url = \"https://hooks.example.com/T00/B11/xoxb-not-a-real-token\"\n";
        let response = Response::DogSection {
            toml: secret.to_owned().into(),
        };
        assert_eq!(named(&response), "a DogSection");
    }

    /// fails if a roll's own absolute path starts showing up in an error
    /// message by way of the `#[non_exhaustive]` `Debug` fallback. There is
    /// nothing secret in a roll's path, but naming it by hand keeps this
    /// response in the same "named, not printed" family as the others.
    #[test]
    fn a_roll_saved_is_named_and_never_printed() {
        let response = Response::RollSaved {
            path: "/srv/shep/flock.json".to_owned(),
            apps: 3,
        };
        assert_eq!(named(&response), "a RollSaved");
    }

    /// fails if a response stops being named by what it is and how much of
    /// it there is. The count is what makes these useful: "a Flock of 0" in
    /// answer to `Describe` says the selector matched nothing, which reads
    /// very differently from "a Flock of 12".
    #[test]
    fn a_listing_is_named_with_its_length() {
        let flock = vec![sheep_named("web", None), sheep_named("api", None)];
        assert_eq!(named(&Response::Flock(flock.clone())), "a Flock of 2");
        assert_eq!(
            named(&Response::Described(flock.clone())),
            "a Described of 2"
        );
        assert_eq!(named(&Response::Started(flock.clone())), "a Started of 2");
        assert_eq!(
            named(&Response::Reloading(flock.clone())),
            "a Reloading of 2"
        );
        assert_eq!(named(&Response::Restarted(flock)), "a Restarted of 2");
        assert_eq!(named(&Response::Deleted(vec![7, 8])), "a Deleted of 2");
        assert_eq!(named(&Response::Flock(Vec::new())), "a Flock of 0");
    }

    /// fails if the `#[non_exhaustive]` arm stops naming a variant this dog
    /// was written before, or stops truncating it. That arm has only
    /// `Debug` to work with, and some of those variants carry listings.
    #[test]
    fn an_unknown_response_is_named_from_debug_and_truncated() {
        let response = Response::DogStarted(sheep_named("metrics", Some(1)));
        let shown = named(&response);
        assert!(shown.starts_with("DogStarted"), "{shown}");
        assert!(shown.chars().count() <= 60, "{shown}");
    }

    /// fails if `unexpected` stops saying which request the answer was to.
    /// "a Flock of 2" alone does not tell an operator which call went
    /// wrong, and this dog makes eight different ones.
    #[test]
    fn an_unexpected_answer_names_the_request_it_answered() {
        let err = unexpected("Reload", &Response::Flock(Vec::new()));
        let shown = err.to_string();
        assert!(shown.contains("in answer to Reload"), "{shown}");
        assert!(shown.contains("a Flock of 0"), "{shown}");
    }

    /// fails if the dog cannot work out the name it was adopted under.
    /// A dog is spawned with no argv and one env entry, so the ONLY way it
    /// learns its own `[dog.<name>]` key is to find its own pid in the flock.
    /// Getting this wrong means every config lookup silently reads an empty
    /// section, which looks exactly like running on defaults.
    #[tokio::test]
    async fn the_dog_finds_its_own_name_by_pid() {
        let me = std::process::id();
        let flock = Flock(vec![
            sheep_named("web", Some(me + 1)),
            dog_named("deploy", Some(me)),
        ]);
        assert_eq!(adopted_name(&flock).await.as_deref(), Some("deploy"));
    }

    /// fails if a pid match on a SHEEP is mistaken for the dog itself.
    #[tokio::test]
    async fn a_sheep_sharing_the_pid_is_not_the_dog() {
        let me = std::process::id();
        let flock = Flock(vec![sheep_named("web", Some(me))]);
        assert_eq!(adopted_name(&flock).await, None);
    }

    /// fails if a dog at some OTHER pid is mistaken for this one. This is
    /// the case that guards the filter's pid half: a fixture with a dog
    /// entry but no matching pid, where the two tests above cannot tell a
    /// correct filter from `dog.is_some()` alone, because neither of them
    /// puts a dog at the wrong pid.
    #[tokio::test]
    async fn a_dog_at_another_pid_is_not_this_dog() {
        let flock = Flock(vec![dog_named("metrics", Some(std::process::id() + 1))]);
        assert_eq!(adopted_name(&flock).await, None);
    }

    /// fails if a shepherd that will not answer at all yields anything
    /// other than "no name". `adopted_name` folds every error into `None`
    /// through `.ok()?`; this is the case that exercises that fold.
    #[tokio::test]
    async fn a_shepherd_that_will_not_answer_yields_no_name() {
        assert_eq!(adopted_name(&Unreachable).await, None);
    }
}
