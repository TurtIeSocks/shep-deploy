//! `shep-deploy`: a deploy dog for shep.
//!
//! Watches a git branch, builds a release, swaps to it, and rolls back if it
//! does not come up. This file is the binary entry; the deploy sequence
//! itself lives in [`deploy`], and the layout it works over in [`paths`].
//!
//! # Two invocation modes
//!
//! Supervised, the dog is spawned with no argv at all and two environment
//! entries: `SHEP_HOME`, which every path below is joined onto, and
//! `SHEP_DOG_NAME`, which is what shep registered this process as. That mode
//! runs [`poll::run`], which deploys every `watch = "auto"` target whose
//! branch has moved, once an interval, until the process is asked to stop.
//!
//! The two modes connect differently, in two ways. The poll loop connects
//! through a client that survives a daemon handover, because it is the only
//! mode that outlives one; a verb connects through a plain client and exits.
//! And the poll loop announces a dog name, but only the one shep put in
//! `SHEP_DOG_NAME` - no variable, no dog, no name. See [`poll_forever`] and
//! [`adopted_as`].
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
/// Test helpers shared by every module's own `mod tests`; see its own doc for
/// why a binary crate needs this declared here.
#[cfg(test)]
mod fixtures;
mod flockfile;
mod git;
mod lock;
mod optin;
mod paths;
mod poll;
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

use shep_client::shep_core::paths::ShepPaths;
use shep_client::{Client, LinkState, RECONNECT_MAX_DELAY, ReconnectingClient};
use tokio::signal::unix::{Signal, SignalKind, signal};

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
    let route = match args {
        [] => Route::Poll,
        ["on-remove"] => Route::OnRemove,
        ["survey"] => Route::Survey,
        // A verb whose sheep is missing, before the bare-name arm below can
        // read the verb itself as a sheep. `shep-deploy setup` used to mean
        // "deploy the sheep called setup", which is a different command
        // against a sheep that almost never exists.
        //
        // A sheep genuinely named `setup` or `deploy` is reached the same way
        // one named `survey` already is, by spelling the verb out:
        // `shep deploy setup`. That is the escape hatch USAGE documents.
        ["setup" | "deploy"] => Route::Usage,
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
    };

    // Every name that reaches a `Tree` comes through here, so this is the one
    // place it has to be a name rather than a path. `Tree::for_sheep` joins it
    // onto `$SHEP_HOME/deploy`, and an absolute name replaces that root
    // outright rather than traversing out of it.
    match route {
        Route::Setup(sheep) | Route::Deploy(sheep) if !paths::is_sheep_name(sheep) => Route::Usage,
        Route::Watch { sheep, .. } if !paths::is_sheep_name(sheep) => Route::Usage,
        named => named,
    }
}

/// How long the runtime waits for blocking work after the main future ends.
///
/// One second. The only blocking work is git and the artifact copy, both on
/// tokio's pool through `shared::off_thread`; a stop that lands during a
/// fetch must not wait the fetch out, which dropping the runtime without a
/// timeout would do. The git child left behind is bounded by `git_timeout`
/// and finishes or dies on its own after the dog has gone.
const DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    // Built by hand rather than through `#[tokio::main]`, which drops the
    // runtime without a timeout and therefore waits for every blocking task
    // to finish: see `DRAIN`.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("shep-deploy: cannot start a runtime: {err}");
            return ExitCode::from(1);
        }
    };
    let code = runtime.block_on(run(&args));
    runtime.shutdown_timeout(DRAIN);
    code
}

