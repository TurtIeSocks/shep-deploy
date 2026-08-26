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
//! Do not add trait methods speculatively. Each of the six here exists
//! because a later task in the plan calls it; a method nothing calls is
//! dead surface with no test to justify it.
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

use shep_client::{
    Client,
    shep_core::config::AppConfig,
    shep_core::protocol::{ProcessInfo, Request, Response, SelectorSpec},
};

use crate::error::Error;

/// Everything this dog asks the shepherd for.
///
/// Narrow on purpose: six methods cover the whole plan, and each is written
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
    async fn dog_config(&self, name: &str) -> Result<String, Error>;

    /// Every supervised entry, dogs included.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
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
    async fn restart(&self, sheep: &str) -> Result<(), Error>;
}

/// A [`Client`] behind the [`Daemon`] trait - the only implementation this
/// crate ships that speaks to a real shepherd.
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
        Response::Reloading(flock) => format!("a Reloading of {}", flock.len()),
        Response::Restarted(flock) => format!("a Restarted of {}", flock.len()),
        Response::DogSection { .. } => "a DogSection".to_owned(),
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
        async fn reload(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
        async fn restart(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
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
}
