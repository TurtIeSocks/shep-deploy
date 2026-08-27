//! `shep-deploy`: a deploy dog for shep.
//!
//! Watches a git branch, builds a release, swaps to it, and rolls back if it
//! does not come up. This file is the binary entry; the deploy sequence
//! itself lives in [`deploy`], and the layout it works over in [`paths`].
//!
//! # Two invocation modes
//!
//! Supervised, the dog is spawned with no argv at all and one environment
//! entry, `SHEP_HOME` - that is the dog contract, and it is why
//! [`daemon::adopted_name`] exists. The poll loop that mode runs is not
//! built yet.
//!
//! Run directly, it takes a verb:
//!
//! ```text
//! shep-deploy deploy <sheep>
//! shep-deploy deploy <sheep> --watch auto|manual
//! shep-deploy setup <sheep>
//! shep-deploy survey
//! ```
//!
//! `--watch` changes one setting and returns without deploying. See
//! [`deploy::set_watch`]. `survey` reports where every registered sheep
//! stands and touches nothing; see [`survey::survey`]. `setup` takes a
//! sheep over: it builds the deploy tree and its first release, then
//! re-registers the sheep against `current` and removes the instances it
//! replaced. See [`optin::prepare`] and [`optin::cut_over`] - and note that
//! it is the one deploy that may have downtime, and the one that is not
//! verified against the app's readiness probe.

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!(
    "shep-deploy is Unix only. This is deliberate rather than an oversight: the deploy model is \
     rename(2) over a symlink, the build's privilege drop is a uid and a gid, and both are Unix \
     concepts this crate uses directly rather than through a portability layer. Windows support \
     is planned and will be scoped separately."
);

mod build;
mod config;
mod daemon;
mod deploy;
mod error;
mod flockfile;
mod git;
mod optin;
mod paths;
mod restore;
mod retention;
mod roll;
mod shared;
mod smit;
mod state;
mod survey;
mod swap;
mod verify;

use std::path::PathBuf;
use std::process::ExitCode;

use shep_client::Client;
use shep_client::shep_core::paths::ShepPaths;

use crate::daemon::Live;
use crate::deploy::Outcome;
use crate::error::Error;
use crate::paths::Tree;
use crate::state::{State, Watch};

/// What a direct invocation accepts. Printed on anything else.
const USAGE: &str = "\
usage: shep-deploy <verb> [args]

  deploy <sheep> [--watch auto|manual]   deploy one sheep, or set how it is watched
  setup <sheep>                          take a sheep over
  survey                                 report where every registered sheep stands
  on-remove                              lifecycle hook; shep runs this itself

Adopted as `deploy`, the same verbs run as `shep deploy <verb> [args]`, and
`shep deploy <sheep>` deploys one sheep directly. A sheep whose name is one of
the verbs above is reached with the verb spelled out: `shep deploy deploy survey`.";

/// The exit code for a deploy that was rolled back.
///
/// shep's own taxonomy (`docs/specs/shep-v1.md` section 9) runs from 0 to 11
/// and this is the next free number, claimed rather than invented: a
/// rollback is a cause shep has no code for, and every cause this dog
/// shares with shep uses shep's number for it. A script must be able to
/// tell three outcomes apart, deployed, cleanly reverted, and broke, and
/// two of those were the same code until Rin ruled otherwise.
const ROLLED_BACK: u8 = 12;

/// A cutover that landed and then could not tidy up: the sheep is live on the
/// new release, and something after the swap failed.
///
/// Its own code rather than the generic 1, because a script has to tell three
/// outcomes apart: it worked, it worked and needs tidying, it failed. The
/// poll loop is why that matters. Unattended, a generic failure here reads as
/// a deploy that did not land, and it would retry one that did.
const STRANDED: u8 = 13;

