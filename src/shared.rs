//! Deriving which of the operator's checkout files a fresh release needs.
//!
//! A fresh `git worktree` contains nothing git ignores, so it cannot run:
//! ReactMap needs `config/local.json`, a generated masterfile and several
//! more, none of which are in the repository. Something has to put them
//! there, and the rule is the operator's own checkout plays the role a
//! `shared/` directory would otherwise play - see [`to_link`].
//!
//! Three steps, one function each: [`ignored_present`] asks git what it
//! ignores and finds present on disk right now, [`shepignore_patterns`]
//! reads the operator's opt-out list, and [`to_link`] subtracts the second
//! from the first. [`link_into`] is the only function here that writes
//! anything, and it never writes to the checkout - only into a release.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::error::Error;

/// [`run_git`], abandoning the subprocess if it outlives `budget`.
///
/// Only [`crate::git::fetch`] uses this, and only because it is the one git
/// invocation that talks to a network. The other ten callers operate on local
/// directories and cannot hang on a remote that stopped answering.
///
/// Why it matters more than an ordinary slow call: the poll loop deploys
/// targets one at a time, so an unbounded fetch does not fail one target, it
/// stops every target and the smit refresh with it, with no error and no log
/// line. A remote behind a firewall that drops packets rather than refusing
/// them produces exactly that, and it is an ordinary misconfiguration.
///
/// The child is killed on expiry rather than left to finish, because a git
/// process still holding the bare clone's lock would fail the next tick too.
///
/// ## Why the pipes are drained on threads
///
/// A pipe holds about 64 KiB before a writer blocks in `write(2)`. Reading
/// only after the child exits therefore deadlocks any child that says more
/// than that: it blocks writing, `try_wait` reports it still running forever,
/// and the budget kills a process that was healthy and nearly finished.
///
/// Measured 2026-08-28 while reviewing this function's first version, which
/// had exactly that bug: a child writing 200 KB and then exiting was killed at
/// a three-second deadline, having been ready to exit in milliseconds. It
/// would have turned a `git fetch --prune` against a repository with many
/// refs into a target reported as an unreachable remote, which is a worse
/// failure than the hang this exists to prevent.
///
/// So each pipe gets a thread that reads to EOF, which is what
/// `wait_with_output` does internally and the reason it cannot be used here:
/// it also waits, and waiting is what this function needs to bound.
///
/// Residual, stated rather than hidden: this still occupies its caller for up
/// to `budget`, and the poll loop deploys targets one at a time, so a target
/// whose remote is a black hole delays the others by that much. It no longer
/// occupies the RUNTIME: every caller runs it through [`off_thread`], so a
/// stop, a refused handshake or the client's reconnect are all still served
/// while it waits. Bounded and reported beats unbounded and silent, which is
/// the whole of what this buys.
///
/// # Errors
/// [`Error::Git`] naming the command and a `None` status if `budget` elapses
/// first. Otherwise exactly what [`run_git`] returns.
pub(crate) fn run_git_within(dir: &Path, args: &[&str], budget: Duration) -> Result<String, Error> {
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so the whole tree can be signalled rather
        // than just git. See `abandon` for why that is the difference between
        // bounded and not.
        .process_group(0)
        .spawn()
        .map_err(Error::at(dir))?;

    // Taken before the loop so the child always has a reader, whatever the
    // loop then decides about the clock. See the doc above.
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());

    let deadline = Instant::now() + budget;
    let status = loop {
        match child.try_wait() {
            Err(source) => {
                let _ = child.kill();
                return Err(Error::Io {
                    path: dir.to_owned(),
                    source,
                });
            }
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            abandon(&mut child);
            break None;
        }
        // Never sleep past the deadline: `POLL` is 50ms, and a budget smaller
        // than that would otherwise overshoot to 50ms whatever was asked for.
        thread::sleep(POLL.min(left));
    };

    let Some(status) = status else {
        // Returning WITHOUT joining, deliberately. The reader threads exist to
        // keep the child unblocked, and their output is not wanted on this
        // path. Joining here would reintroduce the unbounded wait this whole
        // function exists to remove: a grandchild holding the pipe means the
        // read end never sees EOF, and `abandon` signalling the group makes
        // that unlikely rather than impossible. They finish and drop on their
        // own when the last writer goes.
        return Err(Error::Git {
            command: format!("git {}", args.join(" ")),
            status: None,
            stderr: format!(
                "no answer within {budget:?}; abandoned so the other targets keep deploying"
            ),
        });
    };

    // Bounded, like the wait above and for the same reason. git exiting does
    // not mean the pipe is closed: anything it forked that inherited fd 1 or 2
    // still holds the write end, and an ssh ControlPersist master or a
    // `&`-backgrounded helper in an alias does exactly that while git itself
    // exits 0. Joining unconditionally here made the SUCCESS path the one
    // remaining unbounded wait, which is worse than the failure paths already
    // fixed, because it fires on the ordinary case.
    //
    // Measured 2026-08-28 before this change: a git alias forking
    // `sh -c 'sleep 8 &'` returned Ok after 8.02 seconds against a 600ms
    // budget, having reported success the whole time.
    // Each wait re-reads the clock. Computing the remaining time once and
    // spending it twice is how a two-pipe collect quietly doubles the budget:
    // if the first pipe consumes all of it, the second is handed a fresh copy.
    // Found while re-reading this function on 2026-08-28, having written the
    // same class of overrun into it three times already.
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let (Ok(stdout), Ok(stderr)) = (out.recv_timeout(remaining()), err.recv_timeout(remaining()))
    else {
        // NOT `abandon` here. That signals the process group by negating
        // `child.id()`, which is only safe while the child is alive: by this
        // point `try_wait` has reaped it, and the wait above can have taken
        // the whole budget, which the dog's `[deploy]` section allows to be minutes. A pid
        // recycled in that window would put a SIGKILL into an unrelated
        // process group, and the dog runs as root under the arrangement
        // shep's own docs recommend, so the usual same-uid check would not
        // stop it.
        //
        // Nothing is signalled instead: what holds the pipe is an orphan git
        // left behind, and this process has no safe way to name it.
        //
        // The readers are not simply abandoned, though. Dropping the receivers
        // is what lets them finish: their `send` then fails, which is the
        // documented shape, and the thread returns. Without that they would
        // sit in `read_to_end` for as long as the orphan lives, holding a
        // thread and a pipe fd each. The poll loop runs a fetch per target per
        // tick forever, so a remote that reliably produces such an orphan
        // would leak two of each per tick until the process ran out, taking
        // every other target down with it.
        drop(out);
        drop(err);
        return Err(Error::Git {
            command: format!("git {}", args.join(" ")),
            status: None,
            stderr: format!(
                "exited, but something it started still held its output after \
                 {budget:?}; abandoned so the other targets keep deploying"
            ),
        });
    };

    decode(dir, args, status, stdout, stderr)
}

/// Kills a whole process group by the leader's pid.
///
/// One spelling, shared, because two are what drift. `crate::build` abandons a
/// build the same way and cannot share the rest: its child is a
/// `tokio::process::Child` and this one is a `std::process::Child`, so the
/// fallback differs while the signal and the group form must not.
///
/// `killpg(2)` through `nix` rather than spawning `kill`. The spawn was there
/// because the crate forbids unsafe and `libc::killpg` is unsafe; `nix` wraps
/// the same call safely and is already a dependency. It also removes the one
/// place this crate ran a binary found on `PATH` other than `git`, and the
/// fork it cost on every abandoned build.
///
/// The failure is ignored on purpose. Every caller is already giving up on the
/// child, and a kill that cannot be delivered leaves nothing they can do about
/// it that they have not already decided. A pid too large for `pid_t` is the
/// same case: no such group can exist.
pub(crate) fn kill_group(pid: u32) {
    // Zero is not a group either: `killpg(0)` signals the CALLER's group,
    // which is this dog and every build it has running.
    if pid == 0 {
        return;
    }
    if let Ok(pid) = i32::try_from(pid) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
}

