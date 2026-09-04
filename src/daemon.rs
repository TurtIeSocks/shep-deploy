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
//! There is one real implementation, [`Live`], a thin wrapper over a
//! connected client that turns each method into one [`Request`] and matches
//! the [`Response`] it expects back. Which client it wraps is the one thing
//! the two constructors disagree about, and [`Link`] says why. Test doubles
//! live in each module's own `#[cfg(test)]` block, this one included.
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
//! Do not add trait methods speculatively. These nine are the surface plan
//! one and plan two need between them, not nine that looked useful:
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
//! [`adopted_name`] answers "what did shep register this process as", by
//! finding this process's own pid in the flock listing: it reports a pid per
//! entry and a dog marker per dog, and this process knows its own pid.
//! Getting it wrong means every config lookup silently reads an empty
//! section, which looks exactly like a dog correctly running on defaults -
//! there is no error to notice.
//!
//! shep also puts that name in `$SHEP_DOG_NAME` when it spawns an adopted
//! dog, and [`crate::main`] reads it for the handshake. This lookup stays
//! all the same, because the two answer different questions. The handshake
//! may carry ONLY a name the daemon itself set, never one this process
//! worked out, since a wrong name there has the daemon restart the wrong dog
//! when a refusal arrives. The config section has no such blast radius, and
//! looking it up here keeps working under a shep old enough not to set the
//! variable at all.

use std::path::PathBuf;

use shep_client::{
    Client, LinkState, ReconnectingClient, RequestError,
    shep_core::config::AppConfig,
    shep_core::protocol::{ProcessInfo, Request, Response, SelectorSpec, Smit},
};

use crate::error::Error;

/// Everything this dog asks the shepherd for.
///
/// Narrow on purpose: nine methods cover the whole plan, and each is written
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

    /// Register and start every app in `apps`, answering the ids of the
    /// instances this `Start` registered.
    ///
    /// The ids are the daemon's own answer to "which rows are yours", so a
    /// caller that later has to take exactly those rows down can name them
    /// rather than guess from a listing which rows are new. `Start` is an
    /// acceptance: the rows are registered when this returns and spawned
    /// afterwards, so a `describe` straight after can still be missing them.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    async fn start(&self, apps: Vec<AppConfig>) -> Result<Vec<u32>, Error>;

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

/// A connected client behind the [`Daemon`] trait - the only implementation
/// this crate ships that speaks to a real shepherd.
///
/// `Debug` is derived: the only thing in here is one client, and both
/// clients' own `Debug` print a socket path and a handshake ack and nothing
/// else.
#[derive(Debug)]
pub struct Live(Link);

/// Which kind of connection a [`Live`] speaks over.
///
/// # Why one type with two constructors rather than two types
///
/// The two clients are not interchangeable and must not become so. A
/// [`ReconnectingClient`] built by [`Live::dog`] announces a dog name in its
/// handshake, and that name is what lets the daemon restart THAT dog when a
/// successor refuses it (shep's handover design, G8). A one-shot command
/// that claimed a name would have the real dog restarted because a CLI
/// invocation was refused, so [`Live::command`] takes the plain [`Client`],
/// which has no way to claim one.
///
/// Everything downstream of this module is written against the [`Daemon`]
/// trait and does not care which it got: the deploy sequence is the same
/// sequence either way. So the split is an enum inside one type rather than
/// a type parameter on `Live` or two `Daemon` implementations. A parameter
/// would propagate `Live<C>` into every signature in `main` to express a
/// distinction only the two constructors here need to make, and a second
/// implementation would duplicate nine method bodies that differ in nothing
/// but which field they call `request` on.
#[derive(Debug)]
enum Link {
    /// The supervised dog's connection: reconnects across a daemon
    /// handover, and stops for good when a successor refuses it.
    Dog(ReconnectingClient),
    /// A one-shot command's connection, which outlives nothing.
    Command(Client),
}

impl Live {
    /// Wrap the supervised dog's reconnecting connection.
    ///
    /// The dog process outlives the daemon it connected to - a handover
    /// replaces the shepherd underneath a dog that keeps running - so this
    /// is the only constructor whose connection has to survive one.
    #[must_use]
    pub const fn dog(client: ReconnectingClient) -> Self {
        Self(Link::Dog(client))
    }

    /// Wrap a one-shot command's connection.
    ///
    /// A command runs for one deploy, setup or survey and exits. It has
    /// nothing to reconnect for, and deliberately no way to announce itself
    /// as a dog - see [`Link`].
    #[must_use]
    pub const fn command(client: Client) -> Self {
        Self(Link::Command(client))
    }

    /// What the reconnecting supervisor is doing, or `None` on a one-shot
    /// command connection, which has no supervisor to ask.
    ///
    /// The caller that matters is [`crate::main`]'s poll loop, watching for
    /// [`LinkState::Refused`]: past that point every request fails forever,
    /// and a loop that kept ticking would deploy nothing while looking
    /// exactly like a dog with nothing to do.
    #[must_use]
    pub fn link(&self) -> Option<LinkState> {
        match &self.0 {
            Link::Dog(client) => Some(client.link()),
            Link::Command(_) => None,
        }
    }
}

impl Link {
    /// Send one request over whichever connection this is.
    ///
    /// Both clients spell `request` identically, down to the error type, so
    /// every [`Daemon`] method below is written once and reads the same as
    /// it did when `Live` held a bare [`Client`].
    async fn request(&self, body: Request) -> Result<Response, RequestError> {
        match self {
            Self::Dog(client) => client.request(body).await,
            Self::Command(client) => client.request(body).await,
        }
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

    async fn start(&self, apps: Vec<AppConfig>) -> Result<Vec<u32>, Error> {
        match self.0.request(Request::Start { apps }).await? {
            Response::Started(flock) => Ok(flock.iter().map(|info| info.id).collect()),
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
        // exactly the one worth naming, and only `Debug` can name it. Only
        // the name: `Debug` goes on to print the body, and a variant nobody
        // here has reviewed is exactly the one whose body might carry a
        // credential, as `DogSection`'s does. The first run of identifier
        // characters is the variant's name and nothing else.
        other => format!("{other:?}")
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect(),
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
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            Ok(self.0.clone())
        }

        crate::fixtures::daemon_methods!(unimplemented;
            dog_config, describe, start, delete, reload, restart, save_roll, set_smit,
        );
    }

    /// A [`Daemon`] that cannot be reached at all - every method answers
    /// the same connection-shaped error, matching what a dropped socket
    /// looks like from the caller's side.
    struct Unreachable;

    impl Daemon for Unreachable {
        crate::fixtures::daemon_methods!(
            answering Err(Error::Protocol("no session".to_owned()));
            dog_config, list_flock, describe, start, delete, reload, restart, save_roll, set_smit,
        );
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
    /// wrong, and this dog makes nine different ones.
    #[test]
    fn an_unexpected_answer_names_the_request_it_answered() {
        let err = unexpected("Reload", &Response::Flock(Vec::new()));
        let shown = err.to_string();
        assert!(shown.contains("in answer to Reload"), "{shown}");
        assert!(shown.contains("a Flock of 0"), "{shown}");
    }

    /// fails if the dog cannot work out the name it was adopted under.
    /// A dog is spawned with no argv, so the flock listing is how it finds
    /// its own `[dog.<name>]` key: its own pid, carrying a dog marker.
    /// Getting this wrong means every config lookup silently reads an empty
    /// section, which looks exactly like running on defaults.
    ///
    /// `$SHEP_DOG_NAME` carries the same name and is NOT read here; see this
    /// module's own doc for why the config section and the handshake take
    /// their names from different places.
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