/// What a parsed argv means to do.
///
/// Split out of `main` so the routing decision - which pattern wins when
/// several could match - is testable on its own. A match that both decides
/// and acts can only be exercised by actually running the binary.
#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    /// No argv at all: the supervised poll loop, per the dog contract.
    Poll,
    /// shep's on-remove lifecycle hook: put every sheep back.
    OnRemove,
    /// Report where every registered sheep stands.
    Survey,
    /// Deploy the named sheep.
    Deploy(&'a str),
    /// Take the named sheep over, up to and including the cutover.
    Setup(&'a str),
    /// Set the named sheep's watch mode.
    Watch { sheep: &'a str, mode: &'a str },
    /// Nothing above matched; print [`USAGE`] and exit on the usage code.
    Usage,
}

/// Decides what an argv means, without acting on it.
///
/// Verb forms are matched before the passthrough arms, deliberately: shep's
/// dog passthrough strips this dog's own name, so `shep deploy koji` and
/// `shep-deploy koji` both arrive as `["koji"]`, indistinguishable from a
/// sheep whose name really is a verb. Checking verbs first means a sheep
/// named `survey` needs the explicit form, `shep deploy deploy survey`,
/// which is the escape hatch [`USAGE`] documents rather than a silent trap.
fn route<'a>(args: &[&'a str]) -> Route<'a> {
    match args {
        [] => Route::Poll,
        ["on-remove"] => Route::OnRemove,
        ["survey"] => Route::Survey,
        ["setup", sheep] => Route::Setup(sheep),
        ["deploy", sheep] => Route::Deploy(sheep),
        ["deploy", sheep, "--watch", mode] => Route::Watch { sheep, mode },
        // Reached only through `shep deploy <sheep>`: the passthrough
        // shipped in shep 0.1.1 and strips the dog's own name, so the
        // flagship command arrives as a bare sheep name with no verb.
        // Last, so a verb always wins.
        [sheep] => Route::Deploy(sheep),
        [sheep, "--watch", mode] => Route::Watch { sheep, mode },
        _ => Route::Usage,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let outcome = match route(&args) {
        Route::Poll => {
            println!("shep-deploy: the poll loop is not implemented yet");
            return ExitCode::SUCCESS;
        }
        // Its own connection, matching every sibling verb, rather than
        // reaching for a `daemon` that does not exist in this scope. It
        // returns an `ExitCode` directly, not a `Result<u8, Error>`, because
        // it never fails outward - see `on_remove`'s own doc.
        Route::OnRemove => return on_remove().await,
        Route::Survey => survey_once().await,
        Route::Deploy(sheep) => deploy_once(sheep).await,
        Route::Setup(sheep) => setup_once(sheep).await,
        Route::Watch { sheep, mode } => set_watch(sheep, mode),
        Route::Usage => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    match outcome {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("shep-deploy: {err}");
            ExitCode::from(code_for(&err))
        }
    }
}

/// The exit code for a run that finished, reporting what it finished as.
const fn code_for_outcome(outcome: &Outcome) -> u8 {
    match outcome {
        Outcome::UpToDate | Outcome::Deployed { .. } => 0,
        Outcome::RolledBack { .. } => ROLLED_BACK,
    }
}

/// The exit code for a run that failed.
///
/// Every arm but the first is shep's own number for the same cause, so an
/// operator reading a dog's status does not have to learn a second
/// vocabulary. Anything with no more specific cause is 1, which is shep's
/// rule as well.
fn code_for(err: &Error) -> u8 {
    match err {
        Error::RolledBack { .. } => ROLLED_BACK,
        Error::Stranded { .. } => STRANDED,
        Error::Config(_) => 4,
        Error::Connect(_) => 5,
        _ => 1,
    }
}

