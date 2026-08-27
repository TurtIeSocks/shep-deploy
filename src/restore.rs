//! On removal, put every sheep back where it ran before this dog took over.
//!
//! shep runs [`all`] as this dog's own `on-remove` hook, once, before
//! forgetting it. There is no next tick and no retry beyond what is written
//! here, so a failure has to be loud rather than swallowed.
//!
//! # The failure this prevents
//!
//! An operator rehomes the dog, goes back to `~/ReactMap` because that is
//! where they think their app lives, restarts the sheep, and cannot work
//! out why nothing updates. That sheep's `cwd` was never `~/ReactMap`; it
//! was a path under `$SHEP_HOME` they have no reason to know about.
//! [`State::origin_cwd`] and [`State::origin_script`], captured once at
//! opt-in, are where this module puts it back.
//!
//! # Two cases, both answered from `deploy.toml`
//!
//! A sheep that pre-existed the dog has `origin_cwd` and `origin_script`
//! and is restored to them. A sheep the dog bootstrapped has neither, so
//! there is nothing to restore: it is left running from `current`,
//! unchanged, and the report says so plainly. Deleting an app because a
//! deploy tool was uninstalled would be far worse than leaving it.
//!
//! # Why there is a fallback, and why one outcome is worse than the others
//!
//! [`Request::Delete`] is stop plus deregister, and `FlockRegistry::roll`
//! drops a name with no live instance, so delete-then-start is destructive
//! if the start is refused: the sheep would be gone from the flock AND the
//! roll, not returning on a reboot. A refused restore is retried with the
//! config the shepherd had a moment ago, which covers the common causes -
//! a transient refusal, a bad `origin_script`, a `user` that no longer
//! resolves - at the cost of one extra request. Only when that fallback
//! also fails is the sheep genuinely gone, and [`Restored::Lost`] says so
//! in words an operator cannot misread as "still running".
//!
//! [`Request::Delete`]: shep_client::shep_core::protocol::Request::Delete

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use shep_client::shep_core::config::AppConfig;

use crate::daemon::Daemon;
use crate::error::Error;
use crate::paths::{self, Tree};
use crate::roll;
use crate::state::State;

/// What became of one sheep during removal.
#[derive(Debug)]
pub enum Restored {
    /// Put back at its own checkout, from before this dog took over.
    Returned {
        /// The sheep restored.
        sheep: String,
        /// Where it was put back to.
        to: PathBuf,
    },
    /// The dog bootstrapped this sheep, so there was nowhere to restore it
    /// to. Left running from `current`, unchanged.
    LeftRunning {
        /// The sheep left running.
        sheep: String,
        /// Where it is still running from.
        from: PathBuf,
    },
    /// Nothing was changed. The sheep is still registered and running,
    /// exactly as it was before this call.
    Failed {
        /// The sheep that could not be restored.
        sheep: String,
        /// Why.
        why: String,
    },
    /// The restore itself was refused, but the fallback put the shepherd's
    /// own previous config back.
    ///
    /// Distinct from [`Self::Failed`] because it is NOT "left as it is":
    /// the delete already ran and `start(vec![current])` then succeeded, so
    /// the sheep was stopped and a fresh instance started. The registered
    /// config an operator reads in `shep flock` now matches what it was a
    /// moment ago, but nothing mid-flight survived the restart to get
    /// there.
    Reset {
        /// The sheep that could not be restored, but was put back running.
        sheep: String,
        /// Why the restore itself failed.
        why: String,
    },
    /// At least one delete already landed for this sheep before another
    /// one refused, so there is still at least one instance to account
    /// for.
    ///
    /// Its own variant because neither of the other failure shapes is true
    /// of it: `Failed`'s "left as it is" is false - at least one instance
    /// really is gone - and `Lost`'s "gone from the flock" over-claims,
    /// since instances the loop never reached may still be serving. What
    /// this crate cannot know from here is whether the registered config
    /// survived: `Delete` is stop plus deregister, and the roll drops a
    /// name with no live instance, so a delete that lands on the LAST
    /// remaining instance really does deregister it. Only `shep describe`
    /// and `shep flock` can say.
    PartlyDeleted {
        /// The sheep left in this state.
        sheep: String,
        /// Why the delete that stopped partway failed.
        why: String,
    },
    /// The delete succeeded, both the restore and the fallback failed, and
    /// the sheep is now gone from the flock AND from the roll.
    Lost {
        /// The sheep that is gone.
        sheep: String,
        /// Why neither the restore nor the fallback could put it back.
        why: String,
    },
    /// The muster roll itself could not be read, so none of these
    /// pre-existing sheep could be checked against it at all.
    ///
    /// One row for the whole run rather than one per sheep: the roll is a
    /// single read shared by every target, so its failure is a dog-wide
    /// condition, not a property of any one of them. Repeating "sheep is no
    /// longer registered" once per target would also bury the real cause -
    /// [`roll::read`](crate::roll)'s own crafted, actionable message about
    /// a shepherd newer than this dog - behind a guess this module made up
    /// because it never got to check.
    RollUnreadable {
        /// Every pre-existing sheep this run could not check.
        sheep: Vec<String>,
        /// [`roll::registered`](crate::roll)'s own failure.
        why: String,
    },
}