/// `text` with every control character replaced by `?`, for a message that
/// quotes something the deployed repository chose.
///
/// `Display` and `Path::display` pass control bytes through, so a committed
/// `artifacts = ["\u{1b}[2J..."]` would write raw terminal escapes into the
/// dog's log, and a newline would forge a second log line. Replaced rather
/// than escaped, because the reader wants to recognise the entry, not
/// reconstruct it: the file is right there.
pub(crate) fn printable(text: impl std::fmt::Display) -> String {
    text.to_string()
        .chars()
        .map(|c| if forges_a_line(c) { '?' } else { c })
        .collect()
}

/// Whether `c` can change how a log line is read: a control character, or
/// one of the Unicode format and separator characters that reverse, split
/// or hide text (the bidi marks, overrides and isolates, the zero-width and
/// soft-hyphen family, the line and paragraph separators, the byte order
/// mark, and the tag block that carries invisible text). None of them
/// belongs in a file name or a TOML key an operator meant. Shared with
/// `paths::is_sheep_name` and `State::validate`, so what is refused in a
/// name is the same set that is replaced in a message.
pub(crate) fn forges_a_line(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00ad}'
                | '\u{061c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2069}'
                | '\u{feff}'
                | '\u{e0000}'..='\u{e007f}'
        )
}

/// Kills a timed-out child and everything it spawned, then reaps it.
///
/// The group, not just the pid. `Child::kill` signals the immediate process
/// only, and `git fetch` over `ssh://` forks an `ssh` that inherits our pipe
/// write-ends. Killing git alone leaves that `ssh` holding the pipe open, so
/// the read end never sees EOF. Demonstrated 2026-08-28: a reader thread on a
/// pipe a grandchild still holds does not finish, so joining it would hang
/// past the budget, which is the exact failure this function exists to
/// prevent.
///
/// Reaped afterwards: leaving a zombie git would keep the bare clone's lock
/// and fail the next tick for a reason that looks unrelated.
fn abandon(child: &mut std::process::Child) {
    kill_group(child.id());
    // The group signal is the one that matters; this covers a child that
    // left its group before the signal landed.
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads one of a child's pipes to EOF on its own thread.
///
/// A free function rather than a closure because the two pipes are different
/// types and a closure cannot be generic over them.
fn drain<R: io::Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        // The receiver is dropped when the budget runs out, so this send can
        // fail. That is the intended shape, not an error to report.
        let _ = tx.send(buf);
    });
    rx
}

/// Runs `work` on tokio's blocking pool and answers with its result.
///
/// Every git call in this crate is a blocking `std::process::Command`, and
/// the artifact copy is blocking I/O. Run on the runtime's one thread, each
/// stalled everything else polled on it for its whole duration: a five
/// minute fetch against a remote that drops packets meant five minutes in
/// which a `SIGTERM` went unanswered, a refused handshake went unnoticed and
/// the client's reconnect could not run. On the pool, the runtime keeps
/// turning and the caller alone waits.
///
/// A panic inside `work` is re-raised here rather than turned into an
/// error, so it reaches the poll loop's own guard and is reported as the
/// bug it is. The only other way a blocking task ends without a result is
/// the runtime shutting down underneath it, which is the process ending.
///
/// # Errors
/// Whatever `work` returns.
pub(crate) async fn off_thread<T, F>(work: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, Error> + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(join) => match join.try_into_panic() {
            Ok(payload) => std::panic::resume_unwind(payload),
            Err(join) => panic!("a blocking task was cancelled: {join}"),
        },
    }
}

/// `O_NOFOLLOW`, without a libc dependency or an unsafe block.
///
/// The value is fixed by each platform's ABI and cannot change without
/// breaking every compiled program on it. Same "ask the host rather than
/// reimplement its answer" spirit as `crate::build`'s use of `id`, except
/// here the host's answer is a constant. Shared by the artifact copy and the
/// Flockfile read, which both open a path the deployed repository chose.
pub(crate) const fn o_nofollow() -> i32 {
    #[cfg(target_os = "linux")]
    {
        0o400_000
    }
    #[cfg(target_vendor = "apple")]
    {
        0x0100
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        compile_error!("O_NOFOLLOW's value is not known for this target")
    }
}

/// Whether `err` is `ELOOP`: the kernel refusing to follow a symlink, which
/// is what an `O_NOFOLLOW` open answers when the last component is one.
///
/// By errno rather than `io::ErrorKind::FilesystemLoop`, which is not stable
/// yet. `nix` already names the number for each platform.
pub(crate) fn is_eloop(err: &io::Error) -> bool {
    err.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32)
}

/// How often [`run_git_within`] asks whether the child has finished.
///
/// Fifty milliseconds: short enough that a fetch finishing is not perceptibly
/// delayed, long enough that a five-minute budget costs six thousand cheap
/// syscalls rather than a busy loop.
const POLL: Duration = Duration::from_millis(50);

/// Runs `git <args>` in `dir` and returns its stdout as a `String`.
///
/// `pub(crate)` rather than private: [`crate::git`] shells out to git for a
/// second directory this crate cares about - the bare clone under
/// [`crate::paths::Tree::git`] - and needs the exact same error mapping this
/// module already built, so it reuses this rather than growing a second
/// copy. The parameter is named `dir`, not `checkout`, because it is called
/// with both: this module's own functions always pass the operator's
/// checkout, `crate::git`'s pass the deploy engine's own bare clone.
///
/// Launching a subprocess and decoding what it printed are not filesystem
/// calls, but they fail with the same shape of error - an
/// [`std::io::Error`] and a path worth naming - and this crate has nowhere
/// else for that shape to live, so both come back as [`Error::Io`] naming
/// `dir`. A `git` invocation that launches but exits non-zero is
/// [`Error::Git`] instead, since `git`'s own stderr is worth keeping
/// separate from "could not even run it".
///
/// Unbounded on purpose: see [`run_git_within`] for the one caller that
/// cannot afford that, and why the other ten can.
///
/// # Errors
/// [`Error::Io`] naming `dir` if git cannot be launched or printed something
/// that is not UTF-8. [`Error::Git`] if it ran and exited non-zero.
pub(crate) fn run_git(dir: &Path, args: &[&str]) -> Result<String, Error> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(Error::at(dir))?;

    decode(dir, args, output.status, output.stdout, output.stderr)
}