/// The whole program past argument parsing, on the runtime `main` built.
async fn run(args: &[&str]) -> ExitCode {
    let outcome = match route(args) {
        Route::Poll => poll_forever().await,
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

/// The environment entry shep puts an adopted dog's registered name in.
///
/// Set by the daemon itself when it spawns the dog, alongside `SHEP_HOME`.
/// Its absence is the signal that this process is NOT a supervised dog; see
/// [`adopted_as`].
const DOG_NAME: &str = "SHEP_DOG_NAME";

/// The supervised mode: connect as this dog, read its own config section,
/// and poll until the process is asked to stop.
///
/// Answers 0 for a stop that was asked for, which is the only way out that
/// is not an error. [`poll::run`] itself never returns.
///
/// # Why this one connection is not a [`Client`]
///
/// This is the only invocation that outlives the daemon it connected to. A
/// `shep upgrade` hands the socket to a successor process, and a dog is an
/// ordinary child that keeps running through it, so its connection has to be
/// re-established or the dog is alive and mute for as long as it is left up.
/// [`ReconnectingClient`] is the client that does that, and
/// `connect_as_dog` is the only public door to a handshake that names a dog.
/// That name is what the daemon marks as handshook, and what it needs to
/// restart the right dog when a successor refuses one. A dog connecting
/// without it serves every request correctly and is still recorded as
/// silent, restarted once, and then written off as stale.
///
/// The four one-shot verbs stay on [`Client`], deliberately. Each runs for
/// one command and exits, so there is nothing to reconnect for, and a
/// command claiming to be this dog would have the daemon record a handshake
/// for a process about to exit - and restart the real dog if the command's
/// own handshake were refused.
///
/// # Errors
/// [`Error::Io`] if `$SHEP_HOME` cannot be resolved, [`Error::Connect`] if
/// the shepherd's socket cannot be reached, [`Error::Refused`] if a
/// successor daemon refuses this dog's handshake, and whatever
/// [`config::read`] returns - a `[dog.<name>]` section that cannot be
/// parsed stops the dog here rather than being ignored, because a dog
/// running on defaults it was not asked for looks exactly like one
/// honouring the config.
///
/// A target's own failure is NOT one of these. It is reported and the loop
/// carries on to the next target; see [`poll::run`].
async fn poll_forever() -> Result<u8, Error> {
    let home = shep_home()?;
    // First, and before anything is awaited. See `Stop`.
    let mut stop = Stop::install();

    let socket = socket()?;
    let client = match adopted_as(&|key| std::env::var(key).ok()) {
        Some(name) => ReconnectingClient::connect_as_dog(&socket, &name).await?,
        None => ReconnectingClient::connect(&socket).await?,
    };
    let daemon = Live::dog(client);
    let config = config::read(&daemon).await?;

    tokio::select! {
        result = poll::run(&daemon, &home, &config) => result.map(|()| 0),
        // Cancels `poll::run` the same way a stop does, and can land inside
        // a deploy for the same reason - see `Stop::arrives`. It costs
        // nothing here that carrying on would not cost anyway: past a
        // refusal every request this loop makes fails, so the deploy it
        // interrupts had already stopped being able to finish.
        refusal = refused(&daemon) => Err(refusal),
        () = stop.arrives() => {
            println!("shep-deploy: stopping");
            Ok(0)
        }
    }
}

/// The name shep registered this process under, or `None` when nothing did.
///
/// This is the handshake identity and nothing else, which is why it has
/// exactly one source. A name reaching the daemon is a claim about which dog
/// to restart when a handshake is refused, so a guessed one - the binary's
/// own stem, the config section's key, a default - gets some other dog
/// restarted for this process's problem. No name at all is a complete and
/// honest answer, and it is the right one for somebody running the binary by
/// hand: they are not a supervised dog, and the daemon should not record a
/// handshake for them.
///
/// Blank is treated as absent. An empty `SHEP_DOG_NAME` names no dog, and
/// announcing it would claim an identity the daemon cannot match. Anything
/// else is passed through verbatim rather than trimmed or validated: the
/// daemon looks this up against its own registry, so the only useful value
/// is the exact one it set.
///
/// Takes its lookup rather than reading the environment directly, matching
/// [`resolved`], so the decision is testable without a process-global
/// `set_var`.
fn adopted_as(env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    env(DOG_NAME).filter(|name| !name.trim().is_empty())
}

/// Resolves when a successor daemon has refused this dog's handshake, and
/// never otherwise.
///
/// A refusal is terminal by design: the supervisor stops, and every later
/// request fails with no chance of recovering. The loop would carry on
/// ticking through that, deploying nothing and reporting each target's
/// failure once, which reads like a broken remote rather than a dog that
/// can no longer talk to its shepherd at all. Ending instead puts the reason
/// in the dog's log once and hands the process back to shep, which knows
/// from the handshake which dog this was and can start it again when it is
/// upgraded.
///
/// Polled rather than awaited, because [`LinkState`] is a snapshot and
/// `shep-client` offers no wakeup for it. [`RECONNECT_MAX_DELAY`] is the
/// supervisor's own ceiling between reconnect attempts, so a refusal is
/// noticed about one attempt after it happens, and the poll costs one
/// read of a lock per five seconds.
async fn refused(daemon: &Live) -> Error {
    loop {
        if let Some(LinkState::Refused {
            daemon_version,
            message,
        }) = daemon.link()
        {
            return Error::Refused {
                daemon_version,
                message,
            };
        }
        tokio::time::sleep(RECONNECT_MAX_DELAY).await;
    }
}

/// The two signals that mean stop, registered.
///
/// `SIGTERM` is what shep sends a dog it is stopping; `SIGINT` is what a
/// terminal sends somebody who started the dog by hand to watch it.
///
/// # Why this is a type, and why it is installed by a plain function
///
/// [`signal`] inside an `async fn` does not run when the future is created.
/// It runs when the future is first polled, and `tokio::select!` polls its
/// branches in an order that is randomised per process, so a handler
/// installed that way does not exist yet on about half of starts. The
/// window is the first tick, which opens with a `git fetch` - and a
/// `SIGTERM` arriving in it kills the process on the signal's default
/// disposition, with no message and nothing else in this file running.
///
/// A non-async `install` cannot be lazy, so the handlers exist before the
/// loop is polled at all.
struct Stop {
    /// The `SIGTERM` stream, or `None` if it could not be installed.
    term: Option<Signal>,
    /// The `SIGINT` stream, on the same terms.
    interrupt: Option<Signal>,
}

impl Stop {
    /// Installs both handlers now.
    fn install() -> Self {
        Self {
            term: listen(SignalKind::terminate()),
            interrupt: listen(SignalKind::interrupt()),
        }
    }

    /// Resolves when either signal arrives, and never otherwise.
    ///
    /// A stream that is absent or closed is not a request to stop, and
    /// returning for one would print "stopping" for a stop nobody asked
    /// for. The signal keeps its default disposition in that case, so a
    /// `SIGTERM` still ends the process - without the tidy message.
    ///
    /// # What a stop does NOT interrupt
    ///
    /// Not a tick boundary: cancelling [`poll::run`] can land inside a
    /// deploy, which is acceptable and documented there. It used to be
    /// deferred while a `git` call was in flight, because those ran through
    /// blocking `std::process::Command` on the runtime's one thread; they
    /// run on tokio's blocking pool now (`shared::off_thread`), so a stop
    /// is answered during a fetch and `main` gives the pool [`DRAIN`] to
    /// wind down rather than waiting the fetch out.
    async fn arrives(&mut self) {
        let asked = match (&mut self.term, &mut self.interrupt) {
            (Some(term), Some(interrupt)) => tokio::select! {
                arrived = term.recv() => arrived,
                arrived = interrupt.recv() => arrived,
            },
            (Some(only), None) | (None, Some(only)) => only.recv().await,
            (None, None) => None,
        };

        if asked.is_none() {
            core::future::pending().await
        }
    }
}

/// One signal handler, or `None` and a complaint if it cannot be installed.
///
/// Registering fails only on a runtime with no I/O driver, which
/// `#[tokio::main]` never builds. It is reported rather than fatal because
/// a dog that cannot catch `SIGTERM` still deploys perfectly well; it just
/// dies less tidily.
fn listen(kind: SignalKind) -> Option<Signal> {
    match signal(kind) {
        Ok(stream) => Some(stream),
        Err(err) => {
            eprintln!("shep-deploy: cannot listen for {kind:?}: {err}");
            None
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
/// The first two arms are this dog's own numbers, for outcomes shep has no
/// code for. Every arm after them is shep's own number for the same cause, so
/// an operator reading a dog's status does not have to learn a second
/// vocabulary. Anything with no more specific cause is 1, which is shep's
/// rule as well.
fn code_for(err: &Error) -> u8 {
    match err {
        Error::RolledBack { .. } => ROLLED_BACK,
        Error::Stranded { .. } => STRANDED,
        // The same number as any other configuration refusal: it is one,
        // from outside. What its own variant buys is inside - see the
        // variant's doc, and `crate::poll::worth_saying`.
        Error::Config(_) | Error::NotCutOver { .. } => 4,
        // A refused handshake takes the same number as a socket that could
        // not be reached, because it is the same thing from an operator's
        // side: this dog and the shepherd are not talking. The message says
        // which of the two it was, and that is where the difference belongs.
        Error::Connect(_) | Error::Refused { .. } => 5,
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
    let daemon = Live::command(client);

    let config = config::read(&daemon).await?;
    let outcome = deploy::deploy(&daemon, &tree, &mut state, &config).await?;
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
    let daemon = Live::command(client);

    let config = config::read(&daemon).await?;
    let prepared = optin::prepare(&daemon, &shep_home()?, sheep, &config).await?;
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
    let daemon = Live::command(client);

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
            let daemon = Live::command(client);
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
/// [`Error::Config`] if `mode` is neither `auto` nor `manual`, if `auto` was
/// asked for on a tree the cutover never landed on, or if `deploy.toml` does
/// not parse or fails validation - see [`deploy::set_watch`]. [`Error::Io`]
/// if it cannot be read or written.
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
    // Both the record before and the record after come back from the write:
    // `set_watch` reads immediately before writing, and a read made here
    // would be the older copy the "was already" line must not be true of.
    let (was, state) = deploy::set_watch(&tree, watch)?;

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
    absolute(resolved().home)
}

/// The shepherd's control socket, from the same layout as [`shep_home`].
///
/// Reads `ShepPaths`'s own `socket` field rather than joining `run/shep.sock`
/// onto the home. Both spellings agree today, and the point is that only one
/// of them is shep's to change: the layout belongs to `shep_core::paths`, and
/// a copy here is a second source of truth that drifts silently the day shep
/// moves the socket.
///
/// # Errors
/// As [`shep_home`].
fn socket() -> Result<PathBuf, Error> {
    absolute(resolved().socket)
}

/// The shep layout, as this process's environment spells it.
fn resolved() -> ShepPaths {
    let home = std::env::home_dir().unwrap_or_default();
    ShepPaths::resolve(&|key| std::env::var(key).ok(), &home)
}

/// One path from [`resolved`], made absolute.
///
/// # Errors
/// [`Error::Io`] if the path cannot be made absolute, which needs the current
/// directory to be readable.
fn absolute(path: PathBuf) -> Result<PathBuf, Error> {
    std::path::absolute(&path).map_err(|source| Error::Io { path, source })
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
        // A tree the cutover never landed on is a configuration problem
        // like any other from outside, whatever the loop makes of it
        // inside: giving it a variant of its own must not give it a number
        // of its own.
        assert_eq!(
            code_for(&Error::NotCutOver {
                sheep: "web".to_owned(),
                tree: PathBuf::from("/srv/shep/deploy/web"),
            }),
            4
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

    /// fails if a supervised dog stops announcing the name shep gave it.
    ///
    /// That name is the whole of the bug this function exists for. Without
    /// it in the handshake the daemon never marks the dog handshook: it
    /// connects, serves every request correctly, and is rendered `silent`,
    /// restarted once after five seconds, then written off as stale and
    /// never started again. Everything an operator can see says the dog is
    /// working.
    #[test]
    fn a_supervised_dog_announces_the_name_shep_gave_it() {
        let env = |key: &str| (key == DOG_NAME).then(|| "log-rotate".to_owned());
        assert_eq!(adopted_as(&env).as_deref(), Some("log-rotate"));
    }

    /// fails if a process nobody supervised claims to be a dog anyway.
    ///
    /// No `SHEP_DOG_NAME` means shep did not spawn this, so there is no dog
    /// to be. The name must never be guessed here - from the binary's stem,
    /// from the `[dog.<name>]` section, from a default - because it is what
    /// the daemon restarts on a refused handshake, and a guess gets some
    /// other dog restarted for a hand run's problem.
    #[test]
    fn a_hand_run_announces_no_name_at_all() {
        assert_eq!(adopted_as(&|_| None), None);
        // Blank names no dog either, and an identity the daemon cannot
        // match is worse than none: it is a claim.
        assert_eq!(adopted_as(&|_| Some(String::new())), None);
        assert_eq!(adopted_as(&|_| Some("   ".to_owned())), None);
    }

    /// fails if the name stops being read from `SHEP_DOG_NAME` specifically.
    /// `SHEP_NAME` and `SHEP_INSTANCE` sit beside it in a dog's environment
    /// and neither is the registered dog name.
    #[test]
    fn only_shep_dog_name_is_read() {
        let env = |key: &str| (key == "SHEP_NAME").then(|| "web".to_owned());
        assert_eq!(adopted_as(&env), None);
    }

    /// fails if a refused handshake stops being an error at all, or starts
    /// taking a number that means something else. It is 5 for the same
    /// reason a dead socket is: this dog and the shepherd are not talking.
    #[test]
    fn a_refused_handshake_exits_on_the_no_daemon_code() {
        let refused = Error::Refused {
            daemon_version: Some("0.4.0".to_owned()),
            message: "protocol mismatch".to_owned(),
        };
        assert_eq!(code_for(&refused), 5);
        assert_eq!(
            code_for(&refused),
            code_for(&Error::Connect(shep_client::ConnectError::HandshakeClosed))
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

    /// fails if the stop handlers stop being installed eagerly. An
    /// `async fn` that calls `signal()` registers on its first poll, not on
    /// creation, and `select!` polls in a randomised order - so lazily
    /// installed handlers do not exist during the first tick on about half
    /// of starts, and that tick opens with a `git fetch`. A `SIGTERM` in
    /// that window kills the dog on the default disposition.
    ///
    /// `install` being a plain function is what makes it eager; this is
    /// what makes it work.
    #[tokio::test]
    async fn both_stop_handlers_are_installed_up_front() {
        let stop = Stop::install();
        assert!(stop.term.is_some(), "SIGTERM");
        assert!(stop.interrupt.is_some(), "SIGINT");
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

    /// fails if a verb with its sheep missing is read as a sheep named after
    /// the verb.
    ///
    /// `shep-deploy setup` fell through the two-element arms to the bare-name
    /// catch-all and became `Deploy("setup")`: a different command, aimed at a
    /// sheep that almost certainly does not exist. Usage is what an operator
    /// who forgot the argument needs, and it is what every other verb already
    /// does.
    #[test]
    fn a_verb_without_its_sheep_is_a_usage_error() {
        assert_eq!(route(&["setup"]), Route::Usage);
        assert_eq!(route(&["deploy"]), Route::Usage);

        // And the escape hatch still reaches a sheep really called that, the
        // same one a sheep named `survey` or `on-remove` uses.
        assert_eq!(route(&["deploy", "setup"]), Route::Deploy("setup"));
        assert_eq!(route(&["deploy", "deploy"]), Route::Deploy("deploy"));
    }

    /// fails if a sheep name that is really a path reaches a `Tree`.
    ///
    /// `Tree::for_sheep` joins the name onto `$SHEP_HOME/deploy`, and
    /// `PathBuf::join` REPLACES the path when given an absolute one. So an
    /// absolute name does not traverse out of the tree, it discards the tree
    /// and roots itself wherever it points, taking everything that later
    /// prunes and removes inside it along.
    #[test]
    fn a_sheep_name_that_is_a_path_is_refused() {
        for name in ["/tmp/anywhere", "../sibling", "a/b", "", ".", ".."] {
            assert_eq!(route(&[name]), Route::Usage, "bare: {name}");
            assert_eq!(route(&["deploy", name]), Route::Usage, "deploy: {name}");
            assert_eq!(route(&["setup", name]), Route::Usage, "setup: {name}");
            assert_eq!(
                route(&[name, "--watch", "auto"]),
                Route::Usage,
                "watch: {name}"
            );
        }
    }
}