/// Puts every target back, and answers with one row per target.
///
/// Never returns a `Result`, and that is the contract rather than a
/// convenience: an operator asking to remove something is entitled to have
/// it removed, so a failure here becomes a row in the report and the
/// process still exits 0. A dog that refused to be uninstalled because one
/// of five sheep would not restart would be worse than one that did nothing
/// at all.
pub async fn all<D: Daemon>(daemon: &D, shep_home: &Path) -> Vec<Restored> {
    let Ok(names) = paths::targets(shep_home) else {
        return Vec::new();
    };
    // Read once for every target rather than once per target: it costs a
    // SaveRoll round trip, and a removal is not the moment to make N of
    // them. Kept as a `Result` rather than `.unwrap_or_default()`: an empty
    // map here is indistinguishable from "nothing is registered", and
    // `put_back` would report every pre-existing sheep as "no longer
    // registered" - a fabricated cause standing in for whatever
    // `roll::registered` actually failed with.
    let registered = roll::registered(daemon).await;

    let mut results = Vec::new();
    // Pre-existing sheep whose restore needs the roll, deferred here rather
    // than reported as they are found: a roll that cannot be read is one
    // failure shared by every one of them, not N separate ones.
    let mut blocked_by_roll = Vec::new();

    for sheep in names {
        let tree = Tree::for_sheep(shep_home, &sheep);
        let state = match State::read(&tree.state_file()) {
            Ok(state) => state,
            Err(err) => {
                results.push(Restored::Failed {
                    sheep,
                    why: err.to_string(),
                });
                continue;
            }
        };

        // Nothing to restore means the dog bootstrapped this sheep, so it
        // is left running and TOLD about. Deleting an app because a deploy
        // tool was uninstalled would be much worse than leaving it. This
        // needs no roll at all, so a roll that failed to read does not
        // touch it.
        let (Some(cwd), Some(script)) = (state.origin_cwd, state.origin_script) else {
            results.push(Restored::LeftRunning {
                sheep,
                from: tree.current(),
            });
            continue;
        };

        let Ok(registered) = &registered else {
            blocked_by_roll.push(sheep);
            continue;
        };

        results.push(
            match put_back(daemon, &sheep, registered, &cwd, &script).await {
                PutBack::Done => Restored::Returned { sheep, to: cwd },
                PutBack::Untouched(err) => Restored::Failed {
                    sheep,
                    why: err.to_string(),
                },
                PutBack::Reset(err) => Restored::Reset {
                    sheep,
                    why: err.to_string(),
                },
                PutBack::PartlyDeleted(err) => Restored::PartlyDeleted {
                    sheep,
                    why: err.to_string(),
                },
                PutBack::Deleted(err) => Restored::Lost {
                    sheep,
                    why: err.to_string(),
                },
            },
        );
    }

    if let Err(err) = &registered
        && !blocked_by_roll.is_empty()
    {
        results.push(Restored::RollUnreadable {
            sheep: blocked_by_roll,
            why: err.to_string(),
        });
    }

    results
}

/// What happened to one sheep, distinguishing "nothing changed" from
/// "it is deleted", because those need different words in the report.
enum PutBack {
    /// Re-registered at its own checkout.
    Done,
    /// Nothing happened: the restore was never attempted, or was refused
    /// before anything about the sheep changed.
    Untouched(Error),
    /// The restore was refused, but the fallback re-registered the config
    /// the shepherd already had. NOT the same claim as `Untouched`: the
    /// delete already ran, so the sheep was stopped and a fresh instance
    /// started to get back to that config.
    Reset(Error),
    /// At least one delete already landed before another one refused.
    /// NOT `Untouched` (at least one instance really is gone) and NOT
    /// `Deleted` either - whether the registered config survived depends
    /// on whether the deletes that landed included the last live instance,
    /// which this loop does not track.
    PartlyDeleted(Error),
    /// The delete landed and neither the restore nor the fallback did.
    Deleted(Error),
}

