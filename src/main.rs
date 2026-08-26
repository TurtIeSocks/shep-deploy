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
//! ```
//!
//! `--watch` changes one setting and returns without deploying. See
//! [`deploy::set_watch`].

#![forbid(unsafe_code)]

mod build;
mod daemon;
mod deploy;
mod error;
mod flockfile;
mod git;
mod paths;
mod shared;
mod state;
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
const USAGE: &str = "usage: shep-deploy deploy <sheep> [--watch auto|manual]";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let outcome = match args.as_slice() {
        // The supervised mode: no argv at all, per the dog contract.
        [] => {
            println!("shep-deploy: the poll loop is not implemented yet");
            return ExitCode::SUCCESS;
        }
        ["deploy", sheep] => deploy_once(sheep).await,
        ["deploy", sheep, "--watch", mode] => set_watch(sheep, mode),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("shep-deploy: {err}");
            ExitCode::FAILURE
        }
    }
}

/// One deploy of `sheep`, reporting what it did.
///
/// # Errors
/// Whatever [`deploy::deploy`] returns, plus [`Error::Io`] if the target's
/// `deploy.toml` cannot be read and [`Error::Connect`] if the shepherd's
/// socket cannot be reached.
async fn deploy_once(sheep: &str) -> Result<(), Error> {
    let tree = Tree::for_sheep(&shep_home()?, sheep);
    let mut state = State::read(&tree.state_file())?;

    let client = Client::connect(&socket()?).await?;
    let daemon = Live::new(client);

    match deploy::deploy(&daemon, &tree, &mut state).await? {
        Outcome::UpToDate => println!("{sheep} is up to date at {}", deployed(&state)),
        Outcome::Deployed { sha } => println!("{sheep} deployed {sha}"),
        Outcome::RolledBack { to, why } => println!("{sheep} rolled back to {to}: {why}"),
    }

    Ok(())
}

/// Sets `sheep`'s watch mode and returns, without deploying.
///
/// # Errors
/// [`Error::Config`] if `mode` is neither `auto` nor `manual`,
/// [`Error::Io`] if `deploy.toml` cannot be read or written.
fn set_watch(sheep: &str, mode: &str) -> Result<(), Error> {
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

    Ok(())
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
