//! The dog's own `[dog.<name>]` section: how often to poll, and how many
//! releases to keep.
//!
//! Deliberately the only two settings here, and deliberately nothing
//! per-target. Per-target state lives in the deploy tree's own
//! `deploy.toml` (see [`crate::state`]) because keying it to the dog's name
//! means renaming or re-adopting the dog destroys the record of every
//! deployment it manages, and those are unrelated things.
//!
//! Both values are refused rather than clamped when they name something
//! that cannot work. That is the same choice
//! [`crate::shared::shepignore_patterns`] makes about a glob and
//! [`crate::deploy`] makes about an ungated `probed` target: a setting that
//! silently does something other than what it says is a setting an operator
//! will believe in for as long as the disk lasts.

use std::time::Duration;

use serde::Deserialize;
use shep_client::shep_core::values::UpDuration;

use crate::daemon::{Daemon, adopted_name};
use crate::error::Error;

/// How often the poll loop looks for new commits, absent a config saying
/// otherwise.
///
/// Thirty seconds, settled in the design spec with its reasoning: adequate
/// for a single host, and it needs no inbound port, no HMAC verification
/// and no public exposure, which is the whole argument for polling over
/// webhooks at this scale.
const DEFAULT_INTERVAL: UpDuration = UpDuration::from_millis(30_000);

/// How many releases retention keeps, absent a config saying otherwise.
const DEFAULT_RETENTION: usize = 5;

/// The fewest releases a target can keep and still be able to roll back.
///
/// Two: the one that is live, and the one before it. Retention keeps the
/// newest N, so N of one prunes the rollback target.
const MINIMUM_RETENTION: usize = 2;

/// The `[dog.<name>]` section, parsed.
///
/// `Debug` is derived: a poll interval and a count are not secrets, and
/// unlike the raw section text this type never holds the whole table. The
/// raw text is a different matter and `crate::daemon::named` already
/// refuses to print it, because a `[dog.<name>]` section routinely carries
/// webhook credentials for other dogs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DogConfig {
    /// How long the poll loop sleeps between ticks.
    pub interval: Duration,
    /// How many releases per target retention keeps.
    pub retention: usize,
}

/// The wire shape, kept separate from [`DogConfig`] so the validated type
/// cannot be constructed without going through [`DogConfig::parse`].
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Raw {
    interval: UpDuration,
    retention: usize,
}

impl Default for Raw {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            retention: DEFAULT_RETENTION,
        }
    }
}

impl DogConfig {
    /// Parses a `[dog.<name>]` section's body.
    ///
    /// An empty string is the ordinary case, not an edge one:
    /// [`Daemon::dog_config`] answers an absent section that way, and a dog
    /// adopted without configuration is the common shape.
    ///
    /// # Errors
    /// [`Error::Config`] if the text is not valid TOML, carries a key this
    /// dog does not know, gives a value of the wrong type, asks for a
    /// retention below two, or asks for a zero interval. The message names
    /// the key in every case, because the section is one an operator edited
    /// by hand.
    pub fn parse(toml: &str) -> Result<Self, Error> {
        let raw: Raw = toml::from_str(toml)
            .map_err(|source| Error::Config(format!("[dog.<name>]: {source}")))?;

        if raw.retention < MINIMUM_RETENTION {
            return Err(Error::Config(format!(
                "retention = {} keeps too few releases to roll back: the release a failed \
                 deploy returns to is the second newest, so anything below {MINIMUM_RETENTION} \
                 prunes the only thing there is to roll back to",
                raw.retention
            )));
        }

        let interval = raw.interval.as_duration();
        if interval.is_zero() {
            return Err(Error::Config(
                "interval = \"0\" would fetch continuously rather than on a schedule, which \
                 reads as a hung dog and hammers the remote it is watching"
                    .to_owned(),
            ));
        }

        Ok(Self {
            interval,
            retention: raw.retention,
        })
    }
}

