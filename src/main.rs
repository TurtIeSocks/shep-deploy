//! `shep-deploy`: a deploy dog for shep.
//!
//! Watches a git branch, builds a release, swaps to it, and rolls back if it
//! does not come up. This file is the binary entry; the crate is built out
//! task by task starting with [`error`], the one error type the rest of the
//! crate reports through.

#![forbid(unsafe_code)]
// TODO(task-10): remove once `main` calls into the deploy engine and every
// module below has a caller. Until then a tree with no operator command yet
// warns on the whole thing as unused - `error::Error` above all, since
// nothing constructs `Protocol`/`Config` outside its own tests until the
// shepherd session and config parser land. shep-log-rotate hit the identical
// gap in its own Task 1 and carried the same allow to its Task 7.
#![allow(dead_code)]

mod daemon;
mod error;
mod paths;
mod shared;
mod state;

fn main() {
    println!("not yet implemented");
}