/// Re-registers one sheep against the `cwd` and `script` it ran with
/// before this dog took over.
///
/// Delete THEN start, and the order is tested. `Request::Start` on an
/// already-registered name adds an instance rather than re-registering it,
/// so starting first would leave the sheep running from both places at
/// once, which is the same fact the cutover is built on. Here that order
/// also leaves a CLEAN roll, unlike the cutover's: the delete drops the
/// name, so the following `Start` re-records against a name with no stale
/// entry behind it.
///
/// # Why there is a fallback
///
/// `Delete` is stop plus deregister, and the roll drops a name with no live
/// instance, so a refused `Start` here leaves the sheep gone from the flock
/// AND the roll, not returning on a reboot. The fallback re-registers the
/// config the shepherd had a moment ago, which costs one request on the
/// transient failures that are the common case. Only when that fails too is
/// the sheep genuinely gone, and the caller says so in those words.
async fn put_back<D: Daemon>(
    daemon: &D,
    sheep: &str,
    registered: &BTreeMap<String, AppConfig>,
    cwd: &Path,
    script: &str,
) -> PutBack {
    let Some(current) = registered.get(sheep).cloned() else {
        return PutBack::Untouched(Error::Config(format!(
            "{sheep} is no longer registered, so there is nothing to put back"
        )));
    };

    let mut restored = current.clone();
    restored.cwd = Some(cwd.display().to_string());
    restored.script = script.to_owned();

    let live = match daemon.describe(sheep).await {
        Ok(live) => live,
        Err(err) => return PutBack::Untouched(err),
    };
    let mut any_delete_landed = false;
    for info in &live {
        if let Err(err) = daemon.delete(info.id).await {
            // The discriminator is whether a PRIOR delete already landed,
            // not whether this one failed: a refusal on the very first
            // instance with nothing behind it has changed nothing at all,
            // and is `Untouched` exactly as before this variant existed. A
            // refusal after at least one delete succeeded is the case
            // `Untouched` cannot describe truthfully - see
            // `PutBack::PartlyDeleted`'s doc.
            return if any_delete_landed {
                PutBack::PartlyDeleted(err)
            } else {
                PutBack::Untouched(err)
            };
        }
        any_delete_landed = true;
    }

    match daemon.start(vec![restored]).await {
        Ok(()) => PutBack::Done,
        Err(err) => {
            // The sheep is deregistered at this point. Put the shepherd's
            // own config back rather than leaving it deleted, because a
            // refused restore is usually a bad origin_script or a user that
            // no longer resolves, and the config that was working a moment
            // ago still is.
            if daemon.start(vec![current]).await.is_ok() {
                PutBack::Reset(err)
            } else {
                PutBack::Deleted(err)
            }
        }
    }
}