/// This dog's own section, read from the shepherd.
///
/// A dog that cannot work out the name it was adopted under gets the
/// documented defaults rather than an error, which is
/// [`adopted_name`]'s own contract: not knowing the name and being adopted
/// under a name with no section are the same position from here.
///
/// # Errors
/// Whatever [`Daemon::dog_config`] returns, plus [`Error::Config`] from
/// [`DogConfig::parse`].
// The poll loop that calls this is not wired into `main` until a later
// task; this is that caller's target, not a caller itself yet.
#[allow(dead_code)]
pub async fn read<D: Daemon>(daemon: &D) -> Result<DogConfig, Error> {
    let Some(name) = adopted_name(daemon).await else {
        return DogConfig::parse("");
    };
    let section = daemon.dog_config(&name).await?;
    DogConfig::parse(&section)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if an empty or absent section stops meaning "the documented
    /// defaults". A dog is adopted with no config at all in the ordinary
    /// case, and `Daemon::dog_config` answers an absent section with an
    /// empty string rather than an error, so this is the path almost every
    /// real dog takes.
    #[test]
    fn an_empty_section_is_the_documented_defaults() {
        let config = DogConfig::parse("").expect("an empty section parses");
        assert_eq!(config.interval, Duration::from_secs(30));
        assert_eq!(config.retention, 5);
    }

    /// fails if either key stops being read. Both are the only reason this
    /// module exists, and a config silently running on defaults looks
    /// exactly like a config being honoured.
    #[test]
    fn both_keys_are_read() {
        let config = DogConfig::parse("interval = \"5m\"\nretention = 12").expect("parses");
        assert_eq!(config.interval, Duration::from_secs(300));
        assert_eq!(config.retention, 12);
    }

    /// fails if a retention count below two is accepted. It would silently
    /// disable rollback: retention keeps the newest N releases, and the
    /// rollback target IS the second newest, so `retention = 1` prunes the
    /// only thing a failed deploy can return to. Refused loudly at parse
    /// time rather than clamped, matching how `.shepignore` refuses a glob:
    /// an operator who asked for something that cannot work should be told,
    /// not quietly given something else.
    #[test]
    fn a_retention_below_two_is_refused_by_name() {
        for count in ["0", "1"] {
            let err = DogConfig::parse(&format!("retention = {count}")).expect_err("refuses");
            let shown = err.to_string();
            assert!(shown.contains("retention"), "{shown}");
            assert!(shown.contains("roll back"), "{shown}");
        }
    }

    /// fails if a zero interval is accepted. A poll loop sleeping zero
    /// between ticks fetches continuously, which is a denial of service
    /// against the operator's own git remote and reads as a hung dog.
    #[test]
    fn a_zero_interval_is_refused() {
        let err = DogConfig::parse("interval = \"0s\"").expect_err("refuses");
        assert!(err.to_string().contains("interval"), "{err}");
    }

    /// fails if a typo is ignored instead of refused. `retenton = 2` that
    /// parses to the default of 5 is a config an operator will believe is
    /// in force for as long as the disk lasts. Same reasoning as
    /// `BuildSpec`'s own `deny_unknown_fields`.
    #[test]
    fn an_unknown_key_is_refused_and_named() {
        let err = DogConfig::parse("retenton = 2").expect_err("refuses");
        assert!(err.to_string().contains("retenton"), "{err}");
    }

    /// fails if a wrong-typed value produces a message that does not name
    /// the key. This section is hand-edited in `shep.toml`, and
    /// `retention = "five"` is a plausible thing to write; a bare "invalid
    /// type: string" with no key names nothing an operator can go and fix.
    #[test]
    fn a_value_of_the_wrong_type_names_the_key() {
        let err = DogConfig::parse("retention = \"five\"").expect_err("refuses");
        assert!(err.to_string().contains("retention"), "{err}");
    }
}