/// Turns a finished `git` invocation into stdout or the error it earned.
///
/// Shared by [`run_git`] and [`run_git_within`] rather than duplicated: the
/// two differ only in how they wait for the child, and a second copy of this
/// is how the bounded path would drift into reporting failures differently
/// from the unbounded one.
///
/// Takes the buffers by value because both callers own an `Output` they never
/// look at again, so borrowing would only force a copy of stdout back out.
///
/// # Errors
/// [`Error::Git`] if `status` is non-zero, carrying git's own stderr.
/// [`Error::Io`] naming `dir` if stdout is not UTF-8.
fn decode(
    dir: &Path,
    args: &[&str],
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> Result<String, Error> {
    if !status.success() {
        return Err(Error::Git {
            command: format!("git {}", args.join(" ")),
            status: status.code(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        });
    }

    String::from_utf8(stdout).map_err(|err| Error::Io {
        path: dir.to_owned(),
        source: io::Error::other(err),
    })
}

/// Every path in `checkout` that git ignores and that exists on disk right
/// now, relative to `checkout`.
///
/// Asks `git status --ignored=matching --porcelain` rather than parsing
/// `.gitignore` by hand. Parsing gets negations
/// (`!server/src/configs/.gitkeep`), anchored globs (`/docker-compose.yml`)
/// and nested ignore files in subdirectories wrong; git already answers
/// this question correctly, because it is git's own question to answer.
///
/// `=matching` (over the porcelain default, which git calls `traditional`)
/// is what keeps a wholly-ignored directory as one entry - `node_modules/`,
/// correctly, since it is meant to move as a single symlink - while still
/// naming an individually-ignored file on its own when it sits beside
/// ordinary tracked content, such as `config/local.json` next to a tracked
/// `config/schema.sql`. The default `traditional` mode collapses both cases
/// down to the containing directory, which would make a single ignored
/// file impossible to symlink without dragging its tracked siblings along
/// as dangling links.
///
/// `-z` terminates each entry with NUL instead of newline, and it is the
/// only output mode in which a path is never quoted. Without it git wraps
/// any path holding a space, a quote or a non-ASCII byte in `"..."` with
/// C-style escapes, `core.quotePath=false` notwithstanding, which covers only
/// the non-ASCII case. The quotes then became part of the path: a
/// gitignored `sp ace` was linked into the release as `"sp ace"`, pointing at
/// a file of that name that does not exist, and `symlink(2)` does not check.
/// Measured 2026-09-03.
///
/// # Errors
/// [`Error::Io`] if `git` cannot be launched or answers with non-UTF-8
/// bytes; [`Error::Git`] if it launches but exits non-zero.
pub fn ignored_present(checkout: &Path) -> Result<Vec<PathBuf>, Error> {
    let stdout = run_git(
        checkout,
        &["status", "--ignored=matching", "--porcelain", "-z"],
    )?;

    // Walked as records rather than filtered as fields: a rename or a copy
    // (`R` or `C` in either status column) is followed by a second field
    // holding the original path, with no status of its own, and a bare field
    // must not be read as an entry.
    let mut fields = stdout.split('\0');
    let mut found = Vec::new();
    while let Some(entry) = fields.next() {
        let Some((status, path)) = entry.split_at_checked(3) else {
            continue;
        };
        if status.as_bytes()[..2]
            .iter()
            .any(|b| matches!(b, b'R' | b'C'))
        {
            fields.next();
        }
        if status == "!! " {
            found.push(PathBuf::from(path.trim_end_matches('/')));
        }
    }
    Ok(found)
}

/// One `.shepignore` entry, with the two things it can mean told apart at
/// parse time rather than at every match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// A bare name: matches a path component at any depth, so `node_modules`
    /// excludes the top-level directory and every `packages/*/node_modules`
    /// beneath it. `.gitignore`'s own rule for a pattern with no `/`.
    Anywhere(String),
    /// A path: matches only that subtree from the checkout root, so
    /// `packages/app/dist` never touches a top-level `dist`. Written with a
    /// `/` inside it, or with a leading `/`, which `.gitignore` also reads
    /// as "anchor this here".
    Anchored(PathBuf),
}

/// The patterns listed in `checkout`'s `.shepignore`, one per line, blank
/// lines and `#`-comments dropped.
///
/// An absent file is not an error: it returns an empty list, which is the
/// zero-configuration case - no `.shepignore` means share everything
/// [`ignored_present`] finds.
///
/// Every spelling `.gitignore` allows and this file does not is refused with
/// [`Error::Config`] naming the pattern, never accepted and silently matched
/// against nothing: a glob (`*`, `?`, `[`), a negation (`!name`), any
/// backslash escape other than `\!`, and a `..` component. `.shepignore`'s syntax is narrower than `.gitignore`'s, see
/// [`to_link`] for what it does support, and an operator who writes `*.log`
/// believing otherwise deserves a failure they see immediately rather than
/// an artifact this subtraction was built to keep out quietly staying shared
/// forever because the glob never matched anything.
///
/// Three spellings `.gitignore` allows ARE honoured, because each used to be
/// accepted and then matched nothing: a leading `/` anchors the pattern to
/// the checkout root, a leading `./` is dropped, and a trailing `/` is not
/// part of the name. Before 2026-09-03 `/dist` was read as a two-component
/// path that no relative entry could ever start with, so the build output it
/// named stayed shared. `\!name` names a file that really begins with `!`.
///
/// # Errors
/// [`Error::Io`], naming `checkout/.shepignore`, if the file exists but
/// cannot be read for any reason other than simply not being there.
/// [`Error::Config`] if any pattern is one of the refused spellings above.
pub fn shepignore_patterns(checkout: &Path) -> Result<Vec<Pattern>, Error> {
    let path = checkout.join(".shepignore");

    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::Io { path, source }),
    };

    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(parse_pattern)
        .collect()
}

/// One trimmed, non-blank, non-comment `.shepignore` line as a [`Pattern`].
///
/// # Errors
/// [`Error::Config`] naming the line, for a glob, a negation, a backslash
/// other than the leading `\!` escape, a `..` component, or a line that is
/// nothing but slashes and dots.
fn parse_pattern(line: &str) -> Result<Pattern, Error> {
    let refuse = |why: &str| {
        Error::Config(format!(
            ".shepignore pattern {line:?} {why} - a .shepignore pattern is a bare name \
             (matches at any depth) or a path containing `/` (anchored to the checkout root), \
             nothing else"
        ))
    };

    if line.contains(['*', '?', '[']) {
        return Err(refuse(
            "uses glob syntax (`*`, `?`, `[`), which is not supported",
        ));
    }
    if line.starts_with('!') {
        return Err(refuse(
            "is a negation, which is not supported: everything is shared unless named here. A \
             name that really begins with `!` is written `\\!name`, as in .gitignore",
        ));
    }
    // `.gitignore`'s escape for a name that begins with `!`, honoured for the
    // same reason the bare form is refused: the two spellings mean different
    // things, and this one is the only way to name such a file.
    let unescaped;
    let line = match line.strip_prefix("\\!") {
        Some(rest) => {
            unescaped = format!("!{rest}");
            unescaped.as_str()
        }
        None => line,
    };
    // Every other backslash is a `.gitignore` escape this file does not
    // read, and a pattern carrying one would be kept verbatim and match
    // nothing: `\#name` never names `#name`.
    if line.contains('\\') {
        return Err(refuse(
            "carries a backslash; the only escape this file reads is a leading `\\!`",
        ));
    }

    let anchored_by_slash = line.starts_with('/');
    // Leading `/` and `./` are stripped until neither is left: one pass
    // each left `.//x` as `/x`, which no relative entry starts with. What
    // remains is either empty, or a body whose first component is a name.
    let mut body = line;
    loop {
        let stripped = body.trim_start_matches('/').trim_start_matches("./");
        if stripped == body {
            break;
        }
        body = stripped;
    }
    let body = body.trim_end_matches('/');
    // Nothing left, or a lone `.`, is a name no path component ever has.
    // `Path` keeps only a leading `.` as a component and drops interior ones,
    // so this catches `.` and `/./` while `x/./y` is left alone: it matches
    // as `x/y`, the same way on both sides of the comparison.
    if body.is_empty()
        || Path::new(body)
            .components()
            .any(|component| component == Component::CurDir)
    {
        return Err(refuse("names nothing"));
    }
    if Path::new(body)
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(refuse(
            "climbs out of the checkout with `..`, so it cannot name anything in it",
        ));
    }

    if anchored_by_slash || body.contains('/') {
        Ok(Pattern::Anchored(PathBuf::from(body)))
    } else {
        Ok(Pattern::Anywhere(body.to_owned()))
    }
}