/// One deploy of `sheep`, reporting what it did.
///
/// # Errors
/// Whatever [`deploy::deploy`] returns, plus [`Error::Io`] if the target's
/// `deploy.toml` cannot be read and [`Error::Connect`] if the shepherd's
/// socket cannot be reached.
async fn deploy_once(sheep: &str) -> Result<u8, Error> {
    let tree = Tree::for_sheep(&shep_home()?, sheep);
    let mut state = State::read(&tree.state_file())?;

    let client = Client::connect(&socket()?).await?;
    let daemon = Live::new(client);

    let keep = config::read(&daemon).await?.retention;
    let outcome = deploy::deploy(&daemon, &tree, &mut state, keep).await?;
    match &outcome {
        Outcome::UpToDate => println!("{sheep} is up to date at {}", deployed(&state)),
        Outcome::Deployed { sha } => println!("{sheep} deployed {sha}"),
        Outcome::RolledBack { to, why } => println!("{sheep} rolled back to {to}: {why}"),
    }

    Ok(code_for_outcome(&outcome))
}

/// Takes `sheep` over: builds its deploy tree and first release, then cuts
/// it over to `current`.
///
/// Prints where the sheep now deploys from, because that path is the one
/// thing an operator has no other way to learn - nothing in `shep flock`
/// names it, and it is where every later release lands.
///
/// # Errors
/// [`Error::Io`] if `$SHEP_HOME` cannot be resolved, [`Error::Connect`] if
/// the shepherd's socket cannot be reached, and whatever
/// [`optin::prepare`] or [`optin::cut_over`] return. [`Error::Stranded`] is
/// the one that is returned AFTER a success is printed: the cutover landed
/// and the instances it replaced did not all go.
async fn setup_once(sheep: &str) -> Result<u8, Error> {
    let client = Client::connect(&socket()?).await?;
    let daemon = Live::new(client);

    let prepared = optin::prepare(&daemon, &shep_home()?, sheep).await?;
    // Read before `cut_over` consumes `prepared`.
    let current = prepared.tree.current();

    match optin::cut_over(&daemon, prepared).await {
        Ok(sha) => {
            println!("{sheep} now deploys from {}, at {sha}", current.display());
            Ok(0)
        }
        // The cutover landed and only the cleanup did not, so the operator
        // still needs the path - it is the one thing this command tells
        // them that nothing else will - and then the error says what is
        // left to remove by hand.
        Err(err @ Error::Stranded { .. }) => {
            println!("{sheep} now deploys from {}", current.display());
            Err(err)
        }
        Err(err) => Err(err),
    }
}

/// Reports where every registered sheep stands, and touches nothing.
///
/// # Errors
/// [`Error::Io`] if `$SHEP_HOME` cannot be resolved, and whatever
/// [`survey::survey`] returns.
async fn survey_once() -> Result<u8, Error> {
    let client = Client::connect(&socket()?).await?;
    let daemon = Live::new(client);

    print!("{}", survey::survey(&daemon, &shep_home()?).await?);
    Ok(0)
}

/// The on-remove hook. shep runs this argv before forgetting the dog, under
/// a timeout, and proceeds regardless of the outcome.
///
/// ALWAYS exits 0, including when a sheep could not be restored and
/// including when the shepherd cannot be reached at all. An operator asking
/// to remove something is entitled to have it removed, and a nonzero exit
/// here would be a dog arguing about its own uninstallation. Failures are
/// named in the report instead, which is the output shep pipes to them and
/// the only thing they see about any of this.
async fn on_remove() -> ExitCode {
    let Ok(home) = shep_home() else {
        return ExitCode::SUCCESS;
    };
    // Through `socket()` rather than a second `home.join(...)`: joining by
    // hand here was a second place that knew the control socket's layout,
    // and a change to it would have had to land in both without anything
    // catching the drift.
    let Ok(socket) = socket() else {
        return ExitCode::SUCCESS;
    };
    match Client::connect(&socket).await {
        Ok(client) => {
            let daemon = Live::new(client);
            print!("{}", restore::report(&restore::all(&daemon, &home).await));
        }
        // Nothing was restored and nothing was broken. Said plainly,
        // because silence here is indistinguishable from success.
        Err(err) => println!(
            "no sheep were restored: the shepherd could not be reached ({err}). Any sheep this \
             dog took over is still running from its deploy tree under {}.",
            home.join("deploy").display()
        ),
    }
    ExitCode::SUCCESS
}