/// The report shep's hook pipes to the operator, which is the whole of what
/// they see about this.
#[must_use]
pub fn report(results: &[Restored]) -> String {
    results
        .iter()
        .map(|result| match result {
            Restored::Returned { sheep, to } => {
                format!("{sheep} restored to {}\n", to.display())
            }
            // Rin's condition for accepting the leave-running case at all.
            // Without this line, "left running, unchanged" is
            // indistinguishable from "quietly abandoned somewhere you will
            // not think to look", which is the failure this whole module
            // exists to prevent.
            Restored::LeftRunning { sheep, from } => {
                format!("{sheep} still running from {}\n", from.display())
            }
            Restored::Failed { sheep, why } => {
                format!("{sheep} could not be restored and was left as it is: {why}\n")
            }
            // NOT the same wording as `Failed`: this sheep was stopped and
            // a fresh instance started to get its own previous config back,
            // so nothing mid-flight survived even though the config an
            // operator reads now matches what it was a moment ago.
            Restored::Reset { sheep, why } => format!(
                "{sheep} could not be restored ({why}), so its previous configuration was put \
                 back instead - doing that stopped it and started it again, so it is running \
                 with the same config as before but nothing mid-flight survived\n"
            ),
            // Neither "left as it is" nor "gone from the flock" is true
            // here, so this gets wording that says exactly that, and sends
            // the operator to the one place that can say what is really
            // running: `shep describe`.
            Restored::PartlyDeleted { sheep, why } => format!(
                "{sheep}: only SOME of its instances could be stopped before a delete failed \
                 ({why}), so it is neither fully running nor fully removed. Run `shep describe \
                 {sheep}` to see what is actually still there, and `shep flock` for whether it \
                 is still registered, before assuming either.\n"
            ),
            // The row an operator must not misread. Every other outcome
            // leaves a running app; this one does not, and "could not be
            // restored" would have them assume it did.
            Restored::Lost { sheep, why } => format!(
                "{sheep} IS NO LONGER REGISTERED: restoring it failed ({why}) and so did \
                 putting its previous configuration back, so it is stopped and gone from the \
                 flock. It will not come back on its own after a restart. Re-register it from \
                 its own Flockfile.\n"
            ),
            // One row naming every affected sheep and the roll's own
            // cause, rather than one wrong-shaped guess per sheep.
            Restored::RollUnreadable { sheep, why } => format!(
                "{}: none of these could be checked against the muster roll, so none of them \
                 could be restored ({why})\n",
                sheep.join(", ")
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;

    use shep_client::RequestError;
    use shep_client::shep_core::protocol::{ProcessInfo, RpcError, RpcErrorCode};
    use shep_client::shep_core::status::ProcStatus;

    use super::*;
    use crate::state::{Verify, Watch};

    /// Writes a `deploy.toml` for `sheep` recording `origin_cwd` and
    /// `origin_script`, as opt-in would have.
    fn write_target_with_origin(home: &Path, sheep: &str, origin_cwd: &str, origin_script: &str) {
        let tree = Tree::for_sheep(home, sheep);
        fs::create_dir_all(tree.state_file().parent().expect("has a parent"))
            .expect("create target dir");
        let state = State {
            remote: "https://example.com/x".to_owned(),
            branch: "main".to_owned(),
            deployed: Some("a1b2c3d".to_owned()),
            failed: None,
            verify: Verify::default(),
            watch: Watch::default(),
            origin_cwd: Some(PathBuf::from(origin_cwd)),
            origin_script: Some(origin_script.to_owned()),
            checkout: PathBuf::from(origin_cwd),
        };
        state.write(&tree.state_file()).expect("write state");
    }

    /// Writes a `deploy.toml` for `sheep` with no `origin_cwd` or
    /// `origin_script`, as a dog-bootstrapped sheep has.
    fn write_target_with_origin_absent(home: &Path, sheep: &str) {
        let tree = Tree::for_sheep(home, sheep);
        fs::create_dir_all(tree.state_file().parent().expect("has a parent"))
            .expect("create target dir");
        let state = State {
            remote: "https://example.com/x".to_owned(),
            branch: "main".to_owned(),
            deployed: Some("a1b2c3d".to_owned()),
            failed: None,
            verify: Verify::default(),
            watch: Watch::default(),
            origin_cwd: None,
            origin_script: None,
            checkout: PathBuf::from("/srv/deploy-tree"),
        };
        state.write(&tree.state_file()).expect("write state");
    }

    /// How many of a `Recording`'s `start` calls get refused.
    enum Refuse {
        Never,
        FirstOnly,
        Always,
    }

    /// A [`Daemon`] double naming a fixed set of already-registered sheep.
    ///
    /// `save_roll` answers with a roll naming every sheep it was
    /// constructed with, `cwd` under the deploy tree and `script` an
    /// arbitrary placeholder - what the shepherd is presumed to have had
    /// registered before this dog's removal began - unless it was built
    /// [`Self::with_unreadable_roll`], which writes a roll no shepherd of
    /// this crate's `shep-core` could have written, so `roll::registered`
    /// fails with its own real, crafted cause rather than this double
    /// inventing one.
    ///
    /// `describe` answers [`Self::instances`] running instances for
    /// whichever name is asked, refusing outright if
    /// [`Self::describe_fails`]. `delete` succeeds until the
    /// [`Self::delete_fails_at`]th call, then refuses every one after -
    /// `Some(1)` refuses the very first delete; a higher number lets some
    /// instances go before one fails partway through.
    ///
    /// `calls()` records only `delete` and `start` calls that actually
    /// landed, in order: those are the two that change the flock, and the
    /// ordering this pins is between them. `save_roll` and `describe` are
    /// reads and are not recorded, so a reordering of those does not break
    /// the assertion.
    struct Recording {
        sheep: Vec<&'static str>,
        refuse: Refuse,
        instances: u32,
        describe_fails: bool,
        delete_fails_at: Option<u32>,
        unreadable_roll: bool,
        calls: RefCell<Vec<&'static str>>,
        starts: RefCell<Vec<AppConfig>>,
        attempts: Cell<usize>,
        delete_attempts: Cell<u32>,
    }

    impl Recording {
        fn new(sheep: &[&'static str], refuse: Refuse) -> Self {
            Self {
                sheep: sheep.to_vec(),
                refuse,
                instances: 1,
                describe_fails: false,
                delete_fails_at: None,
                unreadable_roll: false,
                calls: RefCell::new(Vec::new()),
                starts: RefCell::new(Vec::new()),
                attempts: Cell::new(0),
                delete_attempts: Cell::new(0),
            }
        }

        /// Every registered sheep accepts every start.
        fn with_registered(sheep: &[&'static str]) -> Self {
            Self::new(sheep, Refuse::Never)
        }

        /// The very first `start` call, across every sheep, is refused;
        /// every later one is accepted.
        fn refusing_first_start_only(sheep: &[&'static str]) -> Self {
            Self::new(sheep, Refuse::FirstOnly)
        }

        /// Every `start` call is refused, for every sheep.
        fn refusing_every_start(sheep: &[&'static str]) -> Self {
            Self::new(sheep, Refuse::Always)
        }

        /// `save_roll` answers with a roll `roll::registered` cannot parse,
        /// naming no particular sheep - a roll failure is dog-wide, so
        /// nothing here is keyed to the sheep the test writes to disk.
        fn with_unreadable_roll() -> Self {
            let mut this = Self::new(&[], Refuse::Never);
            this.unreadable_roll = true;
            this
        }

        /// `describe` refuses outright, for every sheep. `put_back` cannot
        /// learn what to delete without it, so nothing is ever deleted or
        /// started.
        fn refusing_describe(sheep: &[&'static str]) -> Self {
            let mut this = Self::new(sheep, Refuse::Never);
            this.describe_fails = true;
            this
        }

        /// A single instance is described, and its only `delete` refuses.
        /// Nothing lands before the failure, so the sheep is fully running
        /// and fully registered - the ordinary shape a refused delete
        /// takes, and the one that must stay `Failed` rather than
        /// `PartlyDeleted`, which needs a delete to have already landed.
        fn refusing_first_delete_only(sheep: &[&'static str]) -> Self {
            let mut this = Self::new(sheep, Refuse::Never);
            this.delete_fails_at = Some(1);
            this
        }

        /// Two instances are described; the first `delete` lands, the
        /// second refuses. This is the only shape any test here gives more
        /// than one instance, deliberately: it is what exercises the branch
        /// where a delete fails PARTWAY through, with at least one instance
        /// already gone and another one still to account for.
        fn refusing_delete_partway(sheep: &[&'static str]) -> Self {
            let mut this = Self::new(sheep, Refuse::Never);
            this.instances = 2;
            this.delete_fails_at = Some(2);
            this
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.borrow().clone()
        }

        fn started(&self) -> Vec<AppConfig> {
            self.starts.borrow().clone()
        }
    }

    impl Daemon for Recording {
        async fn dog_config(&self, _name: &str) -> Result<String, Error> {
            unimplemented!()
        }
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
            unimplemented!()
        }
        async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
            if self.describe_fails {
                return Err(Error::Request(RequestError::Rpc(RpcError {
                    code: RpcErrorCode::Internal,
                    message: "describe refused".to_owned(),
                })));
            }
            Ok((0..self.instances)
                .map(|offset| {
                    ProcessInfo::builder(offset + 1, sheep, ProcStatus::Online)
                        .pid(Some(1000 + offset))
                        .build()
                })
                .collect())
        }
        async fn start(&self, apps: Vec<AppConfig>) -> Result<(), Error> {
            let attempt = self.attempts.get();
            self.attempts.set(attempt + 1);
            let refused = match self.refuse {
                Refuse::Never => false,
                Refuse::FirstOnly => attempt == 0,
                Refuse::Always => true,
            };
            // Recorded whether accepted or refused: `started()` is what a
            // test asserts the config a call was ATTEMPTED with, including
            // the refused restore itself.
            self.starts.borrow_mut().extend(apps);
            if refused {
                return Err(Error::Request(RequestError::Rpc(RpcError {
                    code: RpcErrorCode::Internal,
                    message: "refused".to_owned(),
                })));
            }
            // `calls()` tracks only what actually changed the flock, so a
            // refused start does not appear here.
            self.calls.borrow_mut().push("start");
            Ok(())
        }
        async fn delete(&self, _id: u32) -> Result<(), Error> {
            let attempt = self.delete_attempts.get() + 1;
            self.delete_attempts.set(attempt);
            if self.delete_fails_at == Some(attempt) {
                return Err(Error::Request(RequestError::Rpc(RpcError {
                    code: RpcErrorCode::Internal,
                    message: "delete refused".to_owned(),
                })));
            }
            self.calls.borrow_mut().push("delete");
            Ok(())
        }
        async fn reload(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
        async fn restart(&self, _sheep: &str) -> Result<(), Error> {
            unimplemented!()
        }
        async fn save_roll(&self) -> Result<PathBuf, Error> {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.keep().join("flock.json");
            if self.unreadable_roll {
                // An unknown field is what a newer shepherd would actually
                // write - `AppConfig` is `deny_unknown_fields` - and is what
                // makes `roll::read` produce its own crafted, actionable
                // message rather than this double inventing one.
                fs::write(
                    &path,
                    "{\"apps\":[{\"app\":{\"name\":\"w\",\"a_field_from_the_future\":1}}]}",
                )
                .expect("write roll");
                return Ok(path);
            }
            let apps: Vec<String> = self
                .sheep
                .iter()
                .map(|name| {
                    format!(
                        "{{\"app\":{{\"name\":{name:?},\"script\":\"the-shepherds-own-script\",\
                         \"cwd\":\"/srv/deploy-tree/current\"}}}}"
                    )
                })
                .collect();
            fs::write(&path, format!("{{\"apps\":[{}]}}", apps.join(","))).expect("write roll");
            Ok(path)
        }
        async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), Error> {
            unimplemented!()
        }
    }

    /// fails if a sheep that pre-existed the dog is not put back where its
    /// operator will look for it. This is the whole point: they will go to
    /// ~/ReactMap, because that is where they think their app lives, and
    /// the cwd it has been running under is a path beneath $SHEP_HOME they
    /// have no reason to know about.
    #[tokio::test]
    async fn a_pre_existing_sheep_goes_back_to_its_own_checkout() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
        let daemon = Recording::with_registered(&["bpm"]);

        let results = all(&daemon, home.path()).await;

        assert!(matches!(results[0], Restored::Returned { .. }));
        let started = daemon.started();
        assert_eq!(started[0].cwd.as_deref(), Some("/srv/reactmap"));
        assert_eq!(started[0].script, "bun .");
    }

    /// fails if the restore stops deleting the old registration first. The
    /// registered config is what has to change, and `Start` on a registered
    /// name ADDS an instance rather than re-registering it, so without the
    /// delete the sheep ends up running from both places at once.
    #[tokio::test]
    async fn the_old_registration_is_removed_before_the_new_one_is_started() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
        let daemon = Recording::with_registered(&["bpm"]);

        all(&daemon, home.path()).await;

        assert_eq!(
            daemon.calls(),
            vec!["delete", "start"],
            "deleting after starting would leave two registrations"
        );
    }

    /// fails if a sheep the dog bootstrapped is deleted, or is left without
    /// being told about. Deleting an app because a deploy tool was
    /// uninstalled would be much worse than leaving it, and "left running,
    /// unchanged" that nobody is told about is indistinguishable from
    /// "quietly abandoned somewhere you will not think to look".
    #[tokio::test]
    async fn a_bootstrapped_sheep_is_left_running_and_named_in_the_report() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin_absent(home.path(), "ctm");
        let daemon = Recording::with_registered(&["ctm"]);

        let results = all(&daemon, home.path()).await;

        assert!(daemon.calls().is_empty(), "nothing is stopped or started");
        let text = report(&results);
        assert!(text.contains("ctm still running from"), "{text}");
        assert!(text.contains("deploy/ctm/current"), "{text}");
    }

    /// fails if one target's failure stops the others being restored, or
    /// stops the removal. An operator asking to remove something is
    /// entitled to have it removed, and a dog that refused to be
    /// uninstalled because one of five sheep would not restart would be
    /// worse than one that did nothing at all.
    #[tokio::test]
    async fn a_failure_is_reported_and_the_rest_still_run() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "aaa", "/srv/a", "./a");
        write_target_with_origin(home.path(), "zzz", "/srv/z", "./z");
        let daemon = Recording::refusing_first_start_only(&["aaa", "zzz"]);

        let results = all(&daemon, home.path()).await;

        assert_eq!(results.len(), 2);
        // `Reset`, not `Lost`: the fallback re-registered what the shepherd
        // already had, so "aaa" is still running. A double that refused
        // EVERY start would give `Lost` here, which is a different claim and
        // has its own test below.
        assert!(
            matches!(results[0], Restored::Reset { .. }),
            "{:?}",
            results[0]
        );
        assert!(matches!(results[1], Restored::Returned { .. }));
        assert!(report(&results).contains("aaa"), "the failure is named");
    }

    /// fails if a refused restore leaves the sheep deleted when it did not
    /// have to be. `Delete` is stop plus deregister and the roll drops a
    /// name with no live instance, so the window between the delete and a
    /// refused `Start` is one where the sheep is gone from both. The
    /// fallback re-registers what the shepherd had a moment ago, which is
    /// the right answer for the common causes: a transient refusal, a bad
    /// origin_script, a `user` that no longer resolves.
    #[tokio::test]
    async fn a_refused_restore_puts_the_shepherds_own_config_back() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
        let daemon = Recording::refusing_first_start_only(&["bpm"]);

        let results = all(&daemon, home.path()).await;

        assert!(
            matches!(results[0], Restored::Reset { .. }),
            "{:?}",
            results[0]
        );
        assert_eq!(daemon.started().len(), 2, "the restore, then the fallback");
        assert_eq!(
            daemon.started()[1].cwd.as_deref(),
            Some("/srv/deploy-tree/current"),
            "the fallback re-registers what the shepherd had, not the restore"
        );
    }

    /// fails if a rescued restore is reported with wording that claims
    /// nothing happened. The delete already ran and `start(vec![current])`
    /// then succeeded, so the sheep was stopped and a fresh instance
    /// started - not "left as it is" in any sense an operator would
    /// recognise, even though the registered config now matches what it
    /// was a moment ago.
    #[tokio::test]
    async fn a_rescued_restore_is_worded_as_a_reset_not_left_alone() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
        let daemon = Recording::refusing_first_start_only(&["bpm"]);

        let results = all(&daemon, home.path()).await;

        let text = report(&results);
        assert!(!text.contains("left as it is"), "{text}");
        assert!(text.contains("stopped it and started it again"), "{text}");
    }

    /// fails if a sheep that really has been deleted is reported as merely
    /// "could not be restored". Every other outcome here leaves a running
    /// app; this one does not, and an operator reading the gentler wording
    /// would assume theirs was still up. This is the one row in the whole
    /// report that has to be alarming.
    #[tokio::test]
    async fn a_sheep_left_deleted_says_so_in_those_words() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
        let daemon = Recording::refusing_every_start(&["bpm"]);

        let results = all(&daemon, home.path()).await;

        assert!(
            matches!(results[0], Restored::Lost { .. }),
            "{:?}",
            results[0]
        );
        let text = report(&results);
        assert!(text.contains("NO LONGER REGISTERED"), "{text}");
        assert!(text.contains("will not come back"), "{text}");
    }

    /// fails if the deploy tree is removed. It is not the dog's to delete,
    /// and in the bootstrap case a running app is still pointing into it,
    /// so deleting it would take down an app during an uninstall.
    #[tokio::test]
    async fn the_deploy_tree_is_left_on_disk() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
        all(&Recording::with_registered(&["bpm"]), home.path()).await;
        assert!(home.path().join("deploy/bpm/deploy.toml").is_file());
    }

    /// fails if a muster roll that cannot be read produces a separate,
    /// fabricated "not registered" row per pre-existing sheep instead of
    /// one row naming all of them and the roll's own real cause. An
    /// operator meeting N copies of a wrong guess has less to act on than
    /// one line naming the actual reason - here, `roll::read`'s "newer
    /// shepherd" message - and the affected sheep.
    #[tokio::test]
    async fn a_roll_read_failure_is_reported_once_dog_wide_with_the_real_cause() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "aaa", "/srv/a", "./a");
        write_target_with_origin(home.path(), "zzz", "/srv/z", "./z");
        let daemon = Recording::with_unreadable_roll();

        let results = all(&daemon, home.path()).await;

        assert_eq!(
            results.len(),
            1,
            "one row for the whole roll failure, not one per sheep: {results:?}"
        );
        assert!(matches!(results[0], Restored::RollUnreadable { .. }));
        let text = report(&results);
        assert!(text.contains("aaa"), "{text}");
        assert!(text.contains("zzz"), "{text}");
        assert!(text.contains("newer"), "{text}");
        assert!(
            daemon.calls().is_empty(),
            "nothing was deleted or started without a readable roll"
        );
    }

    /// fails if a shepherd that refuses `describe` outright is reported as
    /// anything other than `Failed`, or if the refusal is silently
    /// swallowed into `Done`. `put_back` cannot learn what to delete
    /// without this call, so nothing about the sheep changes.
    #[tokio::test]
    async fn a_describe_that_is_refused_leaves_the_sheep_untouched() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
        let daemon = Recording::refusing_describe(&["bpm"]);

        let results = all(&daemon, home.path()).await;

        assert!(
            matches!(results[0], Restored::Failed { .. }),
            "{:?}",
            results[0]
        );
        assert!(daemon.calls().is_empty(), "nothing was deleted or started");
    }

    /// fails if a `delete` that fails PARTWAY through a multi-instance
    /// sheep is reported as anything other than its own `PartlyDeleted`, or
    /// if the loop keeps deleting instances after one refuses. This is the
    /// single most dangerous path in the file: some instances may already
    /// be gone and others still live, and neither `Failed`'s "left as it
    /// is" nor `Lost`'s "gone from the flock" is a true sentence about it -
    /// only `shep describe` can say what is really there.
    #[tokio::test]
    async fn a_delete_that_fails_partway_through_is_reported_as_partly_deleted() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
        let daemon = Recording::refusing_delete_partway(&["bpm"]);

        let results = all(&daemon, home.path()).await;

        assert!(
            matches!(results[0], Restored::PartlyDeleted { .. }),
            "{:?}",
            results[0]
        );
        assert_eq!(
            daemon.calls(),
            vec!["delete"],
            "the first delete lands before the second refuses, and nothing after it runs"
        );
        assert!(
            daemon.started().is_empty(),
            "start is never reached once a delete fails"
        );
    }

    /// fails if a partly-deleted sheep is worded as though it were either
    /// fully running or fully removed. Neither `Failed`'s "left as it is"
    /// nor `Lost`'s "gone from the flock" is true here, and this pins the
    /// two true things the report can say: check `shep describe` for what
    /// is actually there, and `shep flock` for whether it is still
    /// registered - this crate cannot know that from here, since `Delete`
    /// landing on the LAST remaining instance really does deregister it.
    #[tokio::test]
    async fn a_partly_deleted_sheep_is_worded_as_neither_running_nor_removed() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
        let daemon = Recording::refusing_delete_partway(&["bpm"]);

        let results = all(&daemon, home.path()).await;

        let text = report(&results);
        assert!(!text.contains("left as it is"), "{text}");
        assert!(!text.contains("gone from the flock"), "{text}");
        assert!(!text.contains("never touched"), "{text}");
        assert!(
            text.contains("neither fully running nor fully removed"),
            "{text}"
        );
        assert!(text.contains("shep describe bpm"), "{text}");
        assert!(text.contains("shep flock"), "{text}");
    }

    /// fails if a delete refused on the FIRST instance, with nothing behind
    /// it, is reported as `PartlyDeleted`. Nothing landed: the sheep is
    /// fully running and fully registered, which is exactly `Failed`'s
    /// "left as it is" case. The discriminator for `PartlyDeleted` is
    /// whether a delete already landed, not whether one failed - a
    /// single-instance sheep whose only delete is refused must stay
    /// `Failed`, and this is the ordinary shape a refused delete takes.
    #[tokio::test]
    async fn a_delete_refused_with_nothing_landed_yet_stays_failed() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
        let daemon = Recording::refusing_first_delete_only(&["bpm"]);

        let results = all(&daemon, home.path()).await;

        assert!(
            matches!(results[0], Restored::Failed { .. }),
            "{:?}",
            results[0]
        );
        let text = report(&results);
        assert!(text.contains("left as it is"), "{text}");
        assert!(
            !text.contains("neither fully running nor fully removed"),
            "{text}"
        );
    }

    /// fails if `on-remove` ever turns a partial failure into a nonzero
    /// exit. There is no such branch in `main.rs`'s `on_remove` - this pins
    /// the half of that contract that lives here: a report containing a
    /// `Failed` row still names every other row plainly, so nothing about
    /// this module's own output would justify one.
    #[test]
    fn a_report_with_a_failure_still_names_every_other_row() {
        let results = vec![
            Restored::Failed {
                sheep: "aaa".to_owned(),
                why: "refused".to_owned(),
            },
            Restored::Returned {
                sheep: "zzz".to_owned(),
                to: PathBuf::from("/srv/z"),
            },
        ];
        let text = report(&results);
        assert!(text.contains("aaa"), "{text}");
        assert!(text.contains("zzz"), "{text}");
    }
}