/// Whether `path` (relative to the checkout) is named by `pattern`.
///
/// The two arms are the two meanings [`Pattern`] documents. Wildcards never
/// reach here: [`shepignore_patterns`] refuses them before [`to_link`] ever
/// calls this.
fn pattern_matches(path: &Path, pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Anywhere(name) => path
            .components()
            .any(|component| component.as_os_str() == name.as_str()),
        Pattern::Anchored(prefix) => path.starts_with(prefix),
    }
}

/// The relative paths a release shares from the operator's own checkout.
///
/// Only [`to_link`] builds one, and that is the whole point of the type.
/// [`crate::flockfile`] treats the presence of `Flockfile.override.toml` in
/// this list as proof that the override is the operator's file rather than
/// one the repository committed, and proof has to come from the thing that
/// did the linking. A plain slice any caller could assemble was the same
/// evidence with no chain of custody: a wrong or lazy caller could mint the
/// answer. With the constructor private the only way to hold one is to have
/// asked [`to_link`], which read the operator's checkout to make it.
///
/// Read-only afterwards. It derefs to a slice for [`link_into`] and the
/// tests, and answers [`Self::includes_override`] for `flockfile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromCheckout(Vec<PathBuf>);

impl FromCheckout {
    /// Whether the operator's `Flockfile.override.toml` is among the paths.
    ///
    /// Answered from this list, not from the filesystem. Three versions of
    /// this check asked the filesystem and all three were wrong, the last
    /// one subtly enough to be worth recording.
    ///
    /// It required the override to resolve OUTSIDE the release, on the
    /// grounds that the operator's arrives as a symlink into their checkout.
    /// A repository can satisfy that in two commits. Release A ships an
    /// ordinary tracked file holding `user = "root"` under some innocuous
    /// name, and goes live, which by this crate's own invariant means
    /// `current` points at it. Release B then commits
    /// `Flockfile.override.toml` as a symlink to
    /// `../../current/.deploy-payload.toml`. That resolves into release A,
    /// which is outside release B, so the check passed and the refusal was
    /// skipped. Demonstrated 2026-08-28.
    ///
    /// The lesson is not that the rule needed another clause. It is that
    /// provenance cannot be recovered from a path once the path exists:
    /// whoever can write the tree can write the evidence. [`to_link`] is
    /// the only thing that knows which files came from the operator's
    /// checkout, because it is what chose them, so the answer travels from
    /// there instead of being reconstructed later.
    #[must_use]
    pub fn includes_override(&self) -> bool {
        self.0
            .iter()
            .any(|path| path == Path::new(crate::flockfile::OVERRIDE))
    }

    /// A list the tests assemble by hand. Test builds only, so production
    /// code has exactly one way to hold one: asking [`to_link`].
    #[cfg(test)]
    pub fn of(paths: Vec<PathBuf>) -> Self {
        Self(paths)
    }
}

impl std::ops::Deref for FromCheckout {
    type Target = [PathBuf];

    fn deref(&self) -> &[PathBuf] {
        &self.0
    }
}

/// [`ignored_present`], minus whatever `.shepignore` names, except the
/// operator's own `Flockfile.override.toml`, which nothing can filter out.
///
/// This subtraction is the entire reason `.shepignore` exists. `.gitignore`
/// conflates config that must be shared, caches that may be, and build
/// outputs that must not, because git ignores all three for the same
/// reason. Symlinking a build output back to a single shared copy means the
/// next release's build writes straight through that link, replacing the
/// assets the current release is serving mid-build; rolling back afterwards
/// then serves the new build's output under the old release's name. That
/// kills blue/green and rollback together, which is why this subtraction
/// must never be skipped.
///
/// # Errors
/// Whatever [`ignored_present`] or [`shepignore_patterns`] returns.
pub fn to_link(checkout: &Path) -> Result<FromCheckout, Error> {
    let ignored = ignored_present(checkout)?;
    let patterns = shepignore_patterns(checkout)?;

    Ok(FromCheckout(
        ignored
            .into_iter()
            .filter(|path| {
                // The operator's override is the one entry `.shepignore` cannot
                // reach. `.shepignore` is committed by the deployed repository,
                // and this list is the whole of the evidence
                // `FromCheckout::includes_override` has that the override came
                // from the operator rather than from the repo. A repo that can delete the
                // entry deletes every pin the operator wrote in the override,
                // `user` among them, and a build pinned to an unprivileged
                // account runs as the dog's own uid instead. Silently: nothing
                // errors, because an absent override is a legitimate state.
                path == Path::new(crate::flockfile::OVERRIDE)
                    || !patterns
                        .iter()
                        .any(|pattern| pattern_matches(path, pattern))
            })
            .collect(),
    ))
}

/// Symlinks every path in `paths` from `checkout` into `release`, creating
/// whatever parent directories the release needs along the way.
///
/// `checkout` is canonicalised before anything is joined onto it. A
/// symlink's target text is stored exactly as given - `symlink()` performs
/// no resolution of its own - and the OS later resolves a relative target
/// against the *symlink's own containing directory*, not against this
/// process's working directory or against whatever the caller meant by
/// `checkout`. A relative `checkout` therefore produced a symlink whose
/// target text was embedded literally and dangled the moment anything
/// read through it: `symlink()` itself still succeeded, so the deploy
/// would carry on and the break would only surface after the swap and
/// after the reload, when something finally tried to read a shared file.
/// Canonicalising first makes the target text absolute regardless of what
/// form `checkout` arrived in, and turns a checkout that cannot be
/// resolved at all into an immediate, named error instead of a link that
/// looks fine until it is used.
///
/// Reads from `checkout` and writes only under `release` - the dog never
/// writes to the operator's own checkout, and any code path that would is a
/// bug. `paths` are relative, the same relative paths [`to_link`] returns,
/// and are joined onto both roots here: onto `checkout` to find the real
/// file, onto `release` to decide where its symlink belongs.
///
/// # Errors
/// [`Error::Io`], naming `checkout`, if it cannot be canonicalised - it
/// does not exist, or a component of it cannot be resolved. Otherwise
/// [`Error::Io`], naming the release-side path that failed, if a parent
/// directory cannot be created or the symlink itself cannot be made.
///
/// One collision is not an [`Error::Io`] at all. A release-side path that
/// already exists gives [`Error::Config`] instead, because the cause is a
/// `.shepignore` that shares something the release builds for itself, which
/// the operator fixes by editing that file rather than by looking at the
/// filesystem. The message names the colliding path and the file to edit.
pub fn link_into(release: &Path, checkout: &Path, paths: &FromCheckout) -> Result<(), Error> {
    let checkout = fs::canonicalize(checkout).map_err(Error::at(checkout))?;

    for relative in paths.iter() {
        let target = checkout.join(relative);
        let link = release.join(relative);

        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent).map_err(Error::at(parent))?;
        }

        symlink(&target, &link).map_err(|source| {
            if source.kind() == io::ErrorKind::AlreadyExists {
                return Error::Config(format!(
                    "{} is already present in the release, so {} cannot be linked from the \
                     checkout. The usual cause is a build output that git ignores and \
                     `.shepignore` does not: this dog gives each sheep its own build cache and \
                     links it in first, and the operator's own build artifacts must not be \
                     shared into a release at all, because the next release's build would write \
                     through the link and replace what the current one is serving. Add {} to \
                     `.shepignore` in the checkout.",
                    link.display(),
                    relative.display(),
                    relative.display()
                ));
            }
            Error::Io {
                path: link.clone(),
                source,
            }
        })?;
    }

    Ok(())
}