/// Sets `sheep`'s watch mode and returns, without deploying.
///
/// # Errors
/// [`Error::Config`] if `mode` is neither `auto` nor `manual`, or if `auto`
/// was asked for on a tree the cutover never landed on - see
/// [`deploy::set_watch`]. [`Error::Io`] if `deploy.toml` cannot be read or
/// written.
fn set_watch(sheep: &str, mode: &str) -> Result<u8, Error> {
    let watch = match mode {
        "auto" => Watch::Auto,
        "manual" => Watch::Manual,
        other => {
            return Err(Error::Config(format!(
                "--watch takes auto or manual, not {other:?}"
            )));
        }
    };

    let tree = Tree::for_sheep(&shep_home()?, sheep);
    let mut state = State::read(&tree.state_file())?;
    let was = state.watch;

    deploy::set_watch(&tree, &mut state, watch)?;

    if was == watch {
        println!("{sheep} was already watch = {}", named(watch));
    } else {
        println!(
            "{sheep} watch: {} -> {}, still deployed at {}",
            named(was),
            named(watch),
            deployed(&state)
        );
    }

    Ok(0)
}

/// `$SHEP_HOME`, absolute.
///
/// Absolute at the point of reading, deliberately: every path this crate
/// builds is joined onto this one, and [`crate::swap::point_at`] writes some
/// of them into symlink targets. A symlink target is resolved against the
/// symlink's own directory rather than this process's working directory, so
/// a relative `SHEP_HOME` would produce links that dangle silently -
/// exactly the failure [`crate::shared::link_into`] canonicalises to avoid,
/// one module over. [`std::path::absolute`] rather than `canonicalize`
/// because this may run before the tree exists, and because resolving
/// through symlinks in `$SHEP_HOME` itself is not this dog's business.
///
/// # Errors
/// [`Error::Io`] if the path cannot be made absolute, which needs the
/// current directory to be readable.
fn shep_home() -> Result<PathBuf, Error> {
    let home = std::env::home_dir().unwrap_or_default();
    let resolved = ShepPaths::resolve(&|key| std::env::var(key).ok(), &home).home;

    std::path::absolute(&resolved).map_err(|source| Error::Io {
        path: resolved,
        source,
    })
}

/// The shepherd's control socket, from the same layout as [`shep_home`].
///
/// # Errors
/// As [`shep_home`].
fn socket() -> Result<PathBuf, Error> {
    Ok(shep_home()?.join("run").join("shep.sock"))
}

/// The sha a target is deployed at, for a message.
fn deployed(state: &State) -> &str {
    state.deployed.as_deref().unwrap_or("nothing yet")
}

/// A [`Watch`] as an operator spells it on the command line.
const fn named(watch: Watch) -> &'static str {
    match watch {
        Watch::Auto => "auto",
        Watch::Manual => "manual",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a rolled-back deploy stops being distinguishable from a
    /// hard failure by exit status alone. A script running
    /// `shep deploy web && notify` has three outcomes to tell apart:
    /// deployed, rejected and cleanly reverted, and broke. Collapsing the
    /// middle one into either neighbour is what makes "reports a success it
    /// did not achieve" possible, which is the species of every serious
    /// finding in the engine plan.
    #[test]
    fn a_rollback_has_its_own_code_distinct_from_failure_and_success() {
        let rolled_back = Error::RolledBack {
            to: "old-sha".to_owned(),
            source: Box::new(Error::Build { status: Some(1) }),
        };
        assert_eq!(code_for(&rolled_back), ROLLED_BACK);
        assert_ne!(ROLLED_BACK, 0);
        assert_ne!(
            code_for(&rolled_back),
            code_for(&Error::Build { status: Some(1) })
        );
    }

    /// fails if a cutover that landed and then could not tidy up is reported
    /// as an ordinary failure. The sheep IS live on the new release; only the
    /// cleanup after the swap did not finish. The poll loop is why the
    /// distinction has to survive into the exit status: unattended, a generic
    /// failure here reads as a deploy that never landed, and it would redeploy
    /// one that did.
    #[test]
    fn a_stranded_cutover_is_neither_a_success_nor_an_ordinary_failure() {
        let stranded = Error::Stranded {
            sheep: "web".to_owned(),
            sha: "abc1234".to_owned(),
            ids: vec![3],
        };
        assert_eq!(code_for(&stranded), STRANDED);
        assert_ne!(STRANDED, 0);
        assert_ne!(
            code_for(&stranded),
            code_for(&Error::Build { status: Some(1) })
        );
        assert_ne!(code_for(&stranded), ROLLED_BACK);
    }

    /// fails if this dog stops joining shep's own exit-code taxonomy and
    /// starts inventing numbers. These four are shep's, from
    /// docs/specs/shep-v1.md section 9, and an operator who has learned that
    /// 5 means "no daemon answered" should not have to learn a second
    /// meaning for it because a dog chose differently.
    #[test]
    fn the_shared_causes_use_sheps_own_numbers() {
        assert_eq!(code_for(&Error::Config("bad".to_owned())), 4);
        assert_eq!(code_for(&Error::Protocol("odd".to_owned())), 1);
        assert_eq!(
            code_for(&Error::Git {
                command: "git fetch".to_owned(),
                status: Some(128),
                stderr: String::new(),
            }),
            1
        );
        assert_eq!(code_for(&Error::Build { status: Some(3) }), 1);
        assert_eq!(
            code_for(&Error::Connect(shep_client::ConnectError::HandshakeClosed)),
            5
        );
    }

    /// fails if a rollback that happened on the ORDINARY path stops being
    /// reported. `Outcome::RolledBack` is the common trigger, a verify that
    /// timed out, and `Error::RolledBack` is the rarer one where something
    /// failed after the reload. Both mean the requested deploy did not
    /// happen, so both take the same code; only this one is reached through
    /// `Ok`.
    #[test]
    fn the_ok_rollback_path_reports_the_same_code() {
        let outcome = Outcome::RolledBack {
            to: "old-sha".to_owned(),
            why: "it did not come up".to_owned(),
        };
        assert_eq!(code_for_outcome(&outcome), ROLLED_BACK);
        assert_eq!(code_for_outcome(&Outcome::UpToDate), 0);
        assert_eq!(
            code_for_outcome(&Outcome::Deployed {
                sha: "new".to_owned()
            }),
            0
        );
    }

    /// fails if a bare sheep name stops routing to a deploy, or if it
    /// starts shadowing a verb. `shep deploy koji` is the flagship command
    /// and arrives here as `["koji"]`, with no verb, because the
    /// passthrough strips the dog's own name.
    #[test]
    fn a_bare_name_is_a_deploy_and_a_verb_still_wins() {
        assert_eq!(route(&["koji"]), Route::Deploy("koji"));
        assert_eq!(route(&["deploy", "koji"]), Route::Deploy("koji"));
        assert_eq!(route(&["survey"]), Route::Survey);
        assert_eq!(route(&["setup", "koji"]), Route::Setup("koji"));
        // The escape hatch for a sheep whose name is a verb.
        assert_eq!(route(&["deploy", "survey"]), Route::Deploy("survey"));
        assert_eq!(route(&[]), Route::Poll);
    }

    /// fails if `on-remove` stops routing to its own hook, or starts being
    /// swallowed by the bare-name catch-all - a sheep really could be named
    /// `on-remove`, and it gets the same escape hatch every other verb
    /// does.
    #[test]
    fn on_remove_routes_to_its_own_hook() {
        assert_eq!(route(&["on-remove"]), Route::OnRemove);
        assert_eq!(route(&["deploy", "on-remove"]), Route::Deploy("on-remove"));
    }
}