/// Points `release/target` at the dog's own build cache, creating the cache
/// if this is the first release to want it.
///
/// Runs BEFORE [`link_into`], so a checkout that shares its own `target`
/// (no `.shepignore`, which the design says is a misconfiguration rather
/// than a mode) collides in `link_into` where the error can name the fix,
/// rather than here where it cannot.
///
/// A release that already has a `target` path is left exactly as it is and
/// gets no cache. That is a repository which committed the directory, which
/// is unusual and its own business; overruling it would be this crate
/// deciding the repository's layout for it.
///
/// # Errors
/// [`Error::Io`], naming the cache, if it cannot be created; naming the
/// link, if the symlink cannot be made for any reason other than something
/// already being there.
pub fn link_cache(release: &Path, cache_target: &Path) -> Result<(), Error> {
    let link = release.join("target");
    // `symlink_metadata` answers for a directory, a file, a live link and a
    // dangling one alike, which is every "something is already there".
    if link.symlink_metadata().is_ok() {
        return Ok(());
    }

    fs::create_dir_all(cache_target).map_err(Error::at(cache_target))?;

    symlink(cache_target, &link).map_err(Error::at(link))
}

#[cfg(test)]
mod tests {
    use crate::fixtures;

    /// fails if `off_thread` stops passing a result through, or turns a
    /// panic into an error. The poll loop's guard is what should see a
    /// panic, so it has to leave here as one.
    #[tokio::test]
    async fn off_thread_answers_with_the_result_and_re_raises_a_panic() {
        let answered = off_thread(|| Ok::<_, Error>(41 + 1))
            .await
            .expect("passes through");
        assert_eq!(answered, 42);

        let refused = off_thread(|| Err::<(), _>(Error::Config("no".to_owned())))
            .await
            .expect_err("passes the error through");
        assert!(matches!(refused, Error::Config(_)));

        let caught = std::panic::AssertUnwindSafe(off_thread(|| -> Result<(), Error> {
            panic!("inside the pool");
        }));
        let outcome = futures_catch(caught).await;
        assert!(outcome.is_err(), "the panic must come out as a panic");
    }

    /// `catch_unwind` for a future, the same dozen lines `crate::poll`
    /// keeps, spelled here so this module's test does not reach into that
    /// one's.
    async fn futures_catch<F: std::future::Future>(
        fut: std::panic::AssertUnwindSafe<F>,
    ) -> Result<F::Output, Box<dyn std::any::Any + Send>> {
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll};
        struct Catch<F>(Pin<Box<F>>);
        impl<F: Future> Future for Catch<F> {
            type Output = Result<F::Output, Box<dyn std::any::Any + Send>>;
            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let inner = &mut self.get_mut().0;
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    inner.as_mut().poll(cx)
                })) {
                    Ok(Poll::Pending) => Poll::Pending,
                    Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
                    Err(payload) => Poll::Ready(Err(payload)),
                }
            }
        }
        Catch(Box::pin(fut.0)).await
    }

    /// fails if a chatty-but-healthy git subprocess is killed as if it hung.
    ///
    /// A pipe holds about 64 KiB before a writer blocks. The first version of
    /// `run_git_within` read the pipes only after `try_wait` reported the
    /// child exited, so a child saying more than that blocked in `write(2)`,
    /// never exited, and was killed at the deadline having done nothing wrong.
    ///
    /// Measured 2026-08-28 against that version: a child writing 200 KB and
    /// then exiting was killed at a three-second deadline, when reading its
    /// pipe would have let it finish in milliseconds. `git fetch --prune`
    /// against a repository with many refs says far more than 64 KiB, so this
    /// would have reported healthy remotes as unreachable ones: a worse
    /// failure than the hang the budget exists to prevent.
    ///
    /// `git ls-remote` on the crate's own repository is the chatty subject
    /// because it is guaranteed local, needs no network, and prints one line
    /// per ref.
    #[test]
    fn a_chatty_subprocess_is_not_mistaken_for_a_hung_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        run_git(dir.path(), &["init", "-q", "-b", "main"]).expect("init");
        run_git(dir.path(), &["config", "user.email", "t@example.invalid"]).expect("email");
        run_git(dir.path(), &["config", "user.name", "t"]).expect("name");
        // Comfortably past a 64 KiB pipe buffer, and one write rather than
        // thousands of git invocations.
        std::fs::write(dir.path().join("big.txt"), "x".repeat(400_000)).expect("big file");
        run_git(dir.path(), &["add", "big.txt"]).expect("add");
        run_git(dir.path(), &["commit", "-q", "-m", "big"]).expect("commit");

        let out = run_git_within(
            dir.path(),
            &["show", "HEAD:big.txt"],
            Duration::from_secs(20),
        )
        .expect("a chatty command must not be mistaken for a hung one");
        assert!(
            out.len() > 100_000,
            "the fixture must exceed a pipe buffer or this proves nothing, got {} bytes",
            out.len()
        );
    }

    /// fails if a process git spawned can extend the budget on the SUCCESS
    /// path, where git itself exited cleanly.
    ///
    /// The nastiest of the three versions of this bug, because it fires on the
    /// ordinary case rather than on a failure. `git` can exit 0 while
    /// something it forked still holds fd 1 or 2: an ssh ControlPersist
    /// master, or a `&`-backgrounded helper in an alias. `try_wait` sees
    /// success, the loop breaks, and collecting the output blocks on a pipe
    /// that will not close.
    ///
    /// Measured 2026-08-28 before the fix: this exact shape returned `Ok`
    /// after 8.02 seconds against a 600ms budget.
    #[test]
    fn output_is_collected_within_the_budget_even_when_git_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let started = Instant::now();
        let _ = run_git_within(
            dir.path(),
            &["-c", "alias.fork=!sh -c 'sleep 8 &'", "fork"],
            Duration::from_millis(600),
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(4),
            "git exited at once; collecting its output must not wait out what it \
             forked. Took {elapsed:?} against a 600ms budget"
        );
    }

    /// fails if a process git spawned can extend the budget past its end.
    ///
    /// `Child::kill` signals the immediate pid only. `git fetch` over `ssh://`
    /// forks an `ssh` that inherits our pipe write-ends, so killing git alone
    /// leaves that `ssh` holding the pipe and the read end never sees EOF.
    /// Demonstrated 2026-08-28: a reader thread on a pipe a grandchild still
    /// holds does not finish, so joining it hangs past the budget, which is
    /// the exact failure this function exists to prevent.
    ///
    /// Uses `sh` rather than git because git is hard to make fork on demand,
    /// and the mechanism under test is the process group, not git.
    ///
    /// The assertion is on ELAPSED TIME, not on the error. An error alone
    /// would be returned by the broken version too, eventually; the whole
    /// claim is that it comes back near the budget rather than near the
    /// grandchild's own lifetime.
    #[test]
    fn a_grandchild_cannot_extend_the_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `git -c alias.x=!<shell>` is how you make git fork something that
        // outlives it while still being a git invocation.
        let started = Instant::now();
        let err = run_git_within(
            dir.path(),
            &["-c", "alias.hang=!sh -c 'sleep 30 &' && sleep 30", "hang"],
            Duration::from_millis(600),
        )
        .expect_err("must not succeed");
        let elapsed = started.elapsed();

        assert!(
            format!("{err}").contains("no answer within"),
            "must fail on the budget: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "must return near the budget, not wait out the grandchild; took {elapsed:?}"
        );
    }

    /// fails if a git subprocess can outlive its budget.
    ///
    /// Without the bound this hangs forever rather than failing, which is the
    /// whole point: the poll loop deploys targets one at a time, so an
    /// unanswered fetch stops every target with no error and no log line.
    ///
    /// `10.255.255.1` is RFC 1918 space that routes nowhere on an ordinary
    /// host, so the connect blocks rather than being refused. Measured before
    /// this test was written: the same fetch was still blocked after three
    /// seconds. A refusal would make this test pass for the wrong reason, so
    /// it asserts on the timeout's own message rather than merely on `Err`.
    #[test]
    fn a_git_subprocess_that_never_answers_is_abandoned() {
        let dir = tempfile::tempdir().expect("tempdir");
        run_git(dir.path(), &["init", "-q"]).expect("init");

        let err = run_git_within(
            dir.path(),
            &[
                "fetch",
                "git://10.255.255.1/x",
                "+refs/heads/*:refs/heads/*",
            ],
            Duration::from_millis(400),
        )
        .expect_err("an unanswered fetch must not hang");

        let said = format!("{err}");
        assert!(
            said.contains("no answer within"),
            "must fail on the budget, not on something else: {said}"
        );
    }

    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// [`fixtures::checkout`] under the name every test here calls it by.
    ///
    /// `git add` silently skips anything matched by `.gitignore`, so an entry
    /// meant to be "ignored and present" - `config/local.json` in the
    /// fixtures below - simply stays untracked while a `.gitignore` or
    /// `tracked.txt` entry lands in the commit. That is exactly the split
    /// every test here needs: something tracked, something ignored.
    fn fixture_repo(entries: &[(&str, &str)]) -> TempDir {
        fixtures::checkout(entries)
    }

    /// Guards `link_into_resolves_even_when_checkout_is_relative`, the one
    /// test in this module that mutates the process's current directory.
    /// `std::env::set_current_dir` is global, process-wide state that
    /// Rust's default parallel test runner does nothing to serialise, so a
    /// lock is the difference between "one test briefly changes cwd" and
    /// "two threads race to change and restore cwd at once".
    static CWD_GUARD: Mutex<()> = Mutex::new(());

    /// fails if enumeration stops using git's own answer. Parsing
    /// `.gitignore` by hand gets negations (`!server/src/configs/.gitkeep`),
    /// anchored globs (`/docker-compose.yml`) and nested ignore files
    /// wrong; `git status --ignored` gets all three right because it is
    /// git deciding.
    #[test]
    fn enumeration_asks_git_rather_than_parsing_gitignore() {
        let repo = fixture_repo(&[
            (".gitignore", "config/\n!config/.gitkeep\n"),
            ("config/local.json", "{}"),
            ("config/.gitkeep", ""),
            ("tracked.txt", "x"),
        ]);
        let found = ignored_present(repo.path()).expect("enumerates");
        assert!(found.iter().any(|p| p.ends_with("config")));
        assert!(!found.iter().any(|p| p.ends_with("tracked.txt")));
    }

    /// fails if a plain untracked-but-not-ignored file is treated as shared.
    /// `ignored_present` keeps only lines beginning `!! `; a file git status
    /// reports as `?? ` (untracked, not ignored) has no business in this
    /// list, and nothing in the test above proves that half of the filter -
    /// its only untracked-looking entry (`tracked.txt`) is committed, not
    /// merely present.
    #[test]
    fn ignored_present_excludes_untracked_files_that_are_not_ignored() {
        let repo = fixture_repo(&[(".gitignore", "dist/\n"), ("dist/app.js", "//")]);
        fs::write(repo.path().join("scratch.txt"), "untracked, not ignored")
            .expect("write scratch file");

        let found = ignored_present(repo.path()).expect("enumerates");
        assert!(found.iter().any(|p| p.ends_with("dist")));
        assert!(!found.iter().any(|p| p.ends_with("scratch.txt")));
    }

    /// fails if the provenance answer stops following the list. The
    /// override's presence among the linked paths is the only evidence
    /// `flockfile` has that the file is the operator's; an answer from
    /// anywhere else is one the repository can forge (see
    /// `FromCheckout::includes_override`).
    #[test]
    fn the_override_is_recognised_only_when_it_was_linked() {
        let repo = tempfile::tempdir().expect("tempdir");
        fixtures::run_git(repo.path(), &["init", "-q"]);
        fs::write(repo.path().join(".gitignore"), "Flockfile.override.toml\n").expect("ignore");
        fs::write(
            repo.path().join("Flockfile.override.toml"),
            "[[app]]\nname = 'web'\nuser = 'app'\n",
        )
        .expect("override");

        let linked = to_link(repo.path()).expect("computes");
        assert!(linked.includes_override(), "{linked:?}");

        fs::remove_file(repo.path().join("Flockfile.override.toml")).expect("removed");
        let linked = to_link(repo.path()).expect("computes");
        assert!(!linked.includes_override(), "{linked:?}");
        assert!(!FromCheckout::of(vec![PathBuf::from("config/local.json")]).includes_override());
    }

    /// fails if a `.shepignore` entry is still linked. This is the whole
    /// reason `.shepignore` exists: symlinking a build output means release
    /// B's build writes through the link and replaces what release A is
    /// currently serving, which kills blue/green and rollback in one line.
    #[test]
    fn shepignored_paths_are_not_linked() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\nconfig/local.json\n"),
            (".shepignore", "dist\n"),
            ("dist/app.js", "//"),
            ("config/local.json", "{}"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(linked.iter().any(|p| p.ends_with("config/local.json")));
        assert!(!linked.iter().any(|p| p.ends_with("dist")));
    }

    /// fails if a committed `.shepignore` can drop the operator's own
    /// override out of the shared list.
    ///
    /// `.shepignore` is a repo-committed file, so the deployed repository
    /// writes it. `Flockfile.override.toml` is the operator's, and
    /// `FromCheckout::includes_override` treats presence in this list as the whole
    /// proof of that. Letting the repo delete the entry silently drops every
    /// pin the operator put in the override, `user` included, which is the
    /// one that keeps a build off the dog's own uid. One innocuous-looking
    /// line, and a build the operator pinned to `svc` runs as root instead.
    #[test]
    fn a_committed_shepignore_cannot_drop_the_operators_override() {
        let repo = fixture_repo(&[
            (".gitignore", "Flockfile.override.toml\n"),
            (".shepignore", "Flockfile.override.toml\n"),
        ]);
        // The operator's own, present on disk and never committed, exactly
        // like the `config/local.json` the other fixtures here use.
        fs::write(
            repo.path().join("Flockfile.override.toml"),
            "[[app]]\nname = \"web\"\nuser = \"svc\"\n",
        )
        .expect("the operator's override");

        let linked = to_link(repo.path()).expect("computes");

        assert!(
            linked
                .iter()
                .any(|p| p.ends_with("Flockfile.override.toml")),
            "the override must survive a repo-committed .shepignore: {linked:?}"
        );
    }

    /// fails if a repo with no `.shepignore` stops sharing everything
    /// ignored. That is the zero-configuration case and the common one.
    #[test]
    fn no_shepignore_means_share_everything_ignored() {
        let repo = fixture_repo(&[(".gitignore", "config/\n"), ("config/local.json", "{}")]);
        assert!(!to_link(repo.path()).expect("computes").is_empty());
    }

    /// fails if `.shepignore` parsing stops skipping blank lines, or stops
    /// skipping `#` comments - two separate clauses of the same filter, and
    /// a filter proven on one clause can still be broken on the other.
    /// `shepignored_paths_are_not_linked` above only exercises a
    /// `.shepignore` with neither blank lines nor comments in it, so this
    /// is the only test standing between either clause and going unguarded.
    #[test]
    fn shepignore_patterns_skips_blank_lines_and_comments() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (
                ".shepignore",
                "# build output, never share this\n\ndist\n\n",
            ),
            ("dist/app.js", "//"),
        ]);
        let patterns = shepignore_patterns(repo.path()).expect("reads");
        assert_eq!(patterns, vec![Pattern::Anywhere("dist".to_owned())]);
    }

    /// fails if a leading `/` stops anchoring a pattern to the checkout
    /// root. `.gitignore` reads `/dist` that way, so an operator will write
    /// it; before 2026-09-03 it parsed as a two-component path no relative
    /// entry could start with, and matched nothing while saying nothing.
    #[test]
    fn a_leading_slash_anchors_a_pattern_to_the_checkout_root() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "/dist\n"),
            ("dist/app.js", "//"),
            ("packages/app/dist/bundle.js", "//"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(!linked.contains(&PathBuf::from("dist")), "{linked:?}");
        assert!(
            linked.contains(&PathBuf::from("packages/app/dist")),
            "anchored means only the root one: {linked:?}"
        );
    }

    /// fails if a trailing `/` is read as part of the name. `dist/` is how
    /// `.gitignore` spells "the directory", and the same operator copies the
    /// line across.
    #[test]
    fn a_trailing_slash_is_not_part_of_the_name() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "dist/\n"),
            ("dist/app.js", "//"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(!linked.contains(&PathBuf::from("dist")), "{linked:?}");
    }

    /// fails if `\!name` stops naming a file that really begins with `!`,
    /// or if a lone `.` is accepted. The first is `.gitignore`'s own escape
    /// and the only way to name such a file once the bare form is refused;
    /// the second is a name no path component ever has.
    #[test]
    fn an_escaped_bang_names_the_file_and_a_lone_dot_is_refused() {
        assert_eq!(
            parse_pattern("\\!weird").expect("the escape is honoured"),
            Pattern::Anywhere("!weird".to_owned())
        );
        for pattern in [".", "./.", "./", ".//", "/./"] {
            let err = parse_pattern(pattern).expect_err(pattern);
            assert!(
                err.to_string().contains("names nothing"),
                "{pattern}: {err}"
            );
        }
        // And the spellings that mean something still parse to it. `/./x`
        // used to come out as `Anchored("./x")`, which no entry starts with.
        assert_eq!(
            parse_pattern("./dist").expect("a leading ./ is dropped"),
            Pattern::Anywhere("dist".to_owned())
        );
        assert_eq!(
            parse_pattern("/./x").expect("a leading /./ is an anchor"),
            Pattern::Anchored(PathBuf::from("x"))
        );
        assert_eq!(
            parse_pattern(".//x").expect("stripped to a fixed point"),
            Pattern::Anywhere("x".to_owned())
        );
        let err = parse_pattern("\\#name").expect_err("an escape this file does not read");
        assert!(err.to_string().contains("backslash"), "{err}");
    }

    /// fails if `printable` lets a character through that can reverse,
    /// split or hide a log line. Control characters are the obvious half;
    /// the bidi override and the zero-width space are format characters
    /// `is_control` does not cover.
    #[test]
    fn printable_replaces_everything_that_can_forge_a_line() {
        let shown = printable("a\u{1b}[2Jb\u{202e}c\u{200b}d\ne\u{061c}f\u{e0041}g");
        assert_eq!(shown, "a?[2Jb?c?d?e?f?g");
        assert_eq!(printable("plain/name.txt"), "plain/name.txt");
    }

    /// fails if a rename in the operator's checkout is read as an ignored
    /// entry. With `-z` a rename is two fields, the second a bare path with
    /// no status, and a field-by-field filter would take a second field that
    /// happens to begin `!! ` for an entry.
    #[test]
    fn a_staged_rename_is_one_record_not_two() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            ("dist/app.js", "//"),
            ("old.txt", "x"),
        ]);
        // A file whose new name would parse as an ignored entry if its record
        // were split into two.
        fixtures::run_git(repo.path(), &["mv", "old.txt", "!! x"]);

        let found = ignored_present(repo.path()).expect("enumerates");

        assert!(found.contains(&PathBuf::from("dist")), "{found:?}");
        assert!(
            !found.iter().any(|p| p.ends_with("x")),
            "the rename's own path must not be read as ignored: {found:?}"
        );
    }

    /// fails if a spelling this file cannot honour is accepted and silently
    /// matches nothing. Each of these is valid `.gitignore`, and each used
    /// to parse: `!dist` as a name beginning with `!`, `../x` as a path no
    /// entry starts with, `/` as an empty name.
    #[test]
    fn a_negation_a_parent_component_and_a_bare_slash_are_refused_by_name() {
        for pattern in ["!dist", "../sibling", "/", "./"] {
            let repo = fixture_repo(&[(".gitignore", "dist/\n"), (".shepignore", pattern)]);
            let err = shepignore_patterns(repo.path())
                .expect_err(&format!("{pattern:?} must be refused"));
            assert!(matches!(err, Error::Config(_)), "{pattern:?}: {err}");
            assert!(
                err.to_string().contains(pattern),
                "the refusal must name the pattern: {err}"
            );
        }
    }

    /// fails if a bare `.shepignore` pattern stops matching a nested
    /// directory of the same name. ReactMap's real ignored set has
    /// `node_modules/` at the top level and several more nested under
    /// `packages/*/node_modules/`; a user writing `node_modules` in
    /// `.shepignore` means all of them, matching how a bare name in
    /// `.gitignore` itself matches at any depth.
    #[test]
    fn shepignore_bare_pattern_matches_at_any_depth() {
        let repo = fixture_repo(&[
            (".gitignore", "node_modules/\ndist/\n"),
            (".shepignore", "node_modules\n"),
            ("node_modules/a.js", "//"),
            ("packages/foo/node_modules/b.js", "//"),
            ("dist/app.js", "//"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(!linked.iter().any(|p| p.ends_with("node_modules")));
        assert!(linked.iter().any(|p| p.ends_with("dist")));
    }

    /// fails if a `.shepignore` pattern containing `/` stops being anchored
    /// to the checkout root. A naive "match the last component anywhere"
    /// implementation would make `packages/dist` also exclude an unrelated
    /// top-level `dist`; the anchored rule must exclude only the exact
    /// subtree named.
    #[test]
    fn shepignore_pattern_with_slash_is_anchored_to_its_own_subtree() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\npackages/dist/\n"),
            (".shepignore", "packages/dist\n"),
            ("dist/app.js", "//"),
            ("packages/dist/bundle.js", "//"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(linked.contains(&PathBuf::from("dist")));
        assert!(!linked.contains(&PathBuf::from("packages/dist")));
    }

    /// fails if `link_into` stops actually linking back to the checkout, or
    /// stops creating the parent directories a nested shared path needs.
    /// `config/local.json` exercises both: the release has no `config/`
    /// directory until `link_into` makes one, and the file it links to must
    /// still be the checkout's own copy, not one dragged along.
    #[test]
    fn link_into_creates_symlinks_that_resolve_into_the_checkout() {
        let repo = fixture_repo(&[
            (".gitignore", "config/local.json\n"),
            ("config/local.json", r#"{"real":true}"#),
        ]);
        let release = tempfile::tempdir().expect("release tempdir");

        let paths = to_link(repo.path()).expect("computes");
        link_into(release.path(), repo.path(), &paths).expect("links");

        let linked_path = release.path().join("config").join("local.json");
        assert!(linked_path.is_symlink());
        let contents = fs::read_to_string(&linked_path).expect("read through symlink");
        assert_eq!(contents, r#"{"real":true}"#);
    }

    /// fails if `link_into` goes back to embedding `checkout` literally as
    /// the symlink's target text. A relative `checkout` used to produce a
    /// symlink whose target the OS resolves against the symlink's own
    /// directory inside the release, not against anything the caller
    /// meant - `symlink()` itself never noticed, so the only way to catch
    /// this is to actually read through the result. cwd is changed to the
    /// checkout's own parent so `relative_checkout` is a genuinely relative
    /// path the fix must canonicalise, not merely a path that happens to
    /// already be absolute.
    #[test]
    fn link_into_resolves_even_when_checkout_is_relative() {
        let repo = fixture_repo(&[
            (".gitignore", "config/local.json\n"),
            ("config/local.json", r#"{"real":true}"#),
        ]);
        let release = tempfile::tempdir().expect("release tempdir");
        let paths = vec![PathBuf::from("config/local.json")];

        let _guard = CWD_GUARD.lock().expect("cwd guard poisoned");
        let original_cwd = std::env::current_dir().expect("read cwd");
        std::env::set_current_dir(repo.path().parent().expect("repo has a parent"))
            .expect("chdir into repo's parent");
        let relative_checkout = PathBuf::from(repo.path().file_name().expect("repo has a name"));

        let result = link_into(release.path(), &relative_checkout, &FromCheckout::of(paths));

        std::env::set_current_dir(&original_cwd).expect("restore cwd");
        result.expect("links despite a relative checkout");

        let linked_path = release.path().join("config").join("local.json");
        let contents = fs::read_to_string(&linked_path).expect("read through symlink");
        assert_eq!(contents, r#"{"real":true}"#);
    }

    /// fails if `link_into` stops surfacing a checkout it cannot resolve as
    /// an immediate error. Silently doing nothing, or creating a dangling
    /// link anyway, is exactly the failure-that-looks-like-success shape
    /// the canonicalisation fix exists to close off.
    #[test]
    fn link_into_fails_loudly_when_checkout_does_not_exist() {
        let release = tempfile::tempdir().expect("release tempdir");
        let missing_checkout = release.path().join("no-such-checkout");
        let paths = vec![PathBuf::from("config/local.json")];

        let err = link_into(release.path(), &missing_checkout, &FromCheckout::of(paths))
            .expect_err("a checkout that does not exist cannot be canonicalised");
        assert!(matches!(err, Error::Io { .. }));
    }

    /// fails if a `.shepignore` pattern using `*` stops being refused. An
    /// operator writing `*.log`, trusting the spec's "same idiom as
    /// .gitignore" line, must get a loud failure naming the pattern rather
    /// than a glob that silently matches nothing forever while the
    /// artifact it named stays shared.
    #[test]
    fn shepignore_refuses_a_pattern_with_an_asterisk() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "*.log\n"),
            ("dist/app.js", "//"),
        ]);
        let err = shepignore_patterns(repo.path()).expect_err("must refuse a glob pattern");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("*.log"));
    }

    /// fails if a `.shepignore` pattern using `?` stops being refused - the
    /// second of the three metacharacters `pattern_matches` never gets a
    /// chance to mishandle, since none of them are meant to reach it.
    #[test]
    fn shepignore_refuses_a_pattern_with_a_question_mark() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "cache?.tmp\n"),
            ("dist/app.js", "//"),
        ]);
        let err = shepignore_patterns(repo.path()).expect_err("must refuse a glob pattern");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("cache?.tmp"));
    }

    /// fails if a `.shepignore` pattern using a `[...]` class stops being
    /// refused - the third metacharacter, and the one most likely to be
    /// dropped from a hand-written `contains` check without a test naming
    /// it specifically.
    #[test]
    fn shepignore_refuses_a_pattern_with_a_bracket_class() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "cache[0-9].tmp\n"),
            ("dist/app.js", "//"),
        ]);
        let err = shepignore_patterns(repo.path()).expect_err("must refuse a glob pattern");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("cache[0-9].tmp"));
    }

    /// fails if a release does not get a `target` pointing at the dog's own
    /// cache. Without it every deploy of a Rust project is a from-scratch
    /// build, which the design calls not acceptable for Koji specifically.
    #[test]
    fn a_release_gets_target_linked_at_the_cache() {
        let root = tempfile::tempdir().expect("tempdir");
        let release = root.path().join("release");
        let cache = root.path().join("cache/target");
        fs::create_dir_all(&release).expect("release dir");

        link_cache(&release, &cache).expect("links");

        let link = release.join("target");
        assert_eq!(fs::read_link(&link).expect("a symlink"), cache);
        assert!(cache.is_dir(), "the cache itself must be created");
    }

    /// fails if a release that ships its own tracked `target/` is treated
    /// as an error. A repository committing that directory is unusual and
    /// its own business; refusing the deploy over it would be this crate
    /// overruling the repository about the repository's own layout.
    #[test]
    fn a_release_that_ships_its_own_target_is_left_alone() {
        let root = tempfile::tempdir().expect("tempdir");
        let release = root.path().join("release");
        fs::create_dir_all(release.join("target")).expect("a committed target");
        let cache = root.path().join("cache/target");

        link_cache(&release, &cache).expect("does nothing, successfully");

        assert!(
            release.join("target").is_dir(),
            "the repository's own directory must survive"
        );
        assert!(
            fs::read_link(release.join("target")).is_err(),
            "and must not have been replaced by a link"
        );
    }

    /// fails if a checkout sharing its own `target` collides with the
    /// dog's cache and produces a bare "File exists". The two are genuinely
    /// different things, the operator's dev artifacts and the dog's shared
    /// cache, and the design says the operator's own target directory must
    /// never be linked. The fix is one line in `.shepignore`, so the error
    /// says so.
    #[test]
    fn a_checkout_sharing_target_says_how_to_fix_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let release = root.path().join("release");
        let checkout = root.path().join("checkout");
        fs::create_dir_all(&release).expect("release");
        fs::create_dir_all(checkout.join("target")).expect("their own target");
        link_cache(&release, &root.path().join("cache/target")).expect("links");

        let err = link_into(
            &release,
            &checkout,
            &FromCheckout::of(vec![PathBuf::from("target")]),
        )
        .expect_err("collides");
        let shown = err.to_string();
        assert!(shown.contains(".shepignore"), "{shown}");
        assert!(shown.contains("target"), "{shown}");
    }
}
