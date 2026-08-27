# shep-deploy: poll loop, opt-in and lifecycle implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the working deploy engine into a dog that runs unattended: it surveys the flock, takes a sheep over on request, polls its branch every 30 seconds, deploys, prunes old releases, paints a smit, and puts the sheep back where its operator will look for it when the dog is removed.

**Architecture:** Everything new sits on top of plan one's twelve modules and adds nine more. The engine's own boundary is unchanged: `deploy()` still owns one deploy, `land` still owns every fallible step after the swap. Two of the new pieces (opt-in's cutover, retention) are placed relative to that boundary deliberately rather than beside it, and each says which side it is on and why. One new source of truth appears: shep's muster roll (`$SHEP_HOME/flock.json`), which is the only place the dog can read a sheep's *registered* `AppConfig`.

**Tech Stack:** Rust 2024, MSRV 1.88, `shep-client` 0.1.0 (re-exports `shep_core`), `tokio` (current-thread), `toml`, `serde`, and one new direct dependency, `serde_json`, already in `Cargo.lock` as a transitive dep of `shep-core`.

**Source of truth:** [the design spec](../../brainstorming/specs/2026-08-26-deploy-dog-design.md). Where this plan and the spec disagree, the spec wins and the plan is wrong, EXCEPT for the four places listed under "Where this plan knowingly departs from the spec" below, each of which names its reason.

**Predecessor:** [the deploy engine plan](2026-08-26-deploy-engine.md), complete, 128 unit and 5 integration tests. Its ledger, `.superpowers/sdd/2026-08-26-deploy-engine/progress.md`, records every decision this plan builds on and is worth reading before Task 1.

## Global Constraints

- **MSRV 1.88, edition 2024.** Match shep's.
- **`shep-client`** from crates.io. It re-exports `shep_core`, so do not depend on `shep-core` separately. **`Cargo.lock` pins 0.1.0 while newer patch releases are published**, which is skew worth closing rather than living with: Task 13 installs the newest `shep` and drives it with a dog built against the oldest client. Task 5 already edits `Cargo.toml`, so the bump rides there. Verified before writing this: the protocol is byte-identical between 0.1.0 and 0.1.2, no `Request` variant was added or changed, `ProcessInfo` gained no field, and smits are not in it. So the bump is maintenance and no task's design depends on it. Take the newest published version, run the gate, and if anything moves, say so rather than pinning back.
- **License `MIT OR Apache-2.0`.**
- **`#![forbid(unsafe_code)]`** at the crate root. This is why privilege work shells out to `id` rather than calling `getpwnam`.
- **One crate-level error enum**, `crate::error::Error`, with a manual `Display`. New variants go there. No `anyhow`, no second error type, no per-module enums.
- **`core::error::Error`, never `std::error::Error`.**
- **Every public item documented, with an `# Errors` section describing real behaviour**, not a restatement of the signature. Every new public item also needs a deliberate `Debug` decision: redact anything carrying env or secrets and pin the redaction with an exact-string test (shep's IR-41).
- **Sparse comments.** The doc comment carries the reasoning; the body carries the code.
- **No em dashes**, in code, comments, docs, commit messages or this plan.
- **shep-deploy is Unix only, deliberately, and this plan states it rather than leaving it implicit.** Rin's decision, 2026-08-26: "keep on with just unix for now. I'll do windows after from the windows box." She has Windows support for shep itself nearly finished on another machine and will scope the dog's Windows story herself, from there. This is not an unexamined gap, it is a scoping decision with a known owner and a known sequence. It is the design rather than a detail: the deploy model is `rename(2)` over a **symlink**, and Windows symlinks need developer mode or elevation; the privilege drop is uid and gid, which Windows has no equivalent of; and the root warning shells out to `id -u`. **Write no Windows tasks, add no `cfg(windows)` arms, design no portability layer.** One clear sentence in the three places a person meets it: a `compile_error!`, the README, and this constraint. Task 1 owns it.
- **The dog never writes to the operator's checkout.** It reads from it and symlinks into releases. Any code path that would write there is a bug.
- **`user`/`group` are never taken from a repo-supplied Flockfile.** See spec, "Pinning", and `flockfile::refuse_repo_privilege`.
- **Whatever performs opt-in MUST set `cwd` explicitly**, to `Tree::current()`. This is the single most load-bearing fact in the plan and Task 8 exists around it. See "Facts already measured" below.
- **Every task's tests must be mutation-checked, and the task names the mutation and the expected failure.** A guard test that still passes with its guard removed is worthless, and plan one shipped several before a reviewer caught them by running the mutations rather than reading the diffs. When naming a mutation, say "mutation-check every branch of this predicate", never "mutation-check this clause": naming one clause is exactly what narrowed a Task 2 implementer to checking one clause and missing the other.
- **The gate is four commands, each run on its own with `$?` captured directly:**
  ```bash
  cargo fmt --all --check
  cargo clippy --all-targets --all-features -- -D warnings
  cargo test --locked
  RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
  ```
  The fourth is not optional. Omitting it from plan one's briefs shipped a task that was red on CI while reporting a green gate.
- **One cargo command shape per task.** This crate is a single package, so `cargo test --locked` is the shape; do not mix in `-p shep-deploy` forms.

---

## Facts already measured, which no task should re-derive

Each of these cost real time during plan one. They are inputs, not questions.

- **The poll interval default is 30 seconds**, settled in the spec with its reasoning. Webhooks stay deferred.
- **Do NOT set `CARGO_TARGET_DIR`.** Measured 2026-08-26: with that variable set, `./target` is never created, so any build command hardcoding `./target` breaks outright. Koji's own `make build` ends in `cp ./target/release/koji koji` and exits 1. The design is a dog-owned cache at `$SHEP_HOME/deploy/<sheep>/cache/target`, symlinked as `target` inside each release worktree. Task 3.
- **This is separate from `.shepignore`,** which governs which ignored-and-present files are linked from the *operator's* checkout. `target` correctly stays listed there: the operator's own dev target directory must never be linked.
- **A Flockfile app's DEFAULTED `cwd` is resolved by the CLI, not by the daemon, and over the socket it is not resolved at all.** An earlier draft of this plan said "resolved at registration", which is wrong in a way that would mislead an implementer into thinking the daemon handles it. What actually happens: the CLI's `default_cwd_to_flockfile_dir` canonicalises a `None` `cwd` to the Flockfile's own directory before sending it. **Over the socket, which is the only path this dog uses, a `cwd` of `None` stays `None` and the child inherits the SHEPHERD's working directory, commonly `/`.** So the two failure modes are different and both are bad: a Flockfile registered by the CLI from inside a release pins the sheep to that release forever, and an `AppConfig` this dog sends with `cwd: None` starts the app in the shepherd's cwd, nowhere near its own code. The conclusion is unchanged and now has two reasons instead of one: **set `cwd` explicitly to `Tree::current()`.**
- **The probe a reload uses comes from REGISTRATION, never from the new release.** Nothing in this crate re-registers a running sheep, so a test that needs a reload to fail changes what the probe *looks at*, not the probe.
- **`Online` means something WEAKER after a `Start` than after a `Reload`, and this is the single most consequential fact in Task 8.** `handle_ready_result` (`supervisor.rs:4482`) branches on whether the instance is a reload replacement. For a replacement it defers to `reload_ready_result`, which on `Readiness::TimedOut` calls `abort_reload` and keeps the old instance serving. For a **fresh spawn** it does the opposite: on `TimedOut` it logs "readiness deadline elapsed; marking online anyway" and sets `Online` regardless of what the probe said. That is Rin's own ruling of 2026-08-08, recorded in the comment there, and the reasoning is sound: treating a readiness timeout as a spawn failure would turn a slow-starting app into a restart loop, which is the failure `max_restarts` exists to contain. **The consequence for this dog is that `Online` on a Start is the disjunction "the probe passed OR `listen_timeout` elapsed", and nothing on the wire says which.** `crate::verify` is sound precisely because it verifies a RELOAD. Borrowing its reasoning onto the Start path does not hold.
- **shep's `FlockRegistry` is keyed by NAME and records on every accepted `Start`.** `record` is `map.insert(app.config().name.clone(), ...)` (`snapshot.rs:100`), called from the request handler on every accepted `Start`, so the persisted roll's entry for a name becomes the newest config sent under that name. `roll` then `retain`s only names with a live instance, and the comment says why: "a deleted sheep must not resurrect". **Two consequences this plan is built around.** Deleting an instance does NOT undo the record when another instance keeps the name alive, which is Task 8's problem. And deleting EVERY instance of a name drops it from the roll entirely, which is Task 9's.
- **Nothing in a `ProcessInfo` can reproduce the `AppConfig` a sheep came from.** That is shep's own wording, in `shep-daemon/src/snapshot.rs`, and it is why the muster roll exists. `ProcessInfo` has no `cwd` and no `script`. Task 5 is the consequence.
- **`Request::Start` on an already-registered name ADDS instances, it does not re-register.** `do_start` calls `instance_slots(&existing, instances)`, and `instance_slots(&[0], 1)` is `[1]`. So a `Start` with a new `AppConfig` spawns a *second* instance from the *new* config beside the running one. That is what makes the spec's cutover ("start a new sheep pointing at `current`, wait for `Online`, stop and delete the old registration") implementable at all, and it is also why the first cutover can collide on a port. Task 8.
- **`SelectorSpec::Id(u32)` exists,** and `Request::Delete` is documented as "stop + deregister matching sheep". So the old instance can be deleted by id without touching the new one.
- **`Response::RollSaved { path, apps }` hands back the roll's absolute path,** so the dog asks for it rather than rebuilding it from `ShepPaths`.
- **The integration tier costs about 31 seconds per run** and drives a real shepherd. Every test owns a `tempfile::tempdir()` as `$SHEP_HOME` and kills its own daemon on drop. Do not put a test there that a unit test with an honest fake could carry.
- **Fakes hide defects.** The two worst bugs in plan one survived unit testing because the fakes were too kind: one turned over instantly where a real shepherd keeps the old instance `Online` for seconds, and another could only refuse a reload with an RPC error, never with a transport failure. Where a task's correctness depends on the daemon's real timing or real failure shapes, the task says so and requires the integration tier.
- **The rollback boundary is the swap.** `land` owns every fallible step after it and cannot return an error; each failure is a `Landed` variant, so `deploy`'s match is exhaustive over what has to be undone rather than over what went wrong. **Any new work this plan adds after a swap must live inside that structure rather than beside it.** Building beside the boundary is exactly how plan one accumulated five blockers on a diff where no single round was wrong and the composition was.

---

## Where this plan stands against the spec

**The spec was corrected in four places after this plan's first draft, and three of the four things this plan flagged as departures are now simply the spec.** Re-read against the corrected spec on 2026-08-26:

1. **`CARGO_TARGET_DIR`: resolved, and the spec now leads with the correction.** "Build environment and artifacts" says outright that warm builds do NOT come from that variable and that the section used to say they did; the Koji worked example now says `make build` runs unmodified because its own `cp` resolves through the `target` symlink. This plan already matched the closed open question, so nothing changes here except that the departure is withdrawn. **What has NOT been corrected is this crate's own rustdoc**, which still teaches the overturned mechanism in three places. Task 3 owns that, and see the note under it.
2. **Concurrency: resolved, and the spec now records the reversal.** A push landing mid-deploy does not abort it; the deploy finishes and the newer commit is picked up on the following poll. The spec's own reasoning is the one this plan reached independently, and it goes further: aborting means concurrency around the swap, "and that is where every serious defect in this crate has lived". Task 11 is now implementing the spec rather than departing from it.
3. **The survey's `needs setup` row: resolved.** The spec's example now reads `git checkout, ships a Flockfile`, which is the interpretation this plan took. Departure withdrawn; Task 6 is unchanged.
4. **The smit's two-width exact-string test belongs to shep, not here.** The one item still standing, and it is a scoping statement rather than a disagreement. Rin's ruling was that the smit may be dropped on a narrow terminal *because* it is seen regularly at full width, which carries a requirement her permission does not state outright: it must never be dropped at full width, and must not be crowded out there by a later change. That is a property of `shep flock`'s adaptive column dropping, which lives in shep. This plan owns the string (Task 10) and the publish (Task 12) and carries the two-width requirement forward as a condition on the smit prerequisite.

**One thing this plan does that the spec does not describe, and it is not a departure so much as a gap the spec never reached:** the spec says the first cutover "may have downtime" but never says how the dog decides a freshly started release is healthy. Task 8 answers that, and the answer is weaker than the deploy path's. See Task 8's own preamble, which is the most important prose in this plan.

## Prerequisites in shep, which are NOT this plan's tasks

Changes to `/Users/rin/GitHub/pm2-rs`. Name them, do not implement them.

**Numbered as the spec numbers them.** An earlier draft of this plan renumbered a subset from 1, which made "prerequisite 3" mean the smits work here and the CLI passthrough in the spec, in a document that referred to "prerequisite 3" twice. The spec's numbering is the shared one and this table keeps it.

| # | Change | Status | What it unblocks |
|---|---|---|---|
| 1 | `shep adopt` accepts a binary name on `PATH` and a `~/` path | in flight | Adopting this dog by name at all, which is how it gets supervised |
| 2 | `shep adopt --name`, defaulting from the filename and stripping a leading `shep-` | in flight | `cargo install shep-deploy` then `shep adopt` giving a dog called `deploy` |
| 3 | `shep <dogname> [args]` passthrough | **SHIPPED in 0.1.1** | Nothing outstanding. Verified by adopting a script as `argvtest` and running `shep argvtest foo bar`. `shep deploy koji` fails today only because no dog named `deploy` is adopted yet, which is what `cargo install shep-deploy` plus `shep adopt` fixes. **The measurement also settled the argv question this plan could not**: shep does NOT forward the dog's own name. The dog received `foo bar`, with its own path as `$0` and no `argvtest` anywhere in argv. See "The argument surface" below, because the consequence is not the one it looks like. |
| 4 | `shep rehome` must stop deleting `[dog.<name>]` | approved by Rin, unassigned | Task 2's config surviving a rehome and a re-adoption. Today rehome deletes the operator's own settings, making re-adoption a from-scratch reconfiguration. The help text's `disable`/`rehome` line moves from "forgets its configuration" to "forgets the adoption". |
| 5 | Dogs need an on-remove lifecycle hook | unassigned | Task 9's restore running automatically. **Task 9 is NOT blocked on this**: it builds the subcommand, which is directly runnable, and it starts being called the day shep ships the hook. A dog that has not implemented one exits non-zero on an argument it does not recognise, exactly as `shep-log-rotate` does. The hook's stdout must reach the operator, sanitised through `terminal_safe::sanitise`, carried in the JSON envelope as a field under `--format json`, and riding the existing notice stream so `--quiet` governs it. |
| 6 | **Smits** | unassigned | Task 12, the only task here that cannot be implemented today. Carries the two-width exact-string test named above. |
| 7 | The reload response carries the reload deadline | unassigned | Nothing here, and that is the point. `deploy::budget` hardcodes shep's `(listen_timeout + graceful_timeout + RELOAD_DEADLINE_SLACK) x instances`, copied from `supervisor.rs:3581`, in a crate that cannot see shep's source. Returning the deadline **demotes that coupling rather than deleting it**: an older shepherd sends no deadline, so the copied formula survives as the fallback. It stops being load-bearing against a current daemon, which is the win, and the drift test stays either way. |

The engine plan's ledger numbers a different subset 1 to 4 (rehome, the hook, smits, the deadline). That is rows 4 to 7 here. Mentioned only so a reader moving between the two documents is not misled by the collision.

Plus the whole of the spec's "Documentation shep owes" block, which is shep's work.

**One shep-side item this plan surfaces that is not yet on anybody's list**, and it belongs to Rin rather than to this plan: **exit code 12 is claimed here for "the deploy was rejected and the previous release was put back".** shep's own taxonomy in `docs/specs/shep-v1.md` section 9 runs 0 to 11 and should record that 12 is taken, or the two will collide.

## The argument surface, and why it has two shapes

**shep does not forward the dog's own name.** Measured 2026-08-26 by adopting a script and running `shep argvtest foo bar`: the dog got `foo bar`, with its own path as `$0`. So the same operation arrives in two different shapes depending on how it was invoked, and both have to land in the same place:

| typed | dog's argv |
|---|---|
| `shep deploy koji` | `["koji"]` |
| `shep-deploy deploy koji` | `["deploy", "koji"]` |
| `shep deploy setup koji` | `["setup", "koji"]` |
| `shep deploy survey` | `["survey"]` |
| supervised, as an adopted dog | `[]` |

**This turns out well rather than badly, and the reason is worth seeing before writing the match.** Because the dog name is stripped, `shep deploy setup koji` and `shep deploy survey` already read as English and need no special handling: they arrive as the verb forms the direct invocation uses. The only shape needing a new arm is the bare sheep name, which is `shep deploy koji`, the flagship command Rin designed the naming around.

So `main`'s match gains two arms at the END, after every verb:

```rust
        // Reached only through `shep deploy <sheep>`, which strips the dog's
        // own name. Last, so a verb always wins: a sheep named `survey` is
        // deployed as `shep deploy deploy survey`, which routes through the
        // verb arm above and is the documented way out of the collision.
        [sheep] => deploy_once(sheep).await,
        [sheep, "--watch", mode] => set_watch(sheep, mode),
```

**Order is the whole of the design here.** Verbs are matched first, so the four names `deploy`, `survey`, `setup` and `on-remove` cannot be reached as bare sheep names. That collision is real, tiny, and has an escape hatch that costs nothing to document: the explicit verb form still works through the passthrough, because `shep deploy deploy survey` arrives as `["deploy", "survey"]`. Say it in `USAGE` rather than leaving somebody to find it.

Task 1 owns this, since it is already restructuring the match for exit codes. Every later task that adds a verb adds it **above** these two arms.

## File structure

New:

| file | responsibility |
|---|---|
| `src/config.rs` | `[dog.<name>]`: poll interval and retention count, with their refusals |
| `src/roll.rs` | reading shep's muster roll for each sheep's registered `AppConfig` |
| `src/survey.rs` | classifying every sheep as watched, manual, needs setup, eligible or not eligible |
| `src/optin.rs` | bootstrap: build the tree, prepare the first release, cut the sheep over |
| `src/restore.rs` | the on-remove hook: put a sheep back where its operator will look |
| `src/retention.rs` | pruning worktrees beyond the retention count |
| `src/smit.rs` | the smit string, and (Task 12) publishing it |
| `src/poll.rs` | the supervised poll loop |

Modified:

| file | change |
|---|---|
| `src/main.rs` | Unix-only guard, exit codes, the four new verbs, the supervised poll loop |
| `src/daemon.rs` | `save_roll`, `delete`, and (Task 12) `set_smit` |
| `src/paths.rs` | `cache()`, `cache_target()`, and enumerating every target under `$SHEP_HOME/deploy` |
| `src/git.rs` | `init_bare` |
| `src/shared.rs` | `link_cache`, and one reworded collision error in `link_into` |
| `src/deploy.rs` | link the build cache before the build; prune after a verified deploy |
| `src/error.rs` | new variants, each with its `Display` arm and `source()` decision |
| `Cargo.toml` | `serde_json` |
| `README.md` | Unix only, the new verbs, the poll loop, and a Status section that is true |
| `tests/integration.rs` | the exit code of a real rollback; opt-in and a poll tick against a real shepherd |

---

## Shared test helpers

Several tasks' tests use the same fixtures. Each is a few lines, they live in the `#[cfg(test)]` block of the module that first needs them, and later tasks import them with `use crate::<module>::tests::*`. Defined once here so no task reinvents one with slightly different behaviour, which is how two tests come to disagree about what a fixture means.

| helper | introduced in | what it does |
|---|---|---|
| `write_target(home, sheep, watch, deployed)` | Task 6 | creates `<home>/deploy/<sheep>/` and writes a `deploy.toml` **through `State::write`**, so the fixture and the real reader agree by construction rather than through a hand-written string that can drift |
| `write_target_ready(home, sheep)` | Task 11 | `write_target` plus a real bare repo at `git/`, one commit, and `current` pointed at a built release. The minimum for `deploy::deploy` to reach a reload rather than failing at `git::fetch` |
| `write_target_with_origin(home, sheep, cwd, script)` | Task 9 | `write_target` with `origin_cwd` and `origin_script` set, which is a target the dog took over from a pre-existing sheep |
| `write_target_with_origin_absent(home, sheep)` | Task 9 | the same with both `None`, which is a target the dog bootstrapped and has nothing to restore |
| `checkout_fixture(files)` | Task 6 | a tempdir with `git init -q` and the given files written |
| `checkout_with_commit()` | Task 7 | `checkout_fixture` plus a `Flockfile.toml` declaring one app, `user.email`/`user.name` set **locally** so the commit works on a bare CI runner, and one commit |
| `git_in(dir, args)` | Task 7 | runs one `git` subcommand for fixture setup, panicking on failure. Fixture setup only; production code goes through `crate::git` |
| `head_sha(dir)` | Task 7 | `git rev-parse HEAD`, trimmed |
| `RollOf(&[(sheep, cwd)])` | Task 7 | a `Daemon` double whose `save_roll` writes a roll JSON naming each sheep with that `cwd` and `script = "./run.sh"`, and answers with its path. Everything else `unimplemented!()` |
| `config()` | Task 11 | `DogConfig { interval: Duration::from_secs(30), retention: 5 }`, the documented defaults |
| `target(watch)` / `target(watch, deployed)` | Tasks 10, 11 | a `State` in memory, for the pure functions that take one |

**One rule for all of them, and it is the engine plan's most expensive lesson:** a fake must not be kinder than a real shepherd. Where a fixture stands in for a running flock it keeps the old instance `Online` while a replacement starts, and it turns over across several polls rather than instantly. The instant-turnover fake is what let a blocker that disabled every rollback in the crate pass two full reviews.

---

## Task 1: Unix only, and the exit-code taxonomy

**Files:**
- Modify: `src/main.rs`, `README.md`
- Modify: `tests/integration.rs`

**Interfaces:**
- Produces:
  ```rust
  /// A rolled-back deploy: distinct from success and from a hard failure.
  const ROLLED_BACK: u8 = 12;
  fn code_for(err: &Error) -> u8;
  ```
  and `deploy_once`/`set_watch` change from `Result<(), Error>` to `Result<u8, Error>`, where the `u8` is the exit code a successful run reports.

**Why the exit code is here and not in the deploy engine.** Rin's decision, 2026-08-26: a deploy that rolled back gets a distinct nonzero code, not `0` and not the same code as a hard failure, so a script can tell three outcomes apart. The important half is that the COMMON rollback path is `Ok(Outcome::RolledBack)`, not `Err(Error::RolledBack)`: those two are arms of one match and cannot both happen in a run, and today only the rarer error arm is nonzero at all. A plain verify timeout, which is the ordinary rollback trigger, exits 0 today.

**Why `12`, and why NOT `10`.** shep's rule is "distinct causes get distinct codes; no error ever exits 0". This joins it rather than inventing a scheme: reuse shep's numbers where they fit (2 usage, 4 invalid config, 5 daemon unreachable, 1 failure) and take the next free number for the one meaning shep has no code for.

**`10` was proposed during review on the basis that shep's taxonomy runs 0 to 9. It does not, and taking 10 would be the single worst available choice.** Read `crates/shep-cli/src/exit.rs` rather than a summary of it: the discriminants run to **11**, with `DaemonAlreadyRunning = 10` and `FlockEmpty = 11`, and `FlockEmpty`'s own doc says it was given a new row precisely because "Codes 0-10 were all spoken for".

`10` is worse than merely taken. It is the one code whose meaning crosses a process boundary, and **the constant lives in this crate's own dependency**: `shep_client::spawn::DAEMON_ALREADY_RUNNING = 10` (`spawn.rs:44`), documented as coupling the client to the CLI's taxonomy deliberately, because "an exit status is the only channel a dead child leaves behind", and warning that changing either side without the other reintroduces a documented race. A `shep-deploy` exiting 10 would be claiming a status `shep-client` reads as "another daemon won the cold-start race, keep probing".

So **12**, the next genuinely free number, and **shep's `exit.rs` should record that 12 belongs to shep-deploy** as shep-side follow-up. `docs/specs/shep-v1.md` section 9 and `exit.rs` agree on 0 to 11 today, so the table and the enum both want the row.

**Why `compile_error!` rather than shep's runtime "not yet supported" idiom.** shep refuses at runtime because shep *compiles* on Windows: it carries `cfg(windows)` arms and a windows-gnu cross-check in its phase gate. This crate does not and cannot. `src/shared.rs` imports `std::os::unix::fs::symlink` unconditionally, and `src/build.rs` calls `Command::uid`/`Command::gid` from the Unix extension trait. A runtime refusal would mean cfg-gating every one of those call sites, which is the port itself. So the choice is between one sentence and a wall of unrelated compiler errors about symlinks and uids, and one sentence wins.

- [ ] **Step 1: Write the failing tests**

In `src/main.rs`, inside a new `#[cfg(test)] mod tests`:

```rust
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
        assert_ne!(code_for(&rolled_back), code_for(&Error::Build { status: Some(1) }));
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
            code_for_outcome(&Outcome::Deployed { sha: "new".to_owned() }),
            0
        );
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --locked a_rollback_has_its_own_code`
Expected: FAIL, `code_for` not defined.

- [ ] **Step 3: Add the Unix guard at the crate root**

At the very top of `src/main.rs`, above `#![forbid(unsafe_code)]`:

```rust
#[cfg(not(unix))]
compile_error!(
    "shep-deploy is Unix only. This is deliberate rather than an oversight: the deploy model is \
     rename(2) over a symlink, the build's privilege drop is a uid and a gid, and both are Unix \
     concepts this crate uses directly rather than through a portability layer. Windows support \
     is planned and will be scoped separately."
);
```

- [ ] **Step 4: Implement the two mappings**

```rust
/// The exit code for a deploy that was rolled back.
///
/// shep's own taxonomy (`docs/specs/shep-v1.md` section 9) runs from 0 to 11
/// and this is the next free number, claimed rather than invented: a
/// rollback is a cause shep has no code for, and every cause this dog
/// shares with shep uses shep's number for it. A script must be able to
/// tell three outcomes apart, deployed, cleanly reverted, and broke, and
/// two of those were the same code until Rin ruled otherwise.
const ROLLED_BACK: u8 = 12;

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
        Error::Config(_) => 4,
        Error::Connect(_) => 5,
        _ => 1,
    }
}
```

Then thread them through: `deploy_once` returns `Result<u8, Error>`, answering `Ok(code_for_outcome(&outcome))` after printing; `set_watch` returns `Ok(0)`; `main`'s tail becomes

```rust
match outcome {
    Ok(code) => ExitCode::from(code),
    Err(err) => {
        eprintln!("shep-deploy: {err}");
        ExitCode::from(code_for(&err))
    }
}
```

and the bad-argument arm becomes `ExitCode::from(2)`, which is shep's usage code and clap's own convention, rather than `ExitCode::FAILURE`.

- [ ] **Step 5: Add the two passthrough arms**

See "The argument surface" above for why these exist and why they must be last. At the END of the match, after every verb arm:

```rust
        // Reached only through `shep deploy <sheep>`: the passthrough
        // shipped in shep 0.1.1 and strips the dog's own name, so the
        // flagship command arrives as a bare sheep name with no verb.
        // Last, so a verb always wins.
        [sheep] => deploy_once(sheep).await,
        [sheep, "--watch", mode] => set_watch(sheep, mode),
```

`USAGE` gains both spellings and the collision's escape hatch:

```rust
const USAGE: &str = "\
usage: shep-deploy <verb> [args]

  deploy <sheep> [--watch auto|manual]   deploy one sheep, or set how it is watched
  setup <sheep>                          take a sheep over
  survey                                 report where every sheep stands
  on-remove                              lifecycle hook; shep runs this itself

Adopted as `deploy`, the same verbs run as `shep deploy <verb> [args]`, and
`shep deploy <sheep>` deploys one sheep directly. A sheep whose name is one of
the verbs above is reached with the verb spelled out: `shep deploy deploy survey`.";
```

A test for the routing, since a match arm in the wrong order is invisible until somebody names a sheep `setup`:

```rust
    /// fails if a bare sheep name stops routing to a deploy, or if it
    /// starts shadowing a verb. `shep deploy koji` is the flagship command
    /// and arrives here as `["koji"]`, with no verb, because the
    /// passthrough strips the dog's own name.
    #[test]
    fn a_bare_name_is_a_deploy_and_a_verb_still_wins() {
        assert_eq!(route(&["koji"]), Route::Deploy("koji"));
        assert_eq!(route(&["deploy", "koji"]), Route::Deploy("koji"));
        assert_eq!(route(&["survey"]), Route::Survey);
        // The escape hatch for a sheep whose name is a verb.
        assert_eq!(route(&["deploy", "survey"]), Route::Deploy("survey"));
        assert_eq!(route(&[]), Route::Poll);
    }
```

`route` is a pure `fn route<'a>(args: &[&'a str]) -> Route<'a>` holding the match, so `main` becomes a dispatch over its result. Splitting it out is what makes the ordering testable at all; a match that both decides and acts can only be tested by running it.

- [ ] **Step 6: Run the tests**

Run: `cargo test --locked`
Expected: 131 passed (128 carried plus 3 new). The count drifts as later tasks add tests, so treat it as a shape rather than a checksum.

- [ ] **Step 7: Mutation-check every branch**

Four mutations, each restored afterwards. Remember to `touch src/main.rs` after restoring a file with `git checkout`, or cargo can consider the binary fresh and a mutation check can appear to pass on a stale build.

| mutation | expected |
|---|---|
| `ROLLED_BACK` from 12 to 0 | `a_rollback_has_its_own_code...` red on the `assert_ne!(ROLLED_BACK, 0)` |
| `ROLLED_BACK` from 12 to 1 | same test red on the final `assert_ne!` |
| `Error::Config(_) => 4` to `=> 1` | `the_shared_causes_use_sheps_own_numbers` red |
| `Outcome::RolledBack { .. } => ROLLED_BACK` to `=> 0` | `the_ok_rollback_path_reports_the_same_code` red |

- [ ] **Step 8: Pin the Ok-rollback code end to end**

The unit test above proves the mapping, not that the mapping is reached. The integration tier already runs a real rollback, so this costs no extra shepherd. In `tests/integration.rs`, inside `a_release_that_cannot_come_up_is_rolled_back_and_the_old_release_serves`, at the point where the deploy's `Output` is available:

```rust
    // The rollback reached the operator's shell, not just the operator's
    // eyes. A unit test proves the mapping; only a real deploy proves the
    // mapping is on the path a rollback actually takes, and this one goes
    // through `Ok(Outcome::RolledBack)`, which exited 0 until today.
    assert_eq!(
        deployed.status.code(),
        Some(i32::from(ROLLED_BACK_EXIT)),
        "a rolled-back deploy must not report success: {}",
        String::from_utf8_lossy(&deployed.stdout)
    );
```

with, near the file's other constants:

```rust
/// `src/main.rs`'s `ROLLED_BACK`, restated because a test binary cannot see
/// a binary crate's internals. A change to one without the other fails this
/// test, which is the point.
const ROLLED_BACK_EXIT: u8 = 12;
```

Run: `SHEP_BIN="$(command -v shep)" cargo test --features integration --locked`
Expected: 5 passed. Confirm it is non-vacuous by changing `code_for_outcome`'s `RolledBack` arm to `0` and watching this test fail, then restoring.

- [ ] **Step 9: Say it in the README**

Two edits. Add a `## Platform` section after `## Security`:

```markdown
## Platform

Unix only. This is deliberate, not a gap waiting to be filled by accident: the deploy model is `rename(2)` over a symlink, the build's privilege drop is a uid and a gid, and both are Unix concepts the code uses directly rather than through a portability layer. Building on Windows fails with one sentence saying so. Windows support is planned and will be scoped on its own.
```

And in `## Usage`, after the `--watch` line:

```markdown
Exit codes follow [shep's own taxonomy](https://github.com/TurtIeSocks/shep/blob/main/docs/specs/shep-v1.md): `0` deployed or already up to date, `2` bad arguments, `4` bad configuration, `5` no daemon answered, `1` anything else. **`12` is this dog's own: the deploy was rejected and the previous release was put back.** A script that treats any nonzero code as "the deploy broke" will be wrong about `12`, where the flock is healthy on the old release.
```

- [ ] **Step 10: Run the gate and commit**

```bash
git add src/main.rs README.md tests/integration.rs
git commit -F- <<'EOF'
feat: declare Unix only, and give a rollback its own exit code

Two small things that both change what an operator sees, so they ship
together rather than as two commits nobody can review apart.

Unix only is Rin's scoping decision with a known owner and sequence, not an
unexamined gap: she has Windows support for shep itself nearly finished on
another machine and will scope the dog's story afterwards, from there. It is
the design rather than a detail, because the deploy model is rename(2) over
a symlink and the privilege drop is a uid and a gid. A compile_error! on
not(unix) is the mechanism rather than shep's own runtime "not yet
supported" idiom, and the difference is that shep compiles on Windows and
this crate cannot: shared.rs imports std::os::unix::fs::symlink
unconditionally and build.rs calls Command::uid. A runtime refusal would
mean cfg-gating every such call site, which is the port itself, so the real
choice was between one sentence and a wall of errors about symlinks.

The exit code is Rin's ruling too. A deploy that rolled back is neither a
success nor a plain failure: the requested deploy did not happen, and the
flock is healthy on the old release. Both facts matter to a script. The
important half is that the COMMON rollback path is Ok(Outcome::RolledBack),
reached by an ordinary verify timeout, and that path exited 0 until now;
only the rarer Error::RolledBack was ever nonzero.

12 joins shep's taxonomy rather than inventing a scheme. shep occupies 0 to
11 and every cause this dog shares with it keeps shep's number, so 4 is
still invalid config and 5 is still no daemon. shep's own table should
record that 12 is taken.

Pinned twice on purpose: a unit test for the mapping, and an assertion on
the real rollback integration test's exit status for the mapping being
reached. The second costs no extra shepherd, since that test already runs
one, and it is the half a unit test cannot give.
EOF
```

---

## Task 2: `[dog.<name>]`, the poll interval and retention

**Files:**
- Create: `src/config.rs`
- Modify: `src/main.rs` (module declaration)

**Interfaces:**
- Consumes: `Error` from plan one, `Daemon::dog_config` and `daemon::adopted_name`.
- Produces:
  ```rust
  pub struct DogConfig {
      pub interval: Duration,
      pub retention: usize,
  }
  impl DogConfig {
      pub fn parse(toml: &str) -> Result<Self, Error>;
  }
  pub async fn read<D: Daemon>(daemon: &D) -> Result<DogConfig, Error>;
  ```

**The shape an operator writes:**

```toml
[dog.deploy]
interval = "30s"
retention = 5
```

- [ ] **Step 1: Write the failing tests**

```rust
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
        let config = DogConfig::parse("interval = \"5m\"\nretention = 12")
            .expect("parses");
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
            let err = DogConfig::parse(&format!("retention = {count}"))
                .expect_err("refuses");
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
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --locked an_empty_section_is_the_documented_defaults`
Expected: FAIL, `DogConfig` not defined.

- [ ] **Step 3: Implement**

```rust
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
pub async fn read<D: Daemon>(daemon: &D) -> Result<DogConfig, Error> {
    let Some(name) = adopted_name(daemon).await else {
        return DogConfig::parse("");
    };
    let section = daemon.dog_config(&name).await?;
    DogConfig::parse(&section)
}
```

Delete the `#[allow(dead_code)]` from `Daemon::dog_config` and from `adopted_name` in `src/daemon.rs`, since both now have a caller, and correct the comments above them that say the poll loop is plan two.

- [ ] **Step 4: Run the tests**

Run: `cargo test --locked`
Expected: all pass.

- [ ] **Step 5: Mutation-check every refusal**

| mutation | expected |
|---|---|
| drop the `raw.retention < MINIMUM_RETENTION` block | `a_retention_below_two_is_refused_by_name` red |
| `MINIMUM_RETENTION` from 2 to 1 | same test red on the `"1"` case only, which is the point of looping over both |
| drop the `interval.is_zero()` block | `a_zero_interval_is_refused` red |
| remove `deny_unknown_fields` from `Raw` | `an_unknown_key_is_refused_and_named` red |
| `DEFAULT_INTERVAL` to 60_000 | `an_empty_section_is_the_documented_defaults` red |

- [ ] **Step 6: Commit**

```bash
git add src/config.rs src/main.rs src/daemon.rs
git commit -F- <<'EOF'
feat: read [dog.<name>], the poll interval and the retention count

Two settings, and deliberately nothing per-target. Per-target state lives in
the deploy tree's deploy.toml, because keying it to the dog's NAME means
renaming or re-adopting the dog destroys the record of every deployment it
manages, and those are unrelated things.

Thirty seconds is the interval default, settled in the spec with its
reasoning: adequate for one host, and it needs no inbound port, no HMAC
verification and no public exposure. Webhooks stay deferred, because
un-exposing a port after the fact is harder than adding one later.

Both values are refused rather than clamped when they name something that
cannot work, matching how .shepignore refuses a glob and how deploy refuses
an ungated probed target. retention below two is the one worth arguing:
retention keeps the newest N and the rollback target is the second newest,
so retention = 1 silently disables rollback. Clamping to two would have been
kinder to the parser and worse for the operator, who would go on believing
the number they wrote.

deny_unknown_fields for the same reason BuildSpec has it. `retenton = 2`
that parses to the default of five is a config somebody will believe is in
force forever.

dog_config and adopted_name lose their #![allow(dead_code)]: this is the
caller their comments promised.
EOF
```

---

## Task 3: The build cache, symlinked as `target`

**Files:**
- Modify: `src/paths.rs`, `src/shared.rs`, `src/deploy.rs`
- Modify: `src/build.rs`, `src/flockfile.rs` (stale rustdoc, step 7)

**Interfaces:**
- Produces:
  ```rust
  impl Tree {
      pub fn cache(&self) -> PathBuf;         // <root>/cache
      pub fn cache_target(&self) -> PathBuf;  // <root>/cache/target
  }
  pub fn link_cache(release: &Path, cache_target: &Path) -> Result<(), Error>;
  ```

**Why this exists, and why it is not `CARGO_TARGET_DIR`.** Measured 2026-08-26 with a throwaway crate: with `CARGO_TARGET_DIR` set, `./target` is never created and the binary lands at `<cache>/release/<bin>`, so a build command ending in `cp ./target/release/koji koji` exits 1 with "No such file or directory". Koji's `make build` is exactly that shape, and hardcoded `./target` is the common shape in Makefiles generally. With `target` symlinked at the same cache the identical `cp` exits 0. **Running a project's existing build command unmodified is the whole premise of this design**, so the dog owns a cache and links it in rather than redirecting cargo.

- [ ] **Step 1: Write the failing tests**

In `src/paths.rs`:

```rust
    /// fails if the cache moves out of the tree or changes name. Every
    /// release symlinks `target` at this one path, so a release built
    /// against a cache at one location and a later release linking another
    /// would silently lose every incremental artifact between them, which
    /// reads as "the build is just slow today".
    #[test]
    fn the_build_cache_lives_in_the_tree() {
        let tree = Tree::for_sheep(Path::new("/srv/shep"), "koji");
        assert_eq!(tree.cache(), Path::new("/srv/shep/deploy/koji/cache"));
        assert_eq!(
            tree.cache_target(),
            Path::new("/srv/shep/deploy/koji/cache/target")
        );
    }

    /// fails if the cache is ever placed inside `releases/`. It has to
    /// outlive every release it serves: retention removes worktrees under
    /// `releases/`, and a cache swept up with one would make the next
    /// deploy a from-scratch build, which for Koji is the exact outcome
    /// this whole mechanism exists to avoid.
    #[test]
    fn the_cache_is_not_inside_releases() {
        let tree = Tree::for_sheep(Path::new("/srv/shep"), "koji");
        assert!(!tree.cache().starts_with(tree.releases()));
    }
```

In `src/shared.rs`:

```rust
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

        let err = link_into(&release, &checkout, &[PathBuf::from("target")])
            .expect_err("collides");
        let shown = err.to_string();
        assert!(shown.contains(".shepignore"), "{shown}");
        assert!(shown.contains("target"), "{shown}");
    }
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --locked a_release_gets_target_linked_at_the_cache`
Expected: FAIL, `link_cache` not defined.

- [ ] **Step 3: Implement `Tree::cache` and `Tree::cache_target`**

```rust
    /// The dog's own build cache for this sheep, shared by every release.
    ///
    /// Outside `releases/` deliberately: retention removes worktrees under
    /// there, and a cache swept up with one would turn the next deploy into
    /// a from-scratch build.
    #[must_use]
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// The directory every release's `target` symlink points at.
    ///
    /// Named `target` rather than pointed at by `CARGO_TARGET_DIR` because
    /// setting that variable means `./target` is never created, and a build
    /// command ending in `cp ./target/release/koji koji` then exits 1.
    /// Measured, and the reason this is a symlink at all.
    #[must_use]
    pub fn cache_target(&self) -> PathBuf {
        self.cache().join("target")
    }
```

- [ ] **Step 4: Implement `link_cache`**

In `src/shared.rs`:

```rust
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
    if link.exists() || link.symlink_metadata().is_ok() {
        return Ok(());
    }

    fs::create_dir_all(cache_target).map_err(|source| Error::Io {
        path: cache_target.to_owned(),
        source,
    })?;

    symlink(cache_target, &link).map_err(|source| Error::Io {
        path: link,
        source,
    })
}
```

`symlink_metadata` as well as `exists`, because `exists` follows the link and answers false for a dangling one, and a dangling `target` left by an interrupted deploy still occupies the name.

- [ ] **Step 5: Reword `link_into`'s one collision**

In `link_into`'s `symlink` arm:

```rust
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
```

- [ ] **Step 6: Call it from the deploy sequence**

In `src/deploy.rs::deploy`, between the worktree add and `link_into`:

```rust
    shared::link_cache(&release, &tree.cache_target())?;
    shared::link_into(
        &release,
        &state.checkout,
        &shared::to_link(&state.checkout)?,
    )?;
```

Both are before the swap, so a failure of either leaves the running app untouched, which is the property the module doc already claims and this preserves.

- [ ] **Step 7: Correct the rustdoc that still teaches the overturned mechanism**

**The spec has been corrected; this crate's own documentation has not, and a correction has to reach every document that states the thing.** After the code change above, `cargo doc` would ship two contradictory explanations of one mechanism. Three passages, and the distinction between them matters:

| where | says | verdict |
|---|---|---|
| `src/build.rs:50-56` (module doc) | "`build.artifacts` exists because of `build.env`. A shared `CARGO_TARGET_DIR` keeps Rust compilation warm across releases ... so a declared artifact has to be copied back in" | **wrong now.** It states the overturned design as the reason a feature exists. Rewrite. |
| `src/flockfile.rs:76` (`build_spec` doc) | "`build.env` routinely names host-specific paths, a shared `CARGO_TARGET_DIR` is the example the design is built around" | **wrong now.** The design is no longer built around it. Pick a different example. |
| `src/flockfile.rs:440` (a test doc) | "the design is built around (a shared `CARGO_TARGET_DIR`, and copying the binary it produces)" | **wrong now.** Same phrase, same fix. |

**What is NOT stale, and must not be swept up:** `artifact_source`, its `CARGO_TARGET_DIR` resolution, and every test that sets the variable as an env value. `build.env` and `build.artifacts` are both real, both still work, and an operator whose build does not hardcode `./target` may still set the variable themselves. The dog simply does not set it. A blind grep-and-replace over `CARGO_TARGET_DIR` would delete working behaviour along with the stale claims.

Replacement for `src/build.rs:50-56`:

```rust
//! - **`build.artifacts` exists for a build that writes outside the
//!   release.** Warm Rust builds do NOT need it: each sheep gets a
//!   dog-owned cache symlinked in as `target` (see
//!   [`crate::paths::Tree::cache_target`]), so compilation stays warm
//!   across releases AND a hardcoded `./target/release/koji` still
//!   resolves, with nothing copied back. Setting `CARGO_TARGET_DIR`
//!   instead was measured on 2026-08-26 and rejected: with it set
//!   `./target` is never created, so Koji's own `make build`, which ends
//!   in `cp ./target/release/koji koji`, exits 1. `artifacts` remains for
//!   the builds that genuinely do put their output somewhere the release
//!   cannot see, including one an operator points elsewhere themselves.
```

For both `flockfile.rs` sites, replace the `CARGO_TARGET_DIR` example with one this design actually uses, such as a registry token or a `NODE_ENV`, and drop "the design is built around".

- [ ] **Step 8: Run the tests**

Run: `cargo test --locked`
Expected: all pass.

Then `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features` and **read the rendered `build.rs` and `flockfile.rs` pages**, not just the exit status. The failure this step exists for is prose, and a doc build cannot see prose that is merely false.

- [ ] **Step 9: Mutation-check**

| mutation | expected |
|---|---|
| make `link_cache` return `Ok(())` immediately | `a_release_gets_target_linked_at_the_cache` red |
| drop `link_cache`'s existing-path early return | `a_release_that_ships_its_own_target_is_left_alone` red |
| use only `link.exists()`, dropping `symlink_metadata` | neither test moves; add a dangling-link case if you want that branch pinned, or delete the branch. Do not leave an unpinned branch claiming to handle a case. |
| drop the `AlreadyExists` rewording in `link_into` | `a_checkout_sharing_target_says_how_to_fix_it` red |

- [ ] **Step 10: Commit**

```bash
git add src/paths.rs src/shared.rs src/deploy.rs src/build.rs src/flockfile.rs
git commit -F- <<'EOF'
feat: give each sheep a build cache, symlinked into every release as target

Not CARGO_TARGET_DIR, and the difference was measured rather than reasoned
about. With that variable set, ./target is never created and the binary
lands in the cache, so a build command ending in `cp ./target/release/koji
koji` exits 1 with "No such file or directory". Koji's own make build is
exactly that, and hardcoded ./target is the common shape in Makefiles.
Running a project's existing build command unmodified is the premise of this
whole design, so the dog owns a cache and links it in.

The cache lives outside releases/ because retention removes worktrees under
there, and a cache swept up with one turns the next deploy into a
from-scratch build, which for Koji is the exact outcome this exists to
avoid.

A release that ships its own committed target/ keeps it and gets no cache.
That repository is unusual and its own business, and refusing the deploy
over it would be this crate deciding a repository's layout for it.

The other collision is the operator's own target directory being shared in,
which happens when a checkout has no .shepignore. That one is a real
misconfiguration, since the next release's build would write through the
link and replace what the current release is serving, so link_into now says
so and names the one line that fixes it instead of reporting "File exists".

Also corrects three rustdoc passages that still taught the overturned
mechanism: build.rs's module doc explained build.artifacts as existing
BECAUSE of a shared CARGO_TARGET_DIR, and flockfile.rs called that variable
"the example the design is built around" twice. The spec was corrected days
ago and the crate's own docs were not, which would have shipped two
contradictory explanations of one mechanism on docs.rs. A correction has to
reach every document that states the thing, and this ledger has now recorded
that lesson three times.

What deliberately did NOT change: artifact_source, its CARGO_TARGET_DIR
resolution, and the tests that set the variable. build.env and
build.artifacts both still work and an operator may still set it themselves.
Only the claim that the DOG sets it was ever wrong.
EOF
```

---

## Task 4: Retention, and where it sits relative to the boundary

**Files:**
- Create: `src/retention.rs`
- Modify: `src/main.rs` (module declaration), `src/deploy.rs`, `src/git.rs`

**Interfaces:**
- Consumes: `Tree`, `git::worktree_remove`, `git::worktree_prune`, `swap::resolve`.
- Produces:
  ```rust
  pub fn prune(tree: &Tree, keep: usize) -> Result<Vec<String>, Error>;
  fn doomed(releases: &[(String, SystemTime)], keep: usize, live: Option<&str>) -> Vec<String>;
  ```

**Where this sits, and why it is not inside `land`.** The rollback boundary exists for fallible steps whose failure has to be *undone*: `land` cannot return an error precisely so that a step added after the swap has nowhere to escape the undo decision to. Retention is the one piece of post-swap work that genuinely does not belong inside it, and the reason is specific rather than convenient. It runs after `state.write`, at which point `current`, `deploy.toml` and the running process all agree, the deploy is over, and a prune failure has nothing to undo. Putting it inside `land` would force it to be expressible as a `Landed` variant, which would mean inventing an undo for an operation that has none.

So it runs in `deploy`'s `Landed::Verified` arm, **after** the record is written, and **a prune failure never fails a deploy**. It warns and returns. That is not best-effort hand-waving: a deploy that succeeded, verified and recorded itself did in fact succeed, and turning it into an error because a worktree could not be removed would report a failure that did not happen, which is the species of every serious finding in the engine plan.

**Why nothing running can be pruned.** Retention only runs after a *verified* deploy, and verification requires full turnover: every instance under a pid the pre-reload generation never had. A process's `cwd` is resolved at spawn, so an instance started before a swap keeps executing from the old release even after `current` moves, and that is exactly the hazard here. Full turnover means no such instance survives by the time this runs. **This property is why retention must never be moved to any other point in the sequence**, and the module doc says so.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// Three releases, newest first by the second field.
    fn releases(names: &[&str]) -> Vec<(String, SystemTime)> {
        let base = SystemTime::UNIX_EPOCH;
        names
            .iter()
            .enumerate()
            .map(|(age, name)| {
                (
                    (*name).to_owned(),
                    base + Duration::from_secs(1000 - age as u64),
                )
            })
            .collect()
    }

    /// fails if retention stops keeping the newest `keep`. Everything else
    /// in this module is a consequence of getting this ordering right, and
    /// getting it backwards would delete the live release and keep the
    /// ancient ones.
    #[test]
    fn the_newest_releases_survive() {
        let all = releases(&["new", "old", "older", "ancient"]);
        assert_eq!(doomed(&all, 2, None), vec!["older", "ancient"]);
    }

    /// fails if a release still named by `current` can be pruned. Removing
    /// it leaves the sheep with a `cwd` that resolves to nothing, so the
    /// next restart cannot start it at all, and the deploy that caused it
    /// reported success minutes earlier.
    #[test]
    fn the_live_release_is_never_pruned_whatever_its_age() {
        let all = releases(&["new", "old", "older", "ancient"]);
        assert_eq!(doomed(&all, 2, Some("ancient")), vec!["older"]);
        assert!(!doomed(&all, 1, Some("ancient")).contains(&"ancient".to_owned()));
    }

    /// fails if the rollback target is pruned along with the rest. The
    /// second newest release IS what a failed deploy returns to, and
    /// `config` refuses a retention below two for this reason, so this
    /// pins the other end of the same rule.
    #[test]
    fn keeping_two_leaves_a_rollback_target() {
        let all = releases(&["new", "old", "older"]);
        let survivors: Vec<String> = all
            .iter()
            .map(|(name, _)| name.clone())
            .filter(|name| !doomed(&all, 2, None).contains(name))
            .collect();
        assert_eq!(survivors, vec!["new", "old"]);
    }

    /// fails if a tree with fewer releases than the retention count starts
    /// removing things. The first few deploys of every target are this
    /// case.
    #[test]
    fn a_young_tree_loses_nothing() {
        assert!(doomed(&releases(&["new", "old"]), 5, None).is_empty());
        assert!(doomed(&[], 5, None).is_empty());
    }

    /// fails if `prune` stops actually removing directories, or removes
    /// something it should not. The pure function above pins the decision;
    /// this pins that the decision is carried out against real worktrees,
    /// which is where `--force` matters: a built worktree is always dirty.
    #[test]
    fn prune_removes_real_worktrees_and_leaves_the_live_one() {
        let (tree, shas) = fixture_tree_with_releases(4);
        let removed = prune(&tree, 2).expect("prunes");

        assert_eq!(removed.len(), 2);
        assert!(tree.release(&shas[3]).exists(), "the newest survives");
        assert!(tree.release(&shas[2]).exists(), "so does the rollback target");
        assert!(!tree.release(&shas[0]).exists(), "the oldest is gone");
    }
}
```

`fixture_tree_with_releases` builds a real bare repo with `n` commits, adds a worktree per sha under `tree.releases()` in commit order, writes a file into each so they are dirty the way a built release is, and points `current` at the newest. Put it in this module's test block; every other module in this crate keeps its fixtures local rather than sharing them, and `src/git.rs`'s own `fixture_repo_with_commits` is the shape to copy.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --locked the_newest_releases_survive`
Expected: FAIL, `doomed` not defined.

- [ ] **Step 3: Implement**

```rust
//! Reclaiming release worktrees once they are past the retention count.
//!
//! # Why this runs only after a VERIFIED deploy
//!
//! A process's working directory is resolved when it is spawned, so an
//! instance started before a swap goes on executing from the old release
//! even after `current` moves. Removing that release out from under a
//! running process is the hazard this module has to avoid, and it avoids
//! it by when it runs rather than by checking: verification requires full
//! turnover, every instance under a pid the pre-reload generation never
//! had, so by the time this is called there is no instance left that was
//! spawned from an older RELEASE. Move this call anywhere earlier in the
//! sequence and that argument stops holding.
//!
//! "Retention could delete what something is running from" is the right
//! worry to have about this module, so here is the whole of the answer:
//! this reads `releases/` and nothing else. A sheep's pre-adoption
//! instance runs from the operator's own checkout, which is not in there;
//! `current` is spared explicitly whatever its age; and every other
//! instance was spawned from a release this deploy just turned over. There
//! is no path by which a running process's working directory is a
//! candidate here.
//!
//! # Why a failure here never fails a deploy
//!
//! It runs after `deploy.toml` is written, at which point `current`, the
//! record and the running process all agree and the deploy is genuinely
//! over. A worktree that cannot be removed costs disk, not correctness,
//! and reporting the deploy as failed because of it would report a failure
//! that did not happen.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::error::Error;
use crate::paths::Tree;
use crate::{git, swap};

/// Removes every release beyond the newest `keep`, and answers with the
/// shas it removed.
///
/// Never removes the release `current` names, whatever its age. That is
/// belt and braces rather than the primary guard (the newest release is
/// the one just deployed, so ordering alone would spare it), and it exists
/// because the cost of being wrong is a sheep whose `cwd` resolves to
/// nothing.
///
/// # Errors
/// [`Error::Io`], naming `releases/`, if it cannot be listed.
/// [`Error::Git`] if a worktree cannot be removed or the bookkeeping
/// cannot be pruned. A caller is expected to warn on these rather than
/// fail the deploy that triggered them: see this module's own doc.
pub fn prune(tree: &Tree, keep: usize) -> Result<Vec<String>, Error> {
    let live = swap::resolve(&tree.current())?;
    let live = live.as_deref().and_then(sha_of);

    let mut found = Vec::new();
    let releases = tree.releases();
    let entries = match fs::read_dir(&releases) {
        Ok(entries) => entries,
        // A target with no releases directory has nothing to reclaim,
        // which is every target before its first deploy.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::Io { path: releases, source }),
    };

    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: releases.clone(),
            source,
        })?;
        let (Some(name), Ok(modified)) = (
            entry.file_name().to_str().map(str::to_owned),
            entry.metadata().and_then(|meta| meta.modified()),
        ) else {
            continue;
        };
        found.push((name, modified));
    }

    let mut removed = Vec::new();
    for sha in doomed(&found, keep, live.as_deref()) {
        git::worktree_remove(&tree.git(), &tree.release(&sha))?;
        removed.push(sha);
    }

    if !removed.is_empty() {
        git::worktree_prune(&tree.git())?;
    }

    Ok(removed)
}

/// Which of `releases` to remove: everything past the newest `keep`, minus
/// `live`.
///
/// Ordered by directory modification time rather than by commit date. A
/// release's directory is created when its worktree is added and written
/// into by its build, so its mtime is when this host last worked on it,
/// which is the question retention is asking. Commit date is a fact about
/// the repository and would put a redeployed older sha in the wrong place.
fn doomed(releases: &[(String, SystemTime)], keep: usize, live: Option<&str>) -> Vec<String> {
    let mut ordered: Vec<&(String, SystemTime)> = releases.iter().collect();
    ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    ordered
        .into_iter()
        .skip(keep)
        .map(|(name, _)| name.clone())
        .filter(|name| Some(name.as_str()) != live)
        .collect()
}

/// The sha a release path names, which is its last component.
fn sha_of(release: &Path) -> Option<String> {
    release
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}
```

The `.then_with(|| a.0.cmp(&b.0))` in the sort is not decoration: two worktrees created inside one filesystem timestamp tick would otherwise order arbitrarily, and a test that passes on one run and fails on another is worse than no test.

- [ ] **Step 4: Call it from the deploy sequence**

In `src/deploy.rs`, in the `Landed::Verified` arm, after `state.write`:

```rust
        Landed::Verified => {
            state.deployed = Some(head.clone());
            state.write(&tree.state_file())?;
            // After the record, and never fatal. See `crate::retention`'s
            // own doc for both halves: full turnover means nothing is
            // still executing from an older release, and a deploy that has
            // already verified and recorded itself did succeed.
            if let Err(err) = retention::prune(tree, keep) {
                eprintln!("shep-deploy: {sheep}: could not reclaim old releases: {err}");
            }
            Ok(Outcome::Deployed { sha: head })
        }
```

`deploy` gains a `keep: usize` parameter for this, threaded from the caller: `main::deploy_once` passes `config::read(&daemon).await?.retention`, and the poll loop (Task 11) passes the value it already read once. Update `deploy`'s doc comment to say what `keep` is and that a prune failure is reported rather than returned.

- [ ] **Step 5: Run the tests**

Run: `cargo test --locked`
Expected: all pass, including plan one's existing deploy tests once their `deploy(...)` calls gain the new argument.

- [ ] **Step 6: Mutation-check every branch**

| mutation | expected |
|---|---|
| reverse the sort to oldest-first | `the_newest_releases_survive` red |
| drop the `filter(|name| Some(..) != live)` | `the_live_release_is_never_pruned_whatever_its_age` red |
| `.skip(keep)` to `.skip(keep.saturating_sub(1))` | `keeping_two_leaves_a_rollback_target` red |
| drop `--force` from `git::worktree_remove` | `prune_removes_real_worktrees_and_leaves_the_live_one` red, because the fixture's releases are dirty exactly as a built one is. This re-pins plan one's own guard from the caller that finally has one. |
| change the `Landed::Verified` arm's `if let Err` to `?` | nothing goes red today. **Add a test that a prune failure does not fail the deploy** rather than leaving that unpinned: point `tree.git()` at a directory that is not a repository, run a deploy that verifies, and assert `Outcome::Deployed`. |

- [ ] **Step 7: Commit**

```bash
git add src/retention.rs src/deploy.rs src/main.rs
git commit -F- <<'EOF'
feat: reclaim release worktrees past the retention count

Where this sits was the whole decision. The rollback boundary exists for
fallible steps whose failure has to be UNDONE, which is why land cannot
return an error at all. Retention is the one piece of post-swap work that
genuinely does not belong inside it: it runs after deploy.toml is written,
when current, the record and the running process all agree, and a prune
failure has nothing to undo. Expressing it as a Landed variant would mean
inventing an undo for an operation that has none.

So it runs in the Verified arm after the record, and a failure warns rather
than failing the deploy. That is not best-effort hand-waving. A deploy that
verified and recorded itself did succeed, and failing it because a worktree
would not delete reports a failure that did not happen.

Nothing running can be pruned, and the guarantee comes from WHEN this runs
rather than from a check. A process's cwd is resolved at spawn, so an
instance started before a swap goes on executing from the old release after
current moves. Verification requires full turnover, so no such instance
survives by the time this is called. Moving this call earlier breaks that
argument, and the module doc says so where somebody would move it.

Ordered by directory mtime, not commit date: mtime is when this host last
worked on the release, which is the question, where commit date is a fact
about the repository and would misplace a redeployed older sha. Ties break
on name so two worktrees created inside one timestamp tick cannot order
arbitrarily and make a flaky test.

The live release is spared regardless of age. Ordering alone would already
spare it, since the newest release is the one just deployed, so this is belt
and braces, taken because the cost of being wrong is a sheep whose cwd
resolves to nothing.
EOF
```

---

## Task 5: Reading shep's muster roll

**Files:**
- Create: `src/roll.rs`
- Modify: `src/daemon.rs`, `src/main.rs`, `Cargo.toml`

**Interfaces:**
- Produces:
  ```rust
  pub async fn registered<D: Daemon>(daemon: &D) -> Result<BTreeMap<String, AppConfig>, Error>;
  ```
  and on `Daemon`:
  ```rust
  async fn save_roll(&self) -> Result<PathBuf, Error>;
  ```

**Why this module has to exist.** shep says it itself, in `shep-daemon/src/snapshot.rs`: "nothing in a `ProcessInfo` can reproduce the `AppConfig` a sheep came from, which is exactly what a roll needs". `ProcessInfo` carries id, name, status, pid, restarts, uptime, fold, log paths, cpu, memory, dog marker, lambs and last exit. It carries **no `cwd` and no `script`**. The survey needs `cwd` to say whether a sheep runs from a git checkout, and opt-in needs both to record what to restore. There is no request that answers either, and the muster roll is the only place the daemon writes them down.

**Why `SaveRoll` first.** The snapshot writer debounces, so the roll on disk can lag reality by an unspecified amount. `Request::SaveRoll` is documented as "write the muster roll now, bypassing the snapshot writer's debounce", and `Response::RollSaved { path, apps }` hands back the absolute path the daemon actually wrote. Asking for the path rather than rebuilding it from `ShepPaths` means the dog agrees with the daemon about where the file is even if the daemon was started with a different home than this process resolved.

**Name the coupling honestly, because this crate has been bitten by exactly this shape before.** The roll's outer envelope (`{ version, saved_at_ms, apps: [{ app, instances_running }] }`) is `shep-daemon`'s type, and this crate does not and should not depend on `shep-daemon`. So it mirrors the two fields it needs and ignores the rest. The inner `AppConfig` is `shep-core`'s and is shared properly. The exposure is a newer shepherd writing an `AppConfig` field this crate's `shep-core` does not know, which `deny_unknown_fields` would refuse. Task 6 turns that into a named, survivable failure rather than a crash.

- [ ] **Step 1: Add the dependency, and close the version skew**

```toml
serde_json = "1"
```

Already in `Cargo.lock` at 1.0.151 as a transitive dependency of `shep-core`, so this adds a direct edge and no new crate to the tree.

In the same edit, bump `shep-client` to the newest published version and run `cargo update -p shep-client`. The lock pins 0.1.0 while newer patch releases exist, and Task 13 installs the newest `shep`, so the tier currently drives a new daemon from a dog built against the oldest client. The protocol was checked before this plan was written and is byte-identical between 0.1.0 and 0.1.2 (no `Request` variant added or changed, no new `ProcessInfo` field, no smits), so this is expected to be inert. **If it is not inert, stop and report what moved** rather than pinning back: a protocol change between those versions would be information every later task needs.

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A roll as the daemon writes one, with the envelope fields this
    /// crate ignores present so the test proves they are ignored rather
    /// than merely absent.
    fn roll_json(apps: &str) -> String {
        format!("{{\"version\":1,\"saved_at_ms\":1787756216990,\"apps\":[{apps}]}}")
    }

    /// fails if the roll stops yielding the one thing it exists for: the
    /// cwd and script a sheep was REGISTERED with. `ProcessInfo` carries
    /// neither, and shep's own snapshot module says so, so losing this
    /// leaves the survey unable to tell a git checkout from anything else
    /// and opt-in unable to record what to restore.
    #[test]
    fn a_roll_yields_each_sheeps_registered_cwd_and_script() {
        let text = roll_json(
            "{\"app\":{\"name\":\"bpm\",\"script\":\"bun .\",\"cwd\":\"/srv/reactmap\"},\
             \"instances_running\":2}",
        );
        let apps = parse(&text).expect("parses");
        let bpm = apps.get("bpm").expect("bpm is in the roll");
        assert_eq!(bpm.cwd.as_deref(), Some("/srv/reactmap"));
        assert_eq!(bpm.script, "bun .");
    }

    /// fails if the envelope's own fields are treated as app config, or if
    /// an unknown envelope field breaks the parse. `instances_running` is
    /// shep-daemon's and this crate has no use for it; a field added to
    /// that envelope later must not stop the dog reading the roll.
    #[test]
    fn envelope_fields_this_crate_does_not_use_are_ignored() {
        let text = "{\"version\":1,\"saved_at_ms\":0,\"something_new\":true,\"apps\":\
                    [{\"app\":{\"name\":\"web\",\"script\":\"./s\"},\"instances_running\":1,\
                    \"another_new_one\":[]}]}";
        assert!(parse(text).expect("parses").contains_key("web"));
    }

    /// fails if an empty flock is an error rather than an empty map. A
    /// shepherd with nothing registered writes `"apps":[]`, and the survey
    /// has to report an empty flock rather than a failure.
    #[test]
    fn an_empty_flock_is_an_empty_map_not_an_error() {
        assert!(parse(&roll_json("")).expect("parses").is_empty());
    }

    /// fails if a roll this crate cannot understand produces a bare serde
    /// message. The likeliest cause by far is a newer shepherd writing an
    /// AppConfig field this crate's shep-core does not know, and an
    /// operator meeting "unknown field `foo`" with no context has nothing
    /// to act on. The message names the file and the likely cause.
    #[test]
    fn an_unreadable_roll_names_the_file_and_the_likely_cause() {
        let err = read(Path::new("/srv/shep/flock.json"), "{\"apps\":[{\"app\":{}}]}")
            .expect_err("refuses");
        let shown = err.to_string();
        assert!(shown.contains("/srv/shep/flock.json"), "{shown}");
        assert!(shown.contains("newer"), "{shown}");
    }
}
```

- [ ] **Step 3: Run and watch them fail**

Run: `cargo test --locked a_roll_yields_each_sheeps_registered_cwd_and_script`
Expected: FAIL, `parse` not defined.

- [ ] **Step 4: Add `save_roll` to `Daemon`**

In `src/daemon.rs`, on the trait:

```rust
    /// Ask the shepherd to write its muster roll now, and answer with the
    /// path it wrote.
    ///
    /// The snapshot writer debounces, so the roll on disk can lag reality;
    /// `SaveRoll` is documented as bypassing that. The path comes back from
    /// the daemon rather than being rebuilt from [`ShepPaths`] so the dog
    /// agrees with the daemon about where the file is even when the two
    /// resolved `$SHEP_HOME` differently.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    async fn save_roll(&self) -> Result<PathBuf, Error>;
```

and on `Live`:

```rust
    async fn save_roll(&self) -> Result<PathBuf, Error> {
        match self.0.request(Request::SaveRoll).await? {
            Response::RollSaved { path, .. } => Ok(PathBuf::from(path)),
            other => Err(unexpected("SaveRoll", &other)),
        }
    }
```

Add a `Response::RollSaved { .. } => "a RollSaved".to_owned()` arm to `named`, so the roll's path never lands in an error message by way of the `#[non_exhaustive]` `Debug` fallback, and extend `a_listing_is_named_with_its_length`'s sibling test to cover it. Add the method to every existing test double in the crate; `unimplemented!()` is correct for the doubles that do not answer it.

- [ ] **Step 5: Implement `src/roll.rs`**

```rust
//! Reading shep's muster roll, which is the only place a dog can see the
//! `AppConfig` a sheep was REGISTERED with.
//!
//! shep states the reason itself, in its own `snapshot.rs`: "nothing in a
//! `ProcessInfo` can reproduce the `AppConfig` a sheep came from, which is
//! exactly what a roll needs". [`ProcessInfo`] carries status, pid, uptime
//! and log paths, and carries neither `cwd` nor `script`. The survey needs
//! the first to tell a git checkout from anything else, and opt-in needs
//! both to record what a removal should restore.
//!
//! # The coupling, named rather than glossed
//!
//! The roll's envelope is `shep-daemon`'s type and this crate does not
//! depend on `shep-daemon`, so the two fields needed here are mirrored and
//! everything else is ignored. That direction is safe: a field added to
//! the envelope later cannot break this. The inner [`AppConfig`] is
//! `shep-core`'s and is shared properly, but it is `deny_unknown_fields`,
//! so a NEWER shepherd writing a field this crate's `shep-core` does not
//! know would be refused. [`read`] turns that into a message naming the
//! file and the likely cause rather than a bare serde complaint, because
//! an operator meeting "unknown field" with no context has nothing to act
//! on.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;
use shep_client::shep_core::config::AppConfig;

use crate::daemon::Daemon;
use crate::error::Error;

/// The roll's envelope, mirrored down to the one field this crate reads.
#[derive(Deserialize)]
struct Roll {
    apps: Vec<Entry>,
}

/// One sheep's entry. `instances_running` is shep-daemon's and unused here.
#[derive(Deserialize)]
struct Entry {
    app: AppConfig,
}

/// Every registered sheep's config, keyed by name, freshly written.
///
/// Asks the shepherd to write the roll first, so this never reads a
/// debounced snapshot that predates a sheep the caller just started.
///
/// # Errors
/// Whatever [`Daemon::save_roll`] returns, plus [`Error::Io`] naming the
/// roll if it cannot be read and [`Error::Config`] if it cannot be parsed.
pub async fn registered<D: Daemon>(daemon: &D) -> Result<BTreeMap<String, AppConfig>, Error> {
    let path = daemon.save_roll().await?;
    let text = fs::read_to_string(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    read(&path, &text)
}

/// [`parse`], with the failure named against `path`.
///
/// # Errors
/// [`Error::Config`] naming `path` and the likeliest cause.
fn read(path: &Path, text: &str) -> Result<BTreeMap<String, AppConfig>, Error> {
    parse(text).map_err(|source| {
        Error::Config(format!(
            "{}: this shepherd's muster roll is not one this dog can read ({source}). The \
             likeliest cause is a newer shepherd than the shep-core this dog was built against: \
             the roll carries each sheep's own app config, and an app field added since refuses \
             to parse rather than being ignored. Rebuilding or reinstalling shep-deploy against \
             the current shep fixes it.",
            path.display()
        ))
    })
}

/// The roll's apps, keyed by name.
///
/// # Errors
/// [`serde_json::Error`] if the text is not a roll this crate understands.
fn parse(text: &str) -> Result<BTreeMap<String, AppConfig>, serde_json::Error> {
    let roll: Roll = serde_json::from_str(text)?;
    Ok(roll
        .apps
        .into_iter()
        .map(|entry| (entry.app.name.clone(), entry.app))
        .collect())
}
```

- [ ] **Step 6: Run the tests**

Run: `cargo test --locked`
Expected: all pass.

- [ ] **Step 7: Mutation-check**

| mutation | expected |
|---|---|
| key the map by something other than `app.name` (say a running counter as a string) | `a_roll_yields_each_sheeps_registered_cwd_and_script` red |
| add `#[serde(deny_unknown_fields)]` to `Roll` and to `Entry` | `envelope_fields_this_crate_does_not_use_are_ignored` red, which is the point: this crate must tolerate shep-daemon growing its envelope |
| drop the `map_err` in `read` and return the serde error directly | `an_unreadable_roll_names_the_file_and_the_likely_cause` red |
| have `save_roll` build the path from `ShepPaths` instead of reading `RollSaved`'s | nothing unit-testable moves. Pinned instead by Task 13's integration test, which drives a real shepherd in a temp home. Say so rather than claiming a unit test covers it. |

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/roll.rs src/daemon.rs src/main.rs
git commit -F- <<'EOF'
feat: read the registered AppConfig out of shep's muster roll

This module exists because shep says it has to. Its own snapshot.rs: nothing
in a ProcessInfo can reproduce the AppConfig a sheep came from. ProcessInfo
carries status, pid, uptime and log paths and carries neither cwd nor
script, and those are exactly the two fields the survey and opt-in need.
There is no request that answers either, so the roll is the only source.

SaveRoll first, because the snapshot writer debounces and the roll on disk
can lag reality. The path comes back from the daemon rather than being
rebuilt from ShepPaths, so the dog agrees with the daemon about where the
file is even when the two resolved SHEP_HOME differently, which is the same
class of drift that produced a wrong rollback report in the engine plan.

The coupling is named rather than glossed. The envelope is shep-daemon's
type and this crate mirrors two fields of it and ignores the rest, which is
safe in that direction: a field added later cannot break it, and there is a
test that proves the ignoring rather than merely the absence. The inner
AppConfig is shep-core's, shared properly, and it is deny_unknown_fields, so
a NEWER shepherd is the one real exposure. That is turned into a message
naming the file and the likely fix, because "unknown field `foo`" with no
context is not something an operator can act on.

serde_json was already in the lock file as a transitive dep of shep-core, so
this is a direct edge and no new crate in the tree.
EOF
```

---

## Task 6: The flock survey

**Files:**
- Create: `src/survey.rs`
- Modify: `src/main.rs`, `src/paths.rs`, `README.md`

**Interfaces:**
- Consumes: `roll::registered`, `State`, `Tree`.
- Produces:
  ```rust
  pub enum Standing {
      Watched { branch: String, sha: Option<String> },
      Manual { branch: String, sha: Option<String> },
      NeedsSetup,
      Eligible,
      NotEligible(String),
  }
  pub fn classify(shep_home: &Path, app: &AppConfig) -> Standing;
  pub fn render(rows: &[(String, Standing)]) -> String;
  pub async fn survey<D: Daemon>(daemon: &D, shep_home: &Path) -> Result<String, Error>;
  ```
  and on `Tree`, a free function in `paths`:
  ```rust
  pub fn targets(shep_home: &Path) -> Result<Vec<String>, Error>;
  ```

**Discovery is read-only and deploying requires opt-in.** The survey starts nothing, registers nothing, and writes nothing. A sheep whose `cwd` is not a git checkout is reported and left entirely alone: turning a directory into a checkout is the operator's decision, not a dog's.

**What "declares a deploy block" means.** The spec's `needs setup` row distinguishes a checkout that declares one from a checkout where nothing does, and there is no `[deploy]` block anywhere in this design. This plan reads it as **the checkout ships a `Flockfile.toml`**, which is what the spec's own motivation argues: upstream shipping the app definition is what turns "how do I run this" from a README section into a file. Flagged in "Where this plan knowingly departs from the spec".

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// An `AppConfig` with just the two fields this module reads.
    fn app(name: &str, cwd: Option<&str>) -> AppConfig {
        let mut app: AppConfig = toml::from_str(&format!(
            "name = {name:?}\nscript = \"./run.sh\""
        ))
        .expect("parses");
        app.cwd = cwd.map(str::to_owned);
        app
    }

    /// fails if a sheep whose cwd is not a git checkout is offered for
    /// deployment. Turning a directory into a checkout is the operator's
    /// decision, and a dog that started doing it for them would be acting
    /// on a directory it was never pointed at.
    #[test]
    fn a_cwd_that_is_not_a_checkout_is_not_eligible_and_says_why() {
        let home = tempfile::tempdir().expect("tempdir");
        let plain = tempfile::tempdir().expect("tempdir");
        let standing = classify(home.path(), &app("legacy", plain.path().to_str()));
        let Standing::NotEligible(why) = standing else {
            panic!("expected NotEligible, got {standing:?}");
        };
        assert!(why.contains(plain.path().to_str().expect("utf-8")), "{why}");
        assert!(why.contains("git"), "{why}");
    }

    /// fails if a sheep shep records no working directory for is reported
    /// as anything but ineligible. There is nothing to inspect, so there is
    /// nothing to offer.
    #[test]
    fn a_sheep_with_no_recorded_cwd_is_not_eligible() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            classify(home.path(), &app("odd", None)),
            Standing::NotEligible(_)
        ));
    }

    /// fails if a checkout that ships its own Flockfile stops being
    /// distinguished from one that does not. That distinction is the whole
    /// argument for building this: upstream shipping the app definition is
    /// what turns "how do I run this" from a README section people misread
    /// into a file.
    #[test]
    fn a_checkout_shipping_a_flockfile_needs_setup_and_one_without_is_merely_eligible() {
        let home = tempfile::tempdir().expect("tempdir");

        let declared = checkout_fixture(&[("Flockfile.toml", "[[app]]\nname='x'\nscript='y'\n")]);
        assert!(matches!(
            classify(home.path(), &app("reactmap", declared.path().to_str())),
            Standing::NeedsSetup
        ));

        let bare = checkout_fixture(&[]);
        assert!(matches!(
            classify(home.path(), &app("koji", bare.path().to_str())),
            Standing::Eligible
        ));
    }

    /// fails if a sheep that is ALREADY a deploy target is offered as
    /// eligible. Opting in twice would clone over a live tree, and the row
    /// an operator most wants from this command is which of their sheep are
    /// already being watched.
    #[test]
    fn an_existing_target_reports_its_watch_mode_not_its_eligibility() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_fixture(&[]);
        write_target(home.path(), "bpm", Watch::Manual, Some("a1b2c3d4e5f6"));

        let standing = classify(home.path(), &app("bpm", checkout.path().to_str()));
        assert!(
            matches!(standing, Standing::Manual { .. }),
            "{standing:?}"
        );
    }

    /// fails if the rendered table stops naming every standing, or stops
    /// reading as columns. This is the entire output of a command whose
    /// only job is to be read, and an exact string is what keeps a later
    /// change from quietly dropping the reason column, which is the half
    /// that tells an operator what to do next.
    #[test]
    fn the_rendered_survey_is_three_aligned_columns() {
        let rows = vec![
            (
                "bpm".to_owned(),
                Standing::Watched {
                    branch: "main".to_owned(),
                    sha: Some("a1b2c3d4e5f6".to_owned()),
                },
            ),
            ("reactmap".to_owned(), Standing::NeedsSetup),
            ("koji".to_owned(), Standing::Eligible),
            (
                "legacy".to_owned(),
                Standing::NotEligible("/opt/legacy is not a git repository".to_owned()),
            ),
        ];
        assert_eq!(
            render(&rows),
            "bpm       watched       main@a1b2c3, deploys on every new commit\n\
             reactmap  needs setup   a git checkout that ships a Flockfile\n\
             koji      eligible      a git checkout, nothing declares a deploy\n\
             legacy    not eligible  /opt/legacy is not a git repository\n"
        );
    }

    /// fails if an empty flock renders as an empty string. A command that
    /// prints nothing is indistinguishable from one that failed silently,
    /// and an empty flock is the ordinary state of a fresh shepherd.
    #[test]
    fn an_empty_flock_says_so() {
        assert!(render(&[]).contains("no sheep"));
    }
}
```

`checkout_fixture` makes a tempdir, runs `git init -q` in it, and writes the given files. `write_target` creates `<home>/deploy/<sheep>/` and writes a `deploy.toml` through `State::write`, so the fixture and the real reader agree by construction rather than by a hand-written string that can drift.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --locked the_rendered_survey_is_three_aligned_columns`
Expected: FAIL, `render` not defined.

- [ ] **Step 3: Implement `paths::targets`**

```rust
/// Every sheep that is a deploy target, by name.
///
/// A target is a directory under `<shep_home>/deploy` holding a
/// `deploy.toml`. Reading the directory rather than a list held anywhere
/// else is what makes a tree self-describing: it survives the dog being
/// rehomed, re-adopted under a different name, or replaced by a different
/// deploy dog entirely, which is the same reasoning that put per-target
/// state in the tree instead of in `[dog.<name>]`.
///
/// # Errors
/// [`Error::Io`], naming `<shep_home>/deploy`, if it exists but cannot be
/// listed. An absent directory is an empty list, not an error: that is
/// every shepherd with no targets yet.
pub fn targets(shep_home: &Path) -> Result<Vec<String>, Error> {
    let root = shep_home.join("deploy");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::Io { path: root, source }),
    };

    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: root.clone(),
            source,
        })?;
        if !entry.path().join("deploy.toml").is_file() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            found.push(name.to_owned());
        }
    }
    found.sort();
    Ok(found)
}
```

- [ ] **Step 4: Implement `classify` and `render`**

```rust
/// Where one sheep stands with respect to this dog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Already a target, polled for new commits.
    Watched {
        /// The branch it tracks.
        branch: String,
        /// The sha it is deployed at, or `None` before its first deploy.
        sha: Option<String>,
    },
    /// Already a target, deployed only when asked.
    Manual {
        /// The branch it tracks.
        branch: String,
        /// As [`Self::Watched::sha`].
        sha: Option<String>,
    },
    /// A git checkout whose repository ships a `Flockfile.toml`, so
    /// upstream has said how to run it and nothing has taken it over yet.
    NeedsSetup,
    /// A git checkout that could be taken over, where nothing declares a
    /// deploy.
    Eligible,
    /// Left alone, and why.
    NotEligible(String),
}

/// Where `app` stands, without touching anything.
///
/// Order matters and is not arbitrary. An existing target is answered
/// first, because a sheep already being deployed is not "eligible" and
/// offering it again invites a second opt-in that would clone over a live
/// tree. Then a missing `cwd`, then a `cwd` that is not a checkout, and
/// only then the Flockfile question, which is the only one that needs the
/// checkout to be a checkout.
#[must_use]
pub fn classify(shep_home: &Path, app: &AppConfig) -> Standing {
    let tree = Tree::for_sheep(shep_home, &app.name);
    if let Ok(state) = State::read(&tree.state_file()) {
        let (branch, sha) = (state.branch.clone(), state.deployed.clone());
        return match state.watch {
            Watch::Auto => Standing::Watched { branch, sha },
            Watch::Manual => Standing::Manual { branch, sha },
        };
    }

    let Some(cwd) = app.cwd.as_deref() else {
        return Standing::NotEligible(
            "shep records no working directory for it, so there is nothing to inspect".to_owned(),
        );
    };

    let checkout = Path::new(cwd);
    if !checkout.join(".git").exists() {
        return Standing::NotEligible(format!("{cwd} is not a git repository"));
    }

    if checkout.join("Flockfile.toml").is_file() {
        Standing::NeedsSetup
    } else {
        Standing::Eligible
    }
}
```

`.git` by `exists()` rather than `is_dir()`, deliberately: a worktree's `.git` is a file, not a directory, and an operator running a sheep out of a worktree is running it out of a checkout.

```rust
/// Three columns: name, standing, reason.
///
/// Padded to the widest of each of the first two rather than to fixed
/// widths, because a flock of `bpm` and one of
/// `reactmap-staging-europe` should both read as columns.
#[must_use]
pub fn render(rows: &[(String, Standing)]) -> String {
    if rows.is_empty() {
        return "no sheep are registered, so there is nothing to survey\n".to_owned();
    }

    let name_width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0) + 2;
    let label_width = rows
        .iter()
        .map(|(_, standing)| standing.label().len())
        .max()
        .unwrap_or(0)
        + 2;

    rows.iter()
        .map(|(name, standing)| {
            format!(
                "{name:name_width$}{:label_width$}{}\n",
                standing.label(),
                standing.reason()
            )
        })
        .collect()
}

impl Standing {
    /// The one or two words in the second column.
    fn label(&self) -> &'static str {
        match self {
            Self::Watched { .. } => "watched",
            Self::Manual { .. } => "manual",
            Self::NeedsSetup => "needs setup",
            Self::Eligible => "eligible",
            Self::NotEligible(_) => "not eligible",
        }
    }

    /// The third column, which is the half that says what to do next.
    fn reason(&self) -> String {
        match self {
            Self::Watched { branch, sha } => {
                format!("{}, deploys on every new commit", at(branch, sha.as_deref()))
            }
            Self::Manual { branch, sha } => {
                format!("{}, deploys only when asked", at(branch, sha.as_deref()))
            }
            Self::NeedsSetup => "a git checkout that ships a Flockfile".to_owned(),
            Self::Eligible => "a git checkout, nothing declares a deploy".to_owned(),
            Self::NotEligible(why) => why.clone(),
        }
    }
}

/// `main@a1b2c3`, or just the branch for a target with no deploy yet.
///
/// `get`, not a slice, for the same reason [`crate::smit::text`] uses it: a
/// hand-edited `deploy.toml` carrying a short sha must degrade to a shorter
/// string rather than panic in the middle of a listing.
fn at(branch: &str, sha: Option<&str>) -> String {
    sha.map_or_else(
        || format!("{branch}, not deployed yet"),
        |sha| format!("{branch}@{}", sha.get(..6).unwrap_or(sha)),
    )
}
```

- [ ] **Step 5: Implement `survey` and wire the verb**

```rust
/// The whole flock, classified and rendered.
///
/// # Errors
/// Whatever [`crate::roll::registered`] returns.
pub async fn survey<D: Daemon>(daemon: &D, shep_home: &Path) -> Result<String, Error> {
    let apps = roll::registered(daemon).await?;
    let rows: Vec<(String, Standing)> = apps
        .values()
        .map(|app| (app.name.clone(), classify(shep_home, app)))
        .collect();
    Ok(render(&rows))
}
```

In `main.rs`, add `["survey"] => survey_once().await` to the argument match, extend `USAGE`, and add the verb to the module doc's list of invocation forms.

- [ ] **Step 6: Run the tests**

Run: `cargo test --locked`
Expected: all pass.

- [ ] **Step 7: Mutation-check every branch of the classification**

| mutation | expected |
|---|---|
| move the `State::read` check below the `.git` check | `an_existing_target_reports_its_watch_mode_not_its_eligibility` red |
| swap `NeedsSetup` and `Eligible` | `a_checkout_shipping_a_flockfile_needs_setup...` red |
| `.git` check from `exists()` to `is_dir()` | nothing moves, because no fixture uses a worktree. **Add one** rather than leaving the branch unpinned: a `git worktree add` fixture whose `.git` is a file, asserted eligible. |
| drop the `cwd.is_none()` arm and default to eligible | `a_sheep_with_no_recorded_cwd_is_not_eligible` red |
| drop the reason column from `render` | `the_rendered_survey_is_three_aligned_columns` red |

- [ ] **Step 8: README**

Add `shep-deploy survey` to the Usage block with one line: it reports where every sheep stands and starts, registers and writes nothing.

- [ ] **Step 9: Commit**

```bash
git add src/survey.rs src/paths.rs src/main.rs README.md
git commit -F- <<'EOF'
feat: survey the flock, read-only

Discovery is read-only and deploying requires opt-in, so this starts
nothing, registers nothing and writes nothing. A sheep whose cwd is not a
git checkout is reported and left entirely alone: turning a directory into a
checkout is the operator's decision, not a dog's.

"Declares a deploy block" is read as "the checkout ships a Flockfile.toml".
There is no [deploy] block anywhere in this design, and the reading matches
what the spec's own motivation argues, that upstream shipping the app
definition is what turns "how do I run this" from a README section people
misread into a file. Flagged in the plan as an interpretation rather than
a fact, since the spec's wording is loose there.

Classification order is load-bearing and the tests pin it. An existing
target is answered first, because a sheep already being deployed is not
eligible and offering it again invites a second opt-in that would clone over
a live tree.

.git is checked with exists() rather than is_dir(), because a git worktree's
.git is a file, and an operator running a sheep out of a worktree is running
it out of a checkout.

paths::targets reads the deploy directory rather than any list held
elsewhere. That is what makes a tree self-describing: it survives the dog
being rehomed, re-adopted under another name, or replaced by a different
deploy dog, which is the same reasoning that kept per-target state out of
[dog.<name>].
EOF
```

---

## Task 7: Opt-in, part one: building the tree and the first release

**Files:**
- Create: `src/optin.rs`
- Modify: `src/git.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `roll::registered`, `git`, `shared`, `flockfile`, `build`, `swap`, `Tree`, `State`.
- Produces:
  ```rust
  /// `Clone` is needed by Task 8's own tests, which assert on what the
  /// cutover did with a value they still hold afterwards.
  #[derive(Debug, Clone)]
  pub struct Prepared {
      pub tree: Tree,
      pub state: State,
      pub sha: String,
      /// The release's app definition, which the cutover registers after
      /// overriding `cwd`.
      pub app: AppConfig,
      /// The app definition as the shepherd has it RIGHT NOW, carried so an
      /// abandoned cutover can put the shepherd's persisted record back.
      /// See Task 8's `undo_start`: an accepted `Start` records against the
      /// sheep's name, and deleting the instance does not undo that.
      pub previous_config: AppConfig,
  }
  pub async fn prepare<D: Daemon>(
      daemon: &D,
      shep_home: &Path,
      sheep: &str,
  ) -> Result<Prepared, Error>;
  ```
  and in `git`:
  ```rust
  pub fn init_bare(git_dir: &Path) -> Result<(), Error>;
  ```

**Everything in this task happens before anything is registered or reloaded.** A failure anywhere in it leaves the sheep running exactly as it was, from the operator's own checkout, with nothing about the flock changed. The tree it leaves behind on a failure is a directory, and the operator's fix is to run the command again once they have fixed whatever it named.

**That ordering is load-bearing rather than tidy, and Task 8 is why.** `origin_cwd`, `origin_script` and `previous_config` are all read from the shepherd's muster roll, and the roll is poisoned the instant Task 8's `Start` is accepted: `FlockRegistry` is name-keyed, so from that moment the roll's entry for this sheep is the DOG's config, not the operator's. Capture here, before any `Start` has ever been sent, and those three values are the operator's own. Capture them later, or re-run `prepare` after an abandoned cutover, and the dog would record the deploy tree as the thing to restore to, so Task 9 would faithfully put the sheep back into the directory it was trying to leave. The refusal at the top of `prepare` is what stops the re-run case, and this is the reason it is not merely a convenience.

**Why `git init --bare` and a fetch, rather than a clone.** `crate::git::fetch` is anonymous by URL with a mirror refspec and `--prune`, and it needs no configured remote, which is why `git_dir` never gets a `git remote add`. An empty bare repository plus that same fetch reaches exactly the same state a clone would, through the one code path the poll loop already uses every 30 seconds, rather than through a second one that would only ever run once. **The cost is real and worth naming: opt-in downloads the repository from the remote rather than hardlinking from the operator's checkout next door.** Cloning locally would be faster, and it was rejected because it entangles the dog's object store with a checkout the design says the dog only ever reads, and because a first-run-only code path is the one nothing exercises.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a sheep that is already a target can be opted in again.
    /// The second run would `git init --bare` over a live tree and rebuild
    /// a release for an app whose sheep is already running from `current`,
    /// which is a way to break a working deployment with a command whose
    /// name sounds safe.
    #[tokio::test]
    async fn opting_in_twice_is_refused_by_name() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        write_target(home.path(), "bpm", Watch::Auto, Some("old"));
        let daemon = RollOf(&[("bpm", checkout.path())]);

        let err = prepare(&daemon, home.path(), "bpm").await.expect_err("refuses");
        let shown = err.to_string();
        assert!(shown.contains("bpm"), "{shown}");
        assert!(shown.contains("already"), "{shown}");
    }

    /// fails if opt-in stops recording how the sheep ran BEFORE it took
    /// over. These two fields are the only record there is, and losing them
    /// means removing the dog leaves the app running from a path under
    /// $SHEP_HOME the operator has no reason to know about, which is the
    /// exact failure the restore exists to prevent.
    #[tokio::test]
    async fn the_pre_adoption_cwd_and_script_are_recorded() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        let daemon = RollOf(&[("bpm", checkout.path())]);

        let prepared = prepare(&daemon, home.path(), "bpm").await.expect("prepares");

        assert_eq!(
            prepared.state.origin_cwd.as_deref(),
            Some(checkout.path())
        );
        assert_eq!(prepared.state.origin_script.as_deref(), Some("./run.sh"));
        assert_eq!(prepared.state.checkout, checkout.path());
    }

    /// fails if the branch stops coming from the operator's own checkout.
    /// That was the best idea in this design: `git checkout stable`
    /// retargets the deploy and nobody learns a new config key. A --branch
    /// flag would give one fact two sources of truth, which is why there
    /// is not one.
    #[tokio::test]
    async fn the_branch_comes_from_the_checkouts_own_head() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        git_in(checkout.path(), &["checkout", "-q", "-b", "stable"]);
        let daemon = RollOf(&[("bpm", checkout.path())]);

        let prepared = prepare(&daemon, home.path(), "bpm").await.expect("prepares");
        assert_eq!(prepared.state.branch, "stable");
    }

    /// fails if `current` does not end up pointing at a real release
    /// holding the shared files. This is the whole deliverable of part one:
    /// a tree a cutover can point a sheep at.
    #[tokio::test]
    async fn current_ends_up_on_a_release_carrying_the_shared_files() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        std::fs::write(checkout.path().join(".gitignore"), "local.json\n").expect("write");
        std::fs::write(checkout.path().join("local.json"), "{}").expect("write");
        let daemon = RollOf(&[("bpm", checkout.path())]);

        let prepared = prepare(&daemon, home.path(), "bpm").await.expect("prepares");

        let current = swap::resolve(&prepared.tree.current())
            .expect("reads")
            .expect("current is set");
        assert_eq!(current, prepared.tree.release(&prepared.sha));
        assert_eq!(
            std::fs::read_to_string(current.join("local.json")).expect("reads through the link"),
            "{}"
        );
    }

    /// fails if a checkout on a detached HEAD is accepted. There is no
    /// branch to track, so there is nothing to poll, and a target created
    /// this way would silently never deploy again.
    #[tokio::test]
    async fn a_detached_checkout_is_refused() {
        let home = tempfile::tempdir().expect("tempdir");
        let checkout = checkout_with_commit();
        let head = head_sha(checkout.path());
        git_in(checkout.path(), &["checkout", "-q", &head]);
        let daemon = RollOf(&[("bpm", checkout.path())]);

        let err = prepare(&daemon, home.path(), "bpm").await.expect_err("refuses");
        assert!(err.to_string().contains("detached"), "{err}");
    }
}
```

`RollOf` is a `Daemon` double whose `save_roll` writes a roll JSON naming each `(sheep, cwd)` with `script = "./run.sh"` into a tempdir and answers with that path, and which `unimplemented!()`s everything else. `checkout_with_commit` makes a tempdir, `git init -q`, writes a `Flockfile.toml` declaring one app named `bpm` with `script = "./run.sh"`, sets `user.email`/`user.name` locally so the commit works on a bare CI runner, and commits.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --locked the_pre_adoption_cwd_and_script_are_recorded`
Expected: FAIL, `prepare` not defined.

- [ ] **Step 3: Add `git::init_bare`**

```rust
/// Creates an empty bare repository at `git_dir`, making its parents.
///
/// Empty and then fetched, rather than cloned. [`fetch`] is anonymous by
/// URL with a mirror refspec and `--prune` and needs no configured remote,
/// so an empty repository plus that same fetch reaches exactly the state a
/// clone would, through the one code path the poll loop already runs every
/// thirty seconds instead of a second one that would only ever run once.
///
/// # Errors
/// [`Error::Io`], naming `git_dir`, if it cannot be created or is not
/// valid UTF-8. [`Error::Git`] if `git init` refuses.
pub fn init_bare(git_dir: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(git_dir).map_err(|source| Error::Io {
        path: git_dir.to_owned(),
        source,
    })?;
    run_git(git_dir, &["init", "-q", "--bare"]).map(|_| ())
}
```

Drop the `#[allow(dead_code)]` from `git::remote_url` and `git::current_branch`, whose comments already say opt-in is their caller.

- [ ] **Step 4: Implement `prepare`**

The sequence, and each step's reason:

```rust
pub async fn prepare<D: Daemon>(
    daemon: &D,
    shep_home: &Path,
    sheep: &str,
) -> Result<Prepared, Error> {
    let tree = Tree::for_sheep(shep_home, sheep);
    if tree.state_file().is_file() {
        return Err(Error::Config(format!(
            "{sheep} is already a deploy target: its tree is at {}. Deploy it with \
             `shep deploy {sheep}`, or change how it is watched with \
             `shep deploy {sheep} --watch auto|manual`.",
            tree.releases().display()
        )));
    }

    // Cloned rather than borrowed, and named `previous_config` rather than
    // `app`, because the release's own definition shadows the name below and
    // these two must never be confused: this one is the operator's, and it
    // is the only copy of it that exists once Task 8 sends its Start.
    let registered = roll::registered(daemon).await?;
    let previous_config = registered.get(sheep).cloned().ok_or_else(|| {
        Error::Config(format!(
            "the shepherd has no sheep named {sheep:?} registered, so there is nothing to take \
             over: `shep-deploy survey` lists every sheep and where it stands"
        ))
    })?;
    let checkout = PathBuf::from(previous_config.cwd.as_deref().ok_or_else(|| {
        Error::Config(format!(
            "shep records no working directory for {sheep}, so there is no checkout to deploy \
             from"
        ))
    })?);

    // Both of these refuse rather than guess: a checkout with no `origin`
    // gets git's own complaint, and a detached HEAD is refused by name,
    // because there is no branch to track and a target created that way
    // would silently never deploy again.
    let remote = git::remote_url(&checkout)?;
    let branch = git::current_branch(&checkout)?;

    let mut state = State {
        remote,
        branch,
        deployed: None,
        verify: Verify::default(),
        watch: Watch::default(),
        // The two fields the whole restore depends on, captured once, here,
        // and never touched again.
        origin_cwd: Some(checkout.clone()),
        origin_script: Some(previous_config.script.clone()),
        checkout,
    };

    std::fs::create_dir_all(tree.releases()).map_err(|source| Error::Io {
        path: tree.releases(),
        source,
    })?;
    git::init_bare(&tree.git())?;
    git::fetch(&tree.git(), &state.remote)?;
    let sha = git::remote_head(&tree.git(), &state.branch)?;

    let release = tree.release(&sha);
    git::worktree_add(&tree.git(), &release, &sha)?;
    shared::link_cache(&release, &tree.cache_target())?;
    shared::link_into(&release, &state.checkout, &shared::to_link(&state.checkout)?)?;

    let app = flockfile::app_config(&release, sheep)?;
    let spec = flockfile::build_spec(&release)?;
    build::run(sheep, &release, &spec, app.user.as_deref()).await?;

    swap::point_at(&tree.current(), &release)?;
    state.write(&tree.state_file())?;

    Ok(Prepared {
        tree,
        state,
        sha,
        app,
        previous_config,
    })
}
```

Note two things about the ordering. `state.write` comes **after** the swap, so a run that dies partway leaves no `deploy.toml`, and the tree is not yet a target: the refusal at the top then lets the operator simply run the command again. And `state.deployed` stays `None` until the cutover verifies, which is the same rule `deploy` follows, so the record never claims a release that has not served.

- [ ] **Step 5: Run the tests**

Run: `cargo test --locked`
Expected: all pass.

- [ ] **Step 6: Mutation-check every guard**

| mutation | expected |
|---|---|
| drop the `tree.state_file().is_file()` refusal | `opting_in_twice_is_refused_by_name` red |
| set `origin_cwd`/`origin_script` to `None` | `the_pre_adoption_cwd_and_script_are_recorded` red |
| take `branch` from a literal `"main"` | `the_branch_comes_from_the_checkouts_own_head` red |
| drop the `link_into` call | `current_ends_up_on_a_release_carrying_the_shared_files` red |
| use `git rev-parse --abbrev-ref HEAD` in `current_branch` | `a_detached_checkout_is_refused` red. This re-pins plan one's guard from the caller that finally has one. |
| move `state.write` above `swap::point_at` | nothing moves. **Do not add a test for this**: it would need a process killed between two statements, which a single-threaded test cannot arrange, and a test named for a property it does not check is worse than no test. The ordering rests on the doc comment and on review, and say so there, exactly as `swap.rs` says it about atomicity. |

- [ ] **Step 7: Commit**

```bash
git add src/optin.rs src/git.rs src/main.rs
git commit -F- <<'EOF'
feat: opt-in part one, build the tree and the first release

Everything here happens before anything is registered or reloaded, so a
failure anywhere in it leaves the sheep running exactly as it was, from the
operator's own checkout, with nothing about the flock changed. What it
leaves behind on a failure is a directory.

git init --bare plus the ordinary fetch, rather than a clone. fetch is
already anonymous by URL with a mirror refspec and --prune and needs no
configured remote, so an empty repository plus that same fetch reaches the
state a clone would, through the one code path the poll loop runs every
thirty seconds instead of a second one that would only ever run once. The
cost is real: opt-in downloads from the remote rather than hardlinking from
the checkout next door. Cloning locally was rejected because it entangles
the dog's object store with a checkout the design says the dog only reads,
and because a first-run-only path is the one nothing exercises.

The branch comes from the checkout's own HEAD, which was the best idea in
this design: `git checkout stable` retargets the deploy and nobody learns a
new config key. A --branch flag would give one fact two sources of truth,
which is why there is not one. A detached HEAD is refused by name, since
there is no branch to track and a target made that way would silently never
deploy again.

origin_cwd and origin_script are captured once, here, and never touched
again. They are the only record of how the sheep ran before this dog took
over, and losing them means removing the dog leaves the app running from a
path under $SHEP_HOME the operator has no reason to know about.

deploy.toml is written after the swap, not before, so a run that dies
partway leaves no target behind and the operator can simply run the command
again. state.deployed stays None until the cutover verifies, the same rule
deploy follows, so the record never names a release that has not served.
EOF
```

---

## Task 8: Opt-in, part two: the cutover

**Files:**
- Modify: `src/optin.rs`, `src/daemon.rs`, `src/error.rs`, `src/main.rs`, `README.md`
- Modify: `src/verify.rs`, `src/deploy.rs` (widening six private items and adding `Generation::of_infos`, step 5)

**Interfaces:**
- Produces:
  ```rust
  pub async fn cut_over<D: Daemon>(daemon: &D, prepared: Prepared) -> Result<String, Error>;
  enum CutOver { Done, NotStarted(Error), NotVerified(String), Failed(Error) }
  ```
  and on `Daemon`:
  ```rust
  async fn delete(&self, id: u32) -> Result<(), Error>;
  ```

**This is the task the whole plan is arranged around, and it is the one path with no rollback.** Five measured facts decide its shape and none is guessable.

**One: a defaulted `cwd` is not resolved for us.** The CLI canonicalises `None` to the Flockfile's directory before sending; over the socket, `None` stays `None` and the child inherits the shepherd's own working directory, commonly `/`. **So the cutover sets `cwd` explicitly, to `tree.current()`, and a test pins it.** Getting this wrong produces either a sheep pinned to one release forever or a sheep started in `/`, and the first of those looks correct for exactly one release.

**Two: `Request::Start` on an already-registered name ADDS instances rather than re-registering.** `do_start` calls `instance_slots(&existing, instances)` and `instance_slots(&[0], 1)` is `[1]`. That is what makes the spec's cutover implementable at all, and it is why the old instances are removed **by id**: a name selector would delete the replacement along with what it replaced.

**Three: the first cutover may have downtime.** Both instances are alive at once, so unless the app binds with `SO_REUSEPORT` the newcomer cannot take the port. That is a failure to handle, not a caveat to record, and it is the likeliest way a first cutover goes wrong.

**Four: `Online` after a `Start` does not mean the probe passed.** See the fact list above. shep marks a fresh spawn `Online` when `listen_timeout` elapses whatever the probe said, and only a reload replacement is aborted on a readiness timeout.

**Five: the shepherd's persisted roll is poisoned the moment the `Start` is accepted**, and deleting the newcomer does not clean it up.

### What the dog can actually establish about a freshly started release

This is a design question rather than a detail, and the honest answer is weaker than the deploy path's. Working it through, because an earlier draft of this task got it wrong in a way no amount of plan-reading would catch.

**What is available.** `ProcessInfo` gives status, pid, restarts, uptime and `last_exit`. Nothing gives the probe's verdict, and on the `Start` path shep has deliberately discarded it.

**Four stronger checks were considered and each fails for its own reason:**

- **Infer the verdict from timing.** If `Online` arrives well before `listen_timeout`, the probe must have passed. Rejected: it compares wall-clock against a `listen_timeout` read from the release's Flockfile, which may not be what shep registered, and that is the exact class of inference the engine plan spent five fix rounds removing. It is also racy at the boundary, where a probe passing at 0.99 of the timeout is indistinguishable from the timeout.
- **Run the probe ourselves.** The dog can read `readiness_probe` from the release's Flockfile. Rejected, and the reason is worth keeping: **the failure this task is arranged around is invisible to an external probe.** In a port collision the original instance is still bound to and serving that port, so a probe from outside passes while the newcomer is dead. A check that reports healthiest exactly when the thing it guards against happens is worse than no check.
- **Cut over, then issue a `Reload` and verify THAT with the full probed machinery.** Tempting, because a reload IS the path where shep aborts on a readiness timeout. Rejected: by then the original has been deleted, so this converts weak verification into strong verification of a machine that can no longer be repaired. Reload-before-deleting does not work either, since a reload replaces every instance including the original.
- **Register at rest and reload into it.** There is no wire request for `RegisterAtRest`; it is a supervisor command muster uses, not something a dog can send.

**So the answer is: the dog can establish that a new process was spawned, and that after a dwell it is still the same process, still not errored, and has not restarted. Nothing more.** That is exactly `Verify::Alive`.

### The cutover deletes the original, once the alive check passes

**Leaving the original running was tried, on Rin's instruction, and reversed by her on the evidence.** Her words on the reversal: "Just auto delete it ourselves once we have any sign of life from the new deployment." Recorded with its reasoning, because the round trip is what produced two of the things this task now gets right.

The version that survived a cutover kept the original instance alive and printed the exact `shep delete <id>` for the operator to run once they had confirmed the new one served. The argument for it was sound and is still sound as far as it goes: the dog can establish "alive" and cannot establish "serving", the operator can hit the endpoint, so put the judgement where the better information is.

**What killed it was a consequence nobody saw until the plan was written out.** During the window before the operator runs that command, a deploy reloads *every* instance of the name, and each is replaced from its own spec. So the leftover was respawned from the **pre-adoption config**, serving the operator's checkout code, while being actively kept alive by the supervisor. Stale and forgotten would have been tolerable. Stale and being restarted on every deploy is not, and it is the kind of thing that looks like a deploy that silently half-applied.

**Two things the detour bought, which is why it is written down rather than deleted:** it is what surfaced the roll poisoning below, and it is why this plan now states plainly that `Online` after a `Start` carries no readiness information at all.

**So: the original instances are deleted by id, once the alive check passes.** By id and never by name: shep-core's selector parses a bare all-ASCII-digit string as `SelectorSpec::Id` (`selector.rs:95-99`, `input.bytes().all(|b| b.is_ascii_digit())`, with a test pinning `parse("3") == Id(3)`), and a name selector would take the new instance down along with the old.

**Three consequences, each a requirement:**

1. **The ids come from the `describe` taken BEFORE the `Start`.** Afterwards two rows share the name and nothing on the wire says which was already there, so a post-`Start` read would risk deleting the release just deployed.
2. **The alive check has to be worth deleting on**, which is what the dwell is for. One sample distinguishes "it started" from nothing; the dwell distinguishes "it started" from "it stayed up". See [`attempt`], which polls for a newcomer and then requires the same pids, still alive and with no restarts, [`DWELL`] later.
3. **`undo_start` is still needed on the FAILURE path**, and never had anything to do with the leftover. On failure the roll still names the new config, so without the repair a reboot resurrects a rejected release.

**Three further consequences of the alive check itself, unchanged by the above:**

1. **The cutover uses the alive check regardless of `state.verify`.** Not carrying `refuse_ungated_verification` across is deliberate: refusing a target for lacking a readiness gate the cutover cannot consult would be incoherent, and would block setup for precisely the best-configured targets. Silently labelling this `probed` would be a lie.
2. **It is documented as weaker**, in the doc comment, the printed text and the README. The weakness is bounded: once the cutover lands, the target is an ordinary deploy target and every deploy after it gets full turnover verification and auto-rollback.
3. **A release that starts, stays running, and serves nothing passes.** That is what alive means and it is stated rather than hidden. Say it once, as a bounded limitation of the first cutover, not as a warning wall: every deploy after it gets full turnover verification against the readiness probe, with automatic rollback.

`cutover_budget` also stops being dead code under this design. Under the old predicate it never mattered because the check always fired first; here it bounds only how long a newcomer has to APPEAR, with the dwell doing the real work, so a probeless app that stays `Starting` well past eight seconds is fine.

### The two boundaries

**The rollback boundary is the `Start`**, the same shape `land` has around the swap. `attempt` owns every fallible step after the shepherd accepts it and cannot return an error, so this task's `match` is exhaustive over what has to be cleaned up rather than over what went wrong. The question the variants turn on is whether the `Start` was accepted, because before it nothing was spawned and nothing was recorded.

**The second boundary is the roll, and it does not coincide with the first.** An accepted `Start` records its config against the name immediately, and deleting the newcomer does not undo that, because the surviving original keeps the name alive so `roll`'s prune never drops it. Reproduced end to end: after the delete the roll named the new release's `cwd` while the live pid executed in the old one, and after `shep kill`, a restart and `shep muster` the sheep came back **from the abandoned release**.

Three things follow. The abandoned cutover must attempt to put the record back. The error text must stop claiming the machine is unchanged, because that is true of the process and false of the persisted flock. And **this is the concrete reason Task 7 captures `origin_cwd` and `origin_script` before any `Start` is ever sent**: after a poisoned `Start`, the dog's own `roll::registered` reads the dog's values, so a `prepare` re-run would record the deploy tree as the thing to restore to, and Task 9 would faithfully put the sheep back into the directory it was trying to leave.

- [ ] **Step 1: Write the failing tests**

```rust
    /// fails if the new registration's cwd is anything but the `current`
    /// symlink, set EXPLICITLY. A Flockfile app's DEFAULTED cwd is resolved
    /// at registration, so registering from inside a release pins the sheep
    /// to that release and every later swap moves a symlink the app no
    /// longer reaches. Measured on a real shepherd: the reload after a swap
    /// re-ran the OLD release's script. This is the single most
    /// load-bearing line in the crate and it fails silently, one release
    /// later, when it is wrong.
    #[tokio::test]
    async fn the_new_registration_names_current_explicitly() {
        let (daemon, prepared) = cutover_fixture().await;
        cut_over(&daemon, prepared.clone()).await.expect("cuts over");

        let started = daemon.started();
        assert_eq!(started.len(), 1, "exactly one Start");
        assert_eq!(
            started[0].cwd.as_deref(),
            prepared.tree.current().to_str(),
            "cwd must be the current symlink itself, not a release and not None"
        );
    }

    /// fails if the OLD instances are deleted by name rather than by id, or
    /// if the newcomer is deleted instead. A name selector would take the
    /// new instance down with them, because `Start` added it BESIDE the old
    /// under the same name, and an id read after the `Start` could name the
    /// release that was just deployed.
    #[tokio::test(start_paused = true)]
    async fn only_the_old_instances_are_deleted_and_by_id() {
        let (daemon, prepared) = cutover_fixture().await;
        cut_over(&daemon, prepared).await.expect("cuts over");
        assert_eq!(daemon.deleted(), vec![7], "the pre-existing instance's id");
        assert!(!daemon.deleted().contains(&99), "99 is the newcomer");
    }

    /// fails if a scaled sheep loses only one of the instances it was
    /// running. `Start` adds one newcomer per configured instance beside
    /// every existing one, so a cutover that deleted a single id would
    /// leave the rest serving the pre-adoption checkout indefinitely.
    #[tokio::test(start_paused = true)]
    async fn every_replaced_instance_is_deleted() {
        let (daemon, prepared) = cutover_fixture_with_instances(&[7, 8]).await;
        cut_over(&daemon, prepared).await.expect("cuts over");
        assert_eq!(daemon.deleted(), vec![7, 8]);
    }

    /// fails if a new instance that never comes up is left registered. The
    /// likeliest cause is the one the design names: both instances are
    /// alive at once, so without SO_REUSEPORT the new one cannot bind the
    /// port and dies. Leaving it behind gives the operator a permanently
    /// errored second instance of their app and an old one still serving,
    /// with nothing saying which is which.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_never_comes_up_is_deleted_and_the_old_one_kept() {
        let (daemon, prepared) = cutover_fixture_dies_during_dwell().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        assert!(daemon.deleted().contains(&99), "the newcomer is removed");
        assert!(!daemon.deleted().contains(&7), "the old instance is kept");
        let shown = err.to_string();
        assert!(shown.contains("SO_REUSEPORT"), "{shown}");
    }

    /// fails if a release that shep called `Online` without the probe ever
    /// passing is accepted. This is the whole of finding F1 and it is the
    /// engine plan's round-3 blocker on a different path: shep marks a
    /// FRESH SPAWN Online when `listen_timeout` elapses whatever the probe
    /// said, and aborts only a RELOAD replacement on a readiness timeout.
    /// So `is_new(info) && Online` establishes nothing about a Start, and
    /// measured against a real shepherd it returned Done in 15.6ms on a
    /// dead-on-arrival release, after which the healthy original is deleted
    /// by id and `state.deployed` written.
    ///
    /// The fixture is the exact shape shep produces: the newcomer reports
    /// Online, on time, and is gone by the dwell.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_went_online_without_serving_is_still_rejected() {
        let (daemon, prepared) = cutover_fixture_online_then_gone().await;
        cut_over(&daemon, prepared).await.expect_err("the dwell catches it");
    }

    /// fails if a newcomer that crash-loops through the dwell is accepted.
    /// An app whose release cannot run is restarted by shep, so it is
    /// present at every poll and present at the dwell, under a DIFFERENT
    /// pid each time. Pid identity across the dwell is what catches it, and
    /// `restarts` moving is the second signal.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_crash_loops_through_the_dwell_is_rejected() {
        let (daemon, prepared) = cutover_fixture_crash_looping().await;
        cut_over(&daemon, prepared).await.expect_err("the pids moved");
    }

    /// fails if an abandoned cutover leaves shep's persisted roll naming
    /// the release that was just rejected. `FlockRegistry` is name-keyed
    /// and records on every accepted Start, and deleting the newcomer does
    /// NOT undo it, because the surviving original keeps the name alive so
    /// the roll's prune never drops it. Verified end to end: after the
    /// delete the roll named the new release while the live pid executed in
    /// the old one, and `shep kill` plus a restart plus `shep muster`
    /// brought the sheep back from the abandoned release.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_cutover_puts_the_original_config_back_in_the_roll() {
        let (daemon, prepared) = cutover_fixture_dies_during_dwell().await;
        let original = prepared.previous_config.clone();

        cut_over(&daemon, prepared).await.expect_err("gives up");

        let last = daemon.started().last().cloned().expect("a repair Start");
        assert_eq!(last.cwd, original.cwd, "the roll is re-recorded at the old cwd");
        assert_eq!(
            daemon.deleted().len(),
            2,
            "the newcomer, and the instance the repair Start spawned"
        );
    }

    /// fails if a repair that could not be made is glossed over. `Error::
    /// CutOver` used to say the original was serving "unchanged", which is
    /// true of the process and false of the persisted flock: a reboot would
    /// silently resurrect the rejected release. When the repair fails the
    /// operator has to be told that specifically, because it is the half
    /// they cannot see in `shep flock`.
    #[tokio::test(start_paused = true)]
    async fn a_failed_roll_repair_is_named_not_glossed() {
        let (daemon, prepared) = cutover_fixture_dies_and_refuses_repair().await;
        let err = cut_over(&daemon, prepared).await.expect_err("gives up");
        let shown = err.to_string();
        assert!(shown.contains("muster"), "{shown}");
        assert!(shown.contains("reboot") || shown.contains("restart"), "{shown}");
    }

    /// fails if a Start the shepherd refused is treated as one it accepted.
    /// Nothing was spawned, so there is nothing to delete, and issuing a
    /// Delete against an id that was never created would either do nothing
    /// or, worse, match something else.
    #[tokio::test]
    async fn a_refused_start_deletes_nothing() {
        let (daemon, prepared) = cutover_fixture_refusing_start().await;
        cut_over(&daemon, prepared).await.expect_err("refused");
        assert!(daemon.deleted().is_empty());
    }

    /// fails if the record is advanced before the newcomer verified.
    /// `deploy.toml` naming a release nothing has served is the same defect
    /// the engine plan spent five rounds removing from the deploy path, and
    /// it must not come back through this one.
    #[tokio::test]
    async fn the_record_advances_only_after_the_newcomer_is_online() {
        let (daemon, prepared) = cutover_fixture_never_ready().await;
        let path = prepared.tree.state_file();
        cut_over(&daemon, prepared).await.expect_err("gives up");
        assert_eq!(State::read(&path).expect("reads").deployed, None);
    }
```

**The fixtures.** All of them build on Task 7's: run `prepare` against a `RollOf` double, then wrap it in one `CutOverDouble` recording `start` and `delete` calls. Its `describe` answers `[id 7, online, pid 100]` before any `start`, and afterwards that row plus `[id 99, <staged>, pid 200]`. **Stage the newcomer over several polls rather than having it land instantly**, and **keep the original `Online` throughout**: the instant-turnover fake is the exact fiction that let the engine plan's worst blocker survive unit testing, and a real shepherd does keep the old instance serving.

One constructor with parameters, not eight functions. These differ only in what the newcomer does over time, so give `CutOverDouble` a script and name the eight shapes as thin wrappers:

| fixture | the newcomer's script | exists to reach |
|---|---|---|
| `cutover_fixture` | `Starting` for two polls, then `Online`, and stays | the success path |
| `cutover_fixture_with_instances(&[7, 8])` | as above, two originals and two newcomers | every replaced instance being deleted |
| `cutover_fixture_online_then_gone` | `Online` immediately, absent by the dwell | **F1**: shep reports a fresh spawn `Online` on `listen_timeout` whatever the probe said |
| `cutover_fixture_crash_looping` | present at every poll, a NEW pid each time | the dwell's pid identity check |
| `cutover_fixture_dies_during_dwell` | `Online`, then `Errored` at the dwell | the port-collision shape, and the roll repair |
| `cutover_fixture_never_ready` | `Starting` forever, never `Online` | the phase-one budget expiring |
| `cutover_fixture_refusing_start` | `start` returns an `RpcError` | `NotStarted`, where nothing was spawned or recorded |
| `cutover_fixture_dies_and_refuses_repair` | as `dies_during_dwell`, and every later `start` is refused | `repaired: false`, the state an operator cannot see |

`cutover_fixture_dies_and_refuses_repair` refuses the **second** `start`, not the first: the first is the cutover's own and has to be accepted, or the test never reaches the repair it exists to check.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --locked the_new_registration_names_current_explicitly`
Expected: FAIL, `cut_over` not defined.

- [ ] **Step 3: Add `delete` to `Daemon`**

```rust
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
```

```rust
    async fn delete(&self, id: u32) -> Result<(), Error> {
        let asked = Request::Delete {
            selector: SelectorSpec::Id(id),
        };
        match self.0.request(asked).await? {
            Response::Deleted(_) => Ok(()),
            other => Err(unexpected("Delete", &other)),
        }
    }
```

Add `Response::Deleted(ids) => format!("a Deleted of {}", ids.len())` to `named`, and the method to every test double.

- [ ] **Step 4: Add `Error::CutOver`**

```rust
    /// A sheep's first cutover did not come up, and the sheep it was
    /// replacing is still serving.
    ///
    /// Its own variant rather than a reuse of [`Self::Unverified`] because
    /// the situation is different in the way that matters to an operator:
    /// nothing was swapped and nothing was rolled back. What has to be said
    /// instead is the likeliest cause, which is specific to this one moment
    /// in a target's life, and whether the shepherd's persisted record
    /// could be put back.
    ///
    /// `repaired` is not a detail. An accepted `Start` records its config
    /// against the sheep's NAME, and deleting the instance it spawned does
    /// not undo that while the original keeps the name alive, so an
    /// unrepaired roll means a reboot silently brings the sheep back on the
    /// release that was just rejected. That is invisible in `shep flock`,
    /// which is why it is named here rather than left to be discovered.
    CutOver {
        /// The sheep whose cutover was abandoned.
        sheep: String,
        /// Why the newcomer was rejected.
        why: String,
        /// Whether the shepherd's persisted roll was put back.
        repaired: bool,
    },
```

with the `Display` arm:

```rust
            Self::CutOver { sheep, why, repaired } => {
                write!(
                    f,
                    "{sheep}'s first cutover did not come up ({why}), so it was removed and the \
                     original is still running. The first cutover is the one deploy that runs two \
                     instances at once, so an app that does not bind with SO_REUSEPORT cannot take \
                     its own port while the original still holds it. Every deploy after the first \
                     replaces the instance rather than joining it and does not meet this. The \
                     deploy tree is left in place, so `shep-deploy setup {sheep}` retries without \
                     rebuilding."
                )?;
                if !repaired {
                    // The half an operator cannot see. `shep flock` shows a
                    // healthy sheep either way; only the persisted roll is
                    // wrong, and only a restart reveals it.
                    write!(
                        f,
                        " One thing is NOT back as it was: the shepherd recorded the new release \
                         against {sheep} when it accepted the start, and that record could not be \
                         put back. It is correct in the running process and wrong on disk, so a \
                         daemon restart followed by `shep muster` would bring {sheep} back on the \
                         release that was just rejected. Re-register it from its own Flockfile to \
                         correct the record before restarting the shepherd."
                    )?;
                }
                Ok(())
            }
```

and `None` from `source()`, alongside the other variants that carry no inner error.

- [ ] **Step 5: Implement the cutover**

```rust
/// Registers the sheep against `current`, waits for the newcomer to start
/// and stay up, and deletes the instances it replaced.
///
/// # Errors
/// [`Error::CutOver`] if the newcomer never came up, in which case it has
/// been removed and the original is still serving, untouched. That error
/// also carries whether the shepherd's persisted record could be put back:
/// see [`undo_start`] for why deleting the newcomer is not enough on its
/// own, and why a failure there is something an operator cannot see in
/// `shep flock` and has to be told by name.
///
/// A refused `Start` is different in kind and is returned unchanged.
/// Nothing was spawned and, just as importantly, nothing was recorded, so
/// the machine really is exactly as it was.
pub async fn cut_over<D: Daemon>(daemon: &D, prepared: Prepared) -> Result<String, Error> {
    let Prepared { tree, mut state, sha, app, previous_config } = prepared;
    let sheep = tree.sheep().to_owned();

    // Captured BEFORE the Start, and this is the only moment it can be:
    // afterwards two rows share this name and nothing on the wire says
    // which of them was already here. Reading it later would risk deleting
    // the release that was just deployed.
    let before = daemon.describe(&sheep).await?;
    let previous: Vec<u32> = before.iter().map(|info| info.id).collect();
    let generation = Generation::of(daemon, &sheep).await?;

    // THE line this task exists for. Over the socket a `cwd` of None stays
    // None and the child inherits the SHEPHERD's working directory, and a
    // Flockfile registered by the CLI from inside a release is canonicalised
    // to that release and pinned there. An explicit one is stored verbatim,
    // symlink and all, which is the only spelling that follows a swap.
    let mut registering = app;
    registering.cwd = Some(tree.current().display().to_string());

    match attempt(daemon, &sheep, registering, &generation).await {
        CutOver::Done => {
            // By id, because Start added the newcomer BESIDE these under
            // the same name and a name selector would take it down too.
            for id in previous {
                daemon.delete(id).await?;
            }
            state.deployed = Some(sha.clone());
            state.write(&tree.state_file())?;
            Ok(sha)
        }
        // Nothing was spawned and nothing was recorded, so there is nothing
        // to clean up and nothing to say beyond what the shepherd said.
        CutOver::NotStarted(source) => Err(source),
        CutOver::NotVerified(why) => {
            let repaired = undo_start(daemon, &sheep, &previous, previous_config).await;
            Err(Error::CutOver { sheep, why, repaired })
        }
        CutOver::Failed(source) => {
            undo_start(daemon, &sheep, &previous, previous_config).await;
            Err(source)
        }
    }
}
```

```rust
/// How long a newcomer gets to APPEAR before the cutover gives up on it.
///
/// Only to appear. The dwell below is what actually decides health, so
/// this does not have to cover a slow readiness path: a probeless app that
/// sits in `Starting` well past eight seconds is fine here, because
/// `is_alive` accepts `Starting` and the question at this stage is
/// only whether a process exists at all.
///
/// A whole `listen_timeout` plus the slack shep allows itself, rather than
/// `crate::deploy::budget`'s per-instance product: a cutover spawns ONE
/// instance and drains nothing, where a reload replaces every instance one
/// at a time and has to wait out each drain.
fn cutover_budget(app: &AppConfig) -> Duration {
    app.listen_timeout.as_duration() + RELOAD_DEADLINE_SLACK
}

/// Registers `app`, waits for a newcomer, and watches it for a dwell.
///
/// This function cannot return an error, and that is the point rather than
/// tidiness: every fallible step after the shepherd accepts the `Start`
/// happens here, so [`cut_over`]'s match is exhaustive over what has to be
/// cleaned up instead of over what went wrong, and a step added here later
/// has nowhere to escape to. Same structure as [`crate::deploy::land`], for
/// the same reason.
///
/// # This check is `Verify::Alive`, and it is weaker than the deploy path's
///
/// It establishes that a new process was spawned and that after [`DWELL`]
/// it is the same process, not errored, and has not restarted. It does NOT
/// establish that the release serves anything, and it cannot.
///
/// The reason is shep's, not this crate's. `handle_ready_result` marks a
/// FRESH SPAWN `Online` when `listen_timeout` elapses whatever the readiness
/// probe said (Rin, 2026-08-08: erroring instead would turn a slow start
/// into a restart loop), and defers to `reload_ready_result`, which DOES
/// abort on a readiness timeout, only for a reload replacement. So `Online`
/// on this path is "the probe passed OR the timeout elapsed" with nothing
/// on the wire distinguishing the two, and [`crate::verify`] is sound
/// precisely because it verifies a reload. A check of `is_new && Online`
/// here returns success in milliseconds for a release that is already dead.
///
/// `state.verify` is deliberately not consulted. `Probed` is unavailable on
/// this path, so honouring it would be a lie and refusing a target that
/// lacks a gate it could not use would be incoherent. The weakness is
/// bounded rather than permanent: once this lands the target is an ordinary
/// deploy target and every deploy after it gets full turnover verification
/// and auto-rollback.
///
/// What it does catch is the case this task is arranged around. A newcomer
/// that cannot bind its port because the original still holds it exits, and
/// shep either errors it or respawns it under a new pid, and the dwell sees
/// both. What passes anyway is a release that starts, stays up, and serves
/// nothing, which is what `alive` has always meant.
async fn attempt<D: Daemon>(
    daemon: &D,
    sheep: &str,
    app: AppConfig,
    before: &Generation,
) -> CutOver {
    let patience = cutover_budget(&app);

    if let Err(source) = daemon.start(vec![app]).await {
        return CutOver::NotStarted(source);
    }
    let started_at = Instant::now();
    let deadline = started_at + patience;

    // Phase one: a newcomer exists and has not already failed.
    let arrived = loop {
        let flock = match daemon.describe(sheep).await {
            Ok(flock) => flock,
            Err(source) => return CutOver::Failed(source),
        };

        let newcomers: Vec<&ProcessInfo> =
            flock.iter().filter(|info| before.is_new(info)).collect();

        if !newcomers.is_empty() && newcomers.iter().all(|info| is_alive(info)) {
            break Generation::of_infos(&newcomers);
        }
        if newcomers.iter().any(|info| !is_alive(info)) {
            return CutOver::NotVerified(
                "the new instance failed before it finished starting".to_owned(),
            );
        }
        if Instant::now() >= deadline {
            return CutOver::NotVerified(format!(
                "no new instance appeared within {}s",
                started_at.elapsed().as_secs()
            ));
        }
        sleep(POLL).await;
    };

    // Phase two: the dwell, which is what actually decides this.
    sleep(DWELL).await;
    let flock = match daemon.describe(sheep).await {
        Ok(flock) => flock,
        Err(source) => return CutOver::Failed(source),
    };

    let survivors: Vec<&ProcessInfo> = flock
        .iter()
        .filter(|info| arrived.holds(info) && is_alive(info))
        .collect();

    if survivors.len() != arrived.instances() as usize {
        // A different pid means shep respawned it, which means it died.
        return CutOver::NotVerified(format!(
            "the new instance did not stay up for {}s after starting",
            DWELL.as_secs()
        ));
    }
    // Belt and braces beside the pid check: a crash and respawn back onto a
    // pid this generation already held is vanishingly unlikely, and this
    // costs one comparison to rule out.
    if survivors.iter().any(|info| info.restarts > 0) {
        return CutOver::NotVerified("the new instance restarted while it was being watched".to_owned());
    }

    CutOver::Done
}

/// Removes every instance this cutover added, and puts the shepherd's
/// persisted record back the way it was.
///
/// Two separate repairs, because the `Start` did two separate things.
///
/// Deleting the newcomer undoes the process. It does NOT undo the record:
/// `FlockRegistry` is keyed by name and records on every accepted `Start`,
/// and the surviving original keeps that name alive so the roll's own prune
/// never drops the poisoned entry. Left alone, `shep muster` after a reboot
/// brings the sheep back from the release that was just rejected, and this
/// dog's own [`crate::roll::registered`] reads the wrong `cwd` from then on.
///
/// So the record is put back the only way the wire allows: a second `Start`
/// carrying the ORIGINAL config, which re-records it, followed by deleting
/// the instance that `Start` necessarily spawned. There is no request that
/// registers without spawning; `RegisterAtRest` is a supervisor command
/// muster uses and is not on the wire.
///
/// Answers whether the record was restored, because the error text differs:
/// a repair that failed leaves something an operator cannot see in
/// `shep flock` and has to be told about by name.
async fn undo_start<D: Daemon>(
    daemon: &D,
    sheep: &str,
    previous: &[u32],
    original: AppConfig,
) -> bool {
    let Ok(flock) = daemon.describe(sheep).await else {
        return false;
    };
    for info in flock.iter().filter(|info| !previous.contains(&info.id)) {
        // Failures dropped: this path is already failing, and an operator
        // needs the reason they got here rather than a second error about
        // the cleanup. What a failure leaves is a newcomer beside the
        // original, which `shep flock` shows plainly.
        let _ = daemon.delete(info.id).await;
    }

    if daemon.start(vec![original]).await.is_err() {
        return false;
    }
    let Ok(flock) = daemon.describe(sheep).await else {
        return false;
    };
    for info in flock.iter().filter(|info| !previous.contains(&info.id)) {
        let _ = daemon.delete(info.id).await;
    }
    true
}
```

**This code borrows six items that are private today and adds one. Widen each under its EXISTING name rather than duplicating it**, and put every widened item behind `pub(crate)`, never `pub`: this is a binary, and a second copy of any of them is the drift the engine plan's fifth fix round spent a whole round removing.

| item | today | why it moves |
|---|---|---|
| `Generation::is_new` | private to `crate::verify` | the pid comparison is the same question here. Extend its doc to name `crate::optin` as the second caller and say why that caller wants the pid check WITHOUT the full-turnover check around it, so nobody later "fixes" one call site by tightening the other. |
| `Generation::holds` | private | the dwell asks "is this still the same one", which is exactly what it answers |
| `verify::is_alive` | private | the cutover's accept predicate IS `Verify::Alive`'s, and having two would let them drift apart silently |
| `verify::DWELL` | private | one dwell for the crate. Two would be two answers to "how long before we believe a process". |
| `verify::POLL` | private | one polling cadence, for the same reason |
| `deploy::RELOAD_DEADLINE_SLACK` | private to `crate::deploy` | shep's own constant, copied once with a file and line in its comment. Copying it a second time into `optin` is precisely the drift that was removed. |

One item is genuinely new, and belongs beside `Generation::of` in `crate::verify`:

```rust
    /// The generation a listing already in hand describes.
    ///
    /// [`Self::of`] takes a [`Daemon`] and issues a request; this takes the
    /// answer to one. The cutover's first phase already holds the listing it
    /// wants to freeze, and asking again would both cost a round trip and
    /// risk freezing a DIFFERENT set of pids than the one it just decided
    /// on, which is the sort of gap a crash-looping release fits through.
    pub(crate) fn of_infos(infos: &[&ProcessInfo]) -> Self {
        Self {
            pids: infos.iter().filter_map(|info| info.pid).collect(),
        }
    }
```

- [ ] **Step 6: Wire the verb**

`["setup", sheep] => setup_once(sheep).await`, which calls `prepare`, then `cut_over`, then prints one line naming where the sheep now runs from and what it is deployed at:

```rust
    println!("{sheep} now deploys from {}, at {sha}", tree.current().display());
```

That path is the thing an operator has no other way to learn, and the whole of the removal story in Task 9 exists because they would not otherwise think to look there. Extend `USAGE` and the module doc.

- [ ] **Step 7: Run the tests, then mutation-check every branch**

Run: `cargo test --locked`

| mutation | expected |
|---|---|
| drop the `registering.cwd = Some(...)` line | `the_new_registration_names_current_explicitly` red |
| set `cwd` to `tree.release(&sha)` instead of `tree.current()` | same test red. **Run this one specifically**: it is the mistake somebody would actually make, and it is indistinguishable from correct behaviour for exactly one release. |
| **replace the whole two-phase check with `is_new(info) && info.status == Online`** | `a_newcomer_that_went_online_without_serving_is_still_rejected` red, and `a_newcomer_that_crash_loops_through_the_dwell_is_rejected` red. **This is the decisive mutation for this task**: it is the code an earlier draft of this plan specified, it passes every other test here, and against a real shepherd it returned success in 15.6 milliseconds on a dead release. |
| drop the dwell, keeping only phase one | both of those tests red |
| in the dwell, compare instance counts rather than pids | `a_newcomer_that_crash_loops_through_the_dwell_is_rejected` red, because a crash-looped newcomer is present at the same count under a different pid |
| drop the `restarts > 0` check | nothing moves, because the pid check already catches every fixture. **Leave it in and describe it as belt and braces** in the doc, which the implementation does, rather than claiming a test pins it. Do not add a fixture that respawns onto the same pid; it is not a thing a real shepherd does and a test that arranges it would be pinning fiction. |
| delete by name rather than by id | `only_the_old_instances_are_deleted_and_by_id` red |
| take the ids from a describe AFTER the `Start` rather than before | `only_the_old_instances_are_deleted_and_by_id` red on the newcomer assertion. **Run this one**: it deletes the release that was just deployed, and it is invisible until two rows share the name. |
| delete only the first id | `every_replaced_instance_is_deleted` red |
| skip the deletes entirely | `only_the_old_instances_are_deleted_and_by_id` red. This is the version that was tried and reversed; see the preamble for why it does not survive a later deploy. |
| drop the `undo_start` call from `NotVerified` | `a_newcomer_that_never_comes_up_is_deleted_and_the_old_one_kept` red |
| drop only the repair `Start` from `undo_start`, keeping the delete | `an_abandoned_cutover_puts_the_original_config_back_in_the_roll` red. **This is the second decisive one**: the delete alone looks like a complete cleanup and leaves the roll poisoned. |
| have `undo_start` always return `true` | `a_failed_roll_repair_is_named_not_glossed` red |
| have `NotStarted` call `undo_start` too | `a_refused_start_deletes_nothing` red |
| move `state.write` above the `match` | `the_record_advances_only_after_the_newcomer_is_online` red |

**One mutation to run against a real shepherd rather than a fake, because no fake can be wrong about it in the right way.** Set `attempt`'s phase-one predicate back to `Online` and run Task 13's opt-in test against a release whose script exits immediately. The unit fixtures reproduce the shape; only a real daemon reproduces the timing, and the 15.6 millisecond figure that made this finding undeniable came from one.

- [ ] **Step 8: README**

Add `shep-deploy setup <sheep>` to Usage, and a short paragraph under it:

```markdown
`setup` takes a sheep over: it builds the tree, clones, links the shared files in, builds the first release, and re-registers the sheep with its `cwd` set to `current`. **The first cutover is the one deploy that may have downtime.** It runs two instances at once, so an app that does not bind with `SO_REUSEPORT` cannot take its own port while the original still holds it, and the new instance is then removed and the original left serving. Every deploy after the first replaces the instance rather than joining it, and does not meet this.

**It is also the one deploy that is not verified against the readiness probe.** shep reports a freshly started process `Online` once its `listen_timeout` elapses, whatever the probe said, and only aborts a *reload* whose replacement was not ready. So `setup` checks what it can: a new process started and was still the same process, not errored and not restarted, ten seconds later. A release that starts, stays up and serves nothing passes that. Every deploy after the first is verified properly, against the probe, with automatic rollback.

A release that starts, stays up and serves nothing passes that check. That is the one gap, it applies to the first cutover only, and every deploy after it is verified against the probe with automatic rollback.
```

- [ ] **Step 9: Commit**

```bash
git add src/optin.rs src/daemon.rs src/error.rs src/main.rs README.md
git commit -F- <<'EOF'
feat: opt-in part two, cut the sheep over to current

Three measured facts decide this and none was guessable.

A Flockfile app's DEFAULTED cwd is resolved at REGISTRATION, so registering
from inside a release pins the sheep to that release and every later swap
moves a symlink the app no longer reaches. Measured on a real shepherd: the
reload after a swap re-ran the old release's script. So the cutover sets cwd
explicitly, to the current symlink, and a test pins it. This is the single
most load-bearing line in the crate and it fails silently, one release
later, when it is wrong.

Request::Start on an already-registered name ADDS instances rather than
re-registering: do_start calls instance_slots(&existing, instances), and
instance_slots(&[0], 1) is [1]. That is what makes the spec's cutover
implementable at all, and it is also why the old instances are removed by
ID. A name selector would delete the replacement along with what it
replaced.

The first cutover may have downtime, and that is a failure to handle rather
than a caveat to write down. Both instances are alive at once, so without
SO_REUSEPORT the newcomer cannot take the port and dies. It is then removed
and the original left serving, with an error naming the cause, because the
alternative is a permanently errored second instance of the operator's app
and nothing saying which is which.

Verification here is the alive check, deliberately, and it is weaker than
the deploy path's. shep marks a FRESH SPAWN Online when listen_timeout
elapses whatever the readiness probe said (Rin, 2026-08-08, so a slow start
does not become a restart loop) and aborts only a RELOAD replacement on a
readiness timeout. verify::wait is sound precisely because it verifies a
reload; the same reasoning does not transfer. Measured against a real
shepherd, `is_new && Online` returned success in 15.6 milliseconds on a
dead-on-arrival release, after which the healthy original is deleted by id.

So this establishes what it can actually establish: a new process exists,
and after a dwell it is the same process, not errored, and has not
restarted. Nothing more, and the docs say so rather than implying a strength
the check does not have. Four stronger checks were considered and rejected,
the instructive one being running the probe ourselves: in a port collision
the ORIGINAL is still bound to and serving that port, so an external probe
passes exactly when the thing it guards against has happened.

state.verify is not consulted, on purpose. Probed is unavailable on this
path, so honouring it would be a lie and refusing a target for lacking a
gate that cannot be used would be incoherent. The weakness is bounded: once
this lands the target is an ordinary deploy target, and every deploy after
it gets full turnover verification and auto-rollback.

It deletes the instances it replaced, by id, once that check passes. By id
because Start added the newcomer BESIDE them under the same name, and the ids
come from the describe taken BEFORE the Start, since afterwards two rows share
the name and nothing on the wire says which was already there.

Leaving the original running was tried first, on Rin's instruction, and she
reversed it on the evidence. The argument for it was good: the dog can
establish alive and not serving, the operator can hit the endpoint, so put the
judgement where the better information is. What killed it only appeared once
the plan was written out. In the window before the operator ran the delete, a
deploy reloads EVERY instance of the name and replaces each from its own spec,
so the leftover was respawned from the pre-adoption config and served checkout
code while being actively kept alive. Stale and forgotten would have been
tolerable; stale and restarted on every deploy is a deploy that silently half
applies.

The detour is recorded rather than deleted because it paid for itself twice:
it is what surfaced the roll poisoning below, and it is why this crate now
says plainly that Online after a Start carries no readiness information.

The second boundary is the roll, and it does not coincide with the first. An
accepted Start records its config against the sheep's NAME, and deleting the
instance does not undo that while the original keeps the name alive, so the
roll's own prune never drops it. Reproduced end to end: after the delete the
roll named the new release while the live pid executed in the old one, and
a restart plus muster brought the sheep back FROM THE ABANDONED RELEASE. So
an abandoned cutover now re-registers the original config and deletes the
instance that necessarily spawns, and when that repair cannot be made the
error says so, because it is the one half an operator cannot see in
shep flock.
EOF
```

---

## Task 9: On-remove restore

**Files:**
- Create: `src/restore.rs`
- Modify: `src/main.rs`, `README.md`

**Interfaces:**
- Produces:
  ```rust
  pub enum Restored {
      Returned { sheep: String, to: PathBuf },
      LeftRunning { sheep: String, from: PathBuf },
      /// Nothing was changed. The sheep is still registered and running.
      Failed { sheep: String, why: String },
      /// The delete succeeded, both the restore and the fallback failed,
      /// and the sheep is now gone from the flock AND from the roll.
      Lost { sheep: String, why: String },
  }
  pub async fn all<D: Daemon>(daemon: &D, shep_home: &Path) -> Vec<Restored>;
  pub fn report(results: &[Restored]) -> String;
  ```

**NOT blocked on shep prerequisite 2.** The hook is the shepherd calling this; the subcommand is this. Build it now, directly runnable, and it starts being called automatically the day shep ships the hook. A dog that has not implemented the hook simply exits non-zero on an argument it does not recognise, exactly as `shep-log-rotate` does today, so shipping this early costs nothing and is what makes the shep-side change a one-line addition rather than a coordinated release.

**The failure this prevents, in Rin's own framing.** An operator rehomes the dog, goes back to `~/ReactMap` because that is where they think their app lives, restarts the sheep, and cannot work out why nothing updates. That sheep's `cwd` was never `~/ReactMap`; it was a path under `$SHEP_HOME` they have no reason to know about. So removing the dog puts the sheep back where they will look.

**Two cases, both answered from `deploy.toml`.** A sheep that **pre-existed the dog** has an `origin_cwd` and `origin_script` recorded at opt-in and is restored to them. A sheep the **dog bootstrapped** has neither, so there is nothing to restore: it is left running from `current`, unchanged, **and told so plainly**. Deleting an app because a deploy tool was uninstalled would be far worse than leaving it, and Rin's condition for accepting the second case is exactly that the message reaches the operator. Without it, "left running, unchanged" is indistinguishable from "quietly abandoned somewhere you will not think to look", which is the failure this whole section exists to prevent.

**Failure must never block removal.** An operator asking to remove something is entitled to have it removed. Every failure here becomes a row in the report and the process still exits 0.

**But "failure never blocks removal" must not become "failure quietly deletes the app", and an earlier draft of this task had exactly that bug.** `Request::Delete` is stop plus deregister, and `FlockRegistry::roll` drops a name with no live instance ("a deleted sheep must not resurrect"). So delete-then-start with a `Start` that is refused, by a bad `origin_script`, a `user` that no longer resolves, or a transport failure, leaves the sheep **gone from the flock and gone from the roll**, not returning on a reboot. A `Restored::Failed { sheep, why }` row cannot say that, and an operator reading "could not be restored" would reasonably assume their app was still running.

Two things follow. **A refused restore is retried with the config the shepherd had a moment ago**, which is held from before the delete, so the ordinary transient failure costs nothing. And when that fallback fails too, the row is `Restored::Lost`, which says in the report that the sheep has been deleted and what it was. That is the difference between a report an operator can act on and one that misleads them at the worst moment.

**The delete-then-start order is still right here, and unlike the cutover it leaves a clean roll.** Deleting first drops the name, and the following `Start` re-records the restored config against it, so there is no poisoned entry to repair. The cutover cannot do this because it must keep the original alive while the newcomer proves itself.

**The deploy tree is left on disk either way.** It is not the dog's to delete, and in the bootstrap case a running app is still pointing into it.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a sheep that pre-existed the dog is not put back where its
    /// operator will look for it. This is the whole point: they will go to
    /// ~/ReactMap, because that is where they think their app lives, and
    /// the cwd it has been running under is a path beneath $SHEP_HOME they
    /// have no reason to know about.
    #[tokio::test]
    async fn a_pre_existing_sheep_goes_back_to_its_own_checkout() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
        let daemon = Recording::with_registered("bpm");

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
        let daemon = Recording::with_registered("bpm");

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
        let daemon = Recording::with_registered("ctm");

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
        let daemon = Recording::refusing_first_start_only("aaa");

        let results = all(&daemon, home.path()).await;

        assert_eq!(results.len(), 2);
        // `Failed`, not `Lost`: the fallback re-registered what the shepherd
        // already had, so "aaa" is still running. A double that refused
        // EVERY start would give `Lost` here, which is a different claim and
        // has its own test below.
        assert!(matches!(results[0], Restored::Failed { .. }), "{:?}", results[0]);
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
        let daemon = Recording::refusing_first_start_only("bpm");

        let results = all(&daemon, home.path()).await;

        assert!(matches!(results[0], Restored::Failed { .. }), "{:?}", results[0]);
        assert_eq!(daemon.started().len(), 2, "the restore, then the fallback");
        assert_eq!(
            daemon.started()[1].cwd.as_deref(),
            Some("/srv/deploy-tree/current"),
            "the fallback re-registers what the shepherd had, not the restore"
        );
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
        let daemon = Recording::refusing_every_start();

        let results = all(&daemon, home.path()).await;

        assert!(matches!(results[0], Restored::Lost { .. }), "{:?}", results[0]);
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
        all(&Recording::with_registered("bpm"), home.path()).await;
        assert!(home.path().join("deploy/bpm/deploy.toml").is_file());
    }
}
```

`Recording` is this task's `Daemon` double, in four shapes. All of them answer `save_roll` with a roll naming the given sheep (`cwd` under the deploy tree, `script` whatever it was registered with) and `describe` with one running instance:

| constructor | `start` behaviour | reaches |
|---|---|---|
| `Recording::with_registered(sheep)` | accepts | the ordinary restore |
| `Recording::refusing_first_start_only(sheep)` | refuses the first, accepts the second | the fallback re-registering what the shepherd had |
| `Recording::refusing_every_start()` | refuses every one | `PutBack::Deleted`, the sheep genuinely gone |

`Recording::calls()` records **only `delete` and `start`**, not `save_roll` or `describe`. Those two are reads, and the ordering this pins is between the two calls that change the flock; recording all four would make the assertion a transcript of the implementation rather than a statement about the behaviour, and it would break the moment somebody reordered a read.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --locked a_pre_existing_sheep_goes_back_to_its_own_checkout`
Expected: FAIL, `all` not defined.

- [ ] **Step 3: Implement**

```rust
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
    // them.
    let registered = roll::registered(daemon).await.unwrap_or_default();

    let mut results = Vec::new();
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
        // tool was uninstalled would be much worse than leaving it.
        let (Some(cwd), Some(script)) = (state.origin_cwd, state.origin_script) else {
            results.push(Restored::LeftRunning {
                sheep,
                from: tree.current(),
            });
            continue;
        };

        results.push(match put_back(daemon, &sheep, &registered, &cwd, &script).await {
            PutBack::Done => Restored::Returned { sheep, to: cwd },
            PutBack::Untouched(err) => Restored::Failed {
                sheep,
                why: err.to_string(),
            },
            PutBack::Deleted(err) => Restored::Lost {
                sheep,
                why: err.to_string(),
            },
        });
    }
    results
}

/// What happened to one sheep, distinguishing "nothing changed" from
/// "it is deleted", because those need different words in the report.
enum PutBack {
    /// Re-registered at its own checkout.
    Done,
    /// Nothing was deleted, so the sheep is untouched.
    Untouched(Error),
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
    for info in &live {
        if let Err(err) = daemon.delete(info.id).await {
            // Partway through: some instances may be gone. Not `Untouched`,
            // and not `Deleted` either, since the name may still be live.
            // Reported as a failure that names the sheep, and the operator
            // sees the truth in `shep flock`.
            return PutBack::Untouched(err);
        }
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
                PutBack::Untouched(err)
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
            // The row an operator must not misread. Every other outcome
            // leaves a running app; this one does not, and "could not be
            // restored" would have them assume it did.
            Restored::Lost { sheep, why } => format!(
                "{sheep} IS NO LONGER REGISTERED: restoring it failed ({why}) and so did putting \
                 its previous configuration back, so it is stopped and gone from the flock. It \
                 will not come back on its own after a restart. Re-register it from its own \
                 Flockfile.\n"
            ),
        })
        .collect()
}
```

In `main.rs`:

```rust
        ["on-remove"] => return on_remove().await,
```

with its own connection, matching every sibling verb rather than reaching for a `daemon` that does not exist in `main`'s scope:

```rust
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
    // Built from `home` rather than through `socket()`, which is fallible
    // for the same reason `shep_home` is and has already been answered here.
    let socket = home.join("run").join("shep.sock");
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
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --locked`
Expected: all pass.

- [ ] **Step 5: Mutation-check every branch**

| mutation | expected |
|---|---|
| swap `delete` and `start` | `the_old_registration_is_removed_before_the_new_one_is_started` red |
| keep the registered `cwd` instead of `origin_cwd` | `a_pre_existing_sheep_goes_back_to_its_own_checkout` red |
| restore the `origin_cwd`-absent case by deleting the sheep | `a_bootstrapped_sheep_is_left_running_and_named_in_the_report` red |
| drop the `LeftRunning` line from `report` | same test red, which is the half Rin made her condition |
| `return` on the first error instead of collecting | `a_failure_is_reported_and_the_rest_still_run` red |
| drop the fallback `start(current)` from `put_back` | `a_refused_restore_puts_the_shepherds_own_config_back` red |
| collapse `PutBack::Deleted` into `PutBack::Untouched` | `a_sheep_left_deleted_says_so_in_those_words` red. **This is the decisive one for this task**: both rows read as a failure, and only one of them means the operator's app is gone. |
| soften `Restored::Lost`'s wording to match `Failed`'s | same test red |
| have the `on-remove` arm return `ExitCode::FAILURE` when any row failed | nothing moves. **Pin it**: assert in a test that `report` on a slice containing a `Failed` row still names the other rows, and state in the doc that the exit status is deliberately unconditional. |

- [ ] **Step 6: README and commit**

Add to Usage:

```markdown
`shep-deploy on-remove` is the lifecycle hook: shep runs it before forgetting the dog, and it puts every sheep back where it ran before the dog took over. A sheep the dog bootstrapped has nowhere to go back to, so it is left running from `current` and the report says exactly that, with the path. **The deploy tree is never deleted.** It is not the dog's to delete, and in the bootstrap case a running app is still pointing into it.
```

```bash
git add src/restore.rs src/main.rs README.md
git commit -F- <<'EOF'
feat: put every sheep back on removal

The failure this prevents, in Rin's own framing: an operator rehomes the
dog, goes back to ~/ReactMap because that is where they think their app
lives, restarts the sheep, and cannot work out why nothing updates. That
sheep's cwd was never ~/ReactMap. It was a path under $SHEP_HOME they have
no reason to know about.

Two cases, both answered from deploy.toml. A sheep that pre-existed the dog
has origin_cwd and origin_script recorded at opt-in and goes back to them. A
sheep the dog bootstrapped has neither, so there is nothing to restore: it
is left running from current, unchanged, and TOLD so, with the path.
Deleting an app because a deploy tool was uninstalled would be much worse
than leaving it, and Rin's condition for accepting that case is exactly that
the message reaches the operator. Without it, "left running, unchanged" is
indistinguishable from "quietly abandoned somewhere you will not think to
look", which is the failure this whole thing exists to prevent.

Delete before start, and the order is tested, for the same reason the
cutover deletes by id: Start on a registered name adds an instance rather
than re-registering, so starting first leaves the sheep running from both
places at once.

Failure never blocks removal. Every failure becomes a row in the report and
the process still exits 0, because an operator asking to remove something is
entitled to have it removed, and a dog arguing about its own uninstallation
is worse than one that does nothing.

But "failure never blocks removal" must not become "failure quietly deletes
the app", and it did. Delete is stop PLUS deregister, and the roll drops a
name with no live instance, so a refused Start between them leaves the sheep
gone from the flock and gone from the roll, not returning after a reboot. So
a refused restore now retries with the config the shepherd had a moment ago,
which covers the common causes (a bad origin_script, a user that no longer
resolves, a transport blip), and only when THAT fails is the sheep really
gone. That case gets its own report row saying so in words nobody can
misread, because every other row leaves a running app and this one does not.

The deploy tree is left on disk either way. It is not the dog's to delete,
and in the bootstrap case a running app is still pointing into it.

NOT blocked on the shep-side hook. The hook is shep calling this; this is
the subcommand, directly runnable today, and it starts being called
automatically the day shep ships the hook. A dog without one exits non-zero
on an argument it does not recognise, exactly as shep-log-rotate does, so
shipping early costs nothing and makes the shep change one line rather than
a coordinated release.
EOF
```

---

## Task 10: The smit string

**Files:**
- Create: `src/smit.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Produces: `pub fn text(state: &State) -> String;`

**Why this is its own task, separate from publishing it.** The wire request does not exist yet: smits are shep prerequisite 6, and `shep-client` has no way to send one. The string, though, is entirely this crate's and entirely testable today, so it ships now and Task 12 becomes one small blocked task that adds a trait method, its `Live` impl and one call site, rather than a large blocked task that also has to invent the format under time pressure.

**The two-width exact-string test is shep's, not this crate's.** Rin's ruling was that the smit may be dropped on a narrow terminal *because* it is seen regularly at full width, which carries a requirement her permission does not state outright: it must never be dropped at full width, and must not be crowded out there by a later change either. That is a property of `shep flock`'s adaptive column dropping, which lives in shep. **Carry it to prerequisite 6 as a requirement, and do not try to test it here**, where there is no terminal and no table.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a watched and a manual target render the same. The smit
    /// exists so `shep flock` answers "which of these is actually being
    /// watched" without a second command, and two targets that look
    /// identical answer nothing.
    #[test]
    fn watched_and_manual_are_told_apart_at_a_glance() {
        assert_eq!(text(&target(Watch::Auto, Some("a1b2c3d4e5f6"))), "▲ main@a1b2c3");
        assert_eq!(text(&target(Watch::Manual, Some("a1b2c3d4e5f6"))), "⏸ main@a1b2c3");
    }

    /// fails if the branch stops being named. It is the fact an operator
    /// most often wants and cannot get anywhere else in `shep flock`: three
    /// sheep deployed from one repository differ by branch, and Rin runs
    /// exactly that arrangement with bpm, ctm and opm.
    #[test]
    fn the_branch_is_named_not_assumed() {
        let mut state = target(Watch::Auto, Some("a1b2c3d4e5f6"));
        state.branch = "stable".to_owned();
        assert_eq!(text(&state), "▲ stable@a1b2c3");
    }

    /// fails if a target with no deploy yet renders as a lie. `deployed` is
    /// None between opt-in and the first verified deploy, and printing an
    /// empty sha there would read as "deployed at nothing" rather than
    /// "not deployed yet".
    #[test]
    fn a_target_with_no_deploy_yet_says_so() {
        assert_eq!(text(&target(Watch::Auto, None)), "▲ main@none");
    }

    /// fails if a sha shorter than the abbreviation panics or is padded.
    /// Nothing this crate writes produces one, so this is about a
    /// hand-edited deploy.toml degrading rather than taking the dog down on
    /// its next poll.
    #[test]
    fn a_short_sha_degrades_rather_than_panicking() {
        assert_eq!(text(&target(Watch::Auto, Some("abc"))), "▲ main@abc");
    }
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --locked watched_and_manual_are_told_apart_at_a_glance`
Expected: FAIL, `text` not defined.

- [ ] **Step 3: Implement**

```rust
//! The short string this dog attaches to a sheep, which `shep flock` paints
//! without understanding it.
//!
//! ```text
//! ▲ main@a1b2c3      watched
//! ⏸ main@a1b2c3      manual
//! ```
//!
//! The two marks differ so `shep flock` answers "which of these is actually
//! being watched" without a second command, which was the requirement
//! smits were designed for.
//!
//! # Ephemeral, and that is a lifecycle decision
//!
//! The daemon holds smits in memory and never persists them. When a dog
//! stops for any reason, disabled, rehomed, crashed, or lost to a daemon
//! restart, its smits go with it and it republishes on its next poll. The
//! alternative, persisting them, leaves `shep flock` showing a mark
//! attributed to a dog that no longer exists, and removes that orphan class
//! only by adding cleanup to every path that can stop a dog.
//!
//! # Width is shep's half, not this one
//!
//! The smit may be dropped on a narrow terminal, and may never be dropped
//! at a full-width one, because being seen regularly at full width is what
//! makes dropping it acceptable at all. That is a property of `shep flock`'s
//! adaptive column dropping and is pinned there, not here: this module has
//! no terminal and no table.

use crate::state::{State, Watch};

/// How many characters of a sha the smit shows.
///
/// Six, matching what git itself abbreviates to by default at this
/// repository size, and short enough that the whole smit stays inside a
/// column that has to compete with nine others.
const ABBREVIATED: usize = 6;

/// The smit for one target.
///
/// `none` rather than an empty sha for a target between opt-in and its
/// first verified deploy: `main@` reads as deployed at nothing, where
/// `main@none` reads as not deployed yet.
#[must_use]
pub fn text(state: &State) -> String {
    let mark = match state.watch {
        Watch::Auto => '▲',
        Watch::Manual => '⏸',
    };
    let sha = state.deployed.as_deref().map_or("none", |sha| {
        // `get`, not a slice: a hand-edited deploy.toml carrying a short
        // sha must degrade to a shorter smit, never take the dog down on
        // its next poll.
        sha.get(..ABBREVIATED).unwrap_or(sha)
    });
    format!("{mark} {}@{sha}", state.branch)
}
```

- [ ] **Step 4: Run the tests, then mutation-check**

| mutation | expected |
|---|---|
| use the same mark for both `Watch` arms | `watched_and_manual_are_told_apart_at_a_glance` red |
| hardcode `"main"` instead of `state.branch` | `the_branch_is_named_not_assumed` red |
| `map_or("", ...)` instead of `map_or("none", ...)` | `a_target_with_no_deploy_yet_says_so` red |
| `&sha[..ABBREVIATED]` instead of `sha.get(..)` | `a_short_sha_degrades_rather_than_panicking` red, with a panic rather than an assertion failure, which is the point |

- [ ] **Step 5: Commit**

```bash
git add src/smit.rs src/main.rs
git commit -F- <<'EOF'
feat: the smit string a sheep gets painted with

Its own commit, separate from publishing it, because the wire request does
not exist yet: smits are a shep-side prerequisite and shep-client 0.1.0 has
no way to send one. The string is entirely this crate's and entirely
testable today, so it ships now, and the blocked task shrinks to a trait
method, a Live impl and one call site rather than also having to invent the
format later under time pressure.

The two marks differ so shep flock answers "which of these is actually being
watched" without a second command, which is the requirement smits were
designed for at all.

`none` rather than an empty sha between opt-in and the first verified
deploy: main@ reads as deployed at nothing, where main@none reads as not
deployed yet.

sha.get(..6) rather than a slice. Nothing this crate writes produces a short
sha, so this is about a hand-edited deploy.toml degrading to a shorter smit
instead of taking the dog down on its next poll.

Width is shep's half and the module doc says so. The smit may be dropped on
a narrow terminal, and may never be dropped at full width, because being
seen regularly at full width is what makes dropping it acceptable at all.
That is a property of shep flock's adaptive column dropping and belongs
where the table is; this module has no terminal.
EOF
```

---

## Task 11: The poll loop

**Files:**
- Create: `src/poll.rs`
- Modify: `src/main.rs`, `README.md`

**Interfaces:**
- Consumes: `config::read`, `paths::targets`, `State`, `deploy::deploy`.
- Produces:
  ```rust
  pub async fn run<D: Daemon>(daemon: &D, shep_home: &Path, config: DogConfig) -> Result<(), Error>;
  async fn tick<D: Daemon>(daemon: &D, shep_home: &Path, config: DogConfig) -> Vec<(String, Result<Outcome, Error>)>;
  fn due(state: &State) -> bool;
  ```

**Serial by construction, and no mid-deploy abort.** One tick deploys its targets one at a time, and a tick never begins while the previous one is still running, because the loop is a plain `loop { tick().await; sleep(interval).await; }`. A push landing during a build is therefore picked up on the next tick rather than aborting the current one. The spec asks for the abort; this is the deferral named at the top of this plan, and the cost is one extra build of latency before a hotfix lands, not a wrong outcome. **The upside is that the engine's own concurrency guard stays a guard rather than becoming load-bearing**: `deploy` refuses when `current` moved while it was preparing, and that refusal is for a second *operator* invocation, not for the loop racing itself.

**A `manual` target is skipped entirely.** Everything else still applies to it, releases, shared linking, the atomic swap, probe verification, auto-rollback and retention all work identically; the only thing that changes is that nothing happens until somebody says so. Pausing a target during an incident is the case that matters most and is exactly when a deploy must not fire.

**One target's failure never stops the loop.** A dog that exits because one of five targets could not reach its remote stops deploying the other four, and nothing restarts it with a reason anybody can read.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a manual target is polled. This is the switch an operator
    /// reaches for during an incident, and a deploy firing at that moment
    /// is the exact opposite of what they asked for. `set_watch` already
    /// has a test that SETTING the mode does not deploy; this is the other
    /// half, that HOLDING it does not either.
    #[tokio::test]
    async fn a_manual_target_is_never_polled() {
        assert!(!due(&target(Watch::Manual)));
        assert!(due(&target(Watch::Auto)));
    }

    /// fails if one target's failure stops the tick. A dog that gives up
    /// because one of five remotes was unreachable stops deploying the
    /// other four, and nothing restarts it with a reason anybody can read.
    #[tokio::test]
    async fn one_targets_failure_does_not_stop_the_others() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target(home.path(), "broken", Watch::Auto, None);   // no git dir at all
        write_target_ready(home.path(), "fine", Watch::Auto);

        let results = tick(&Ready, home.path(), config()).await;

        assert_eq!(results.len(), 2);
        assert!(results[0].1.is_err(), "broken failed");
        assert!(results[1].1.is_ok(), "fine still ran");
    }

    /// fails if a tick with no targets at all is an error. That is every
    /// freshly adopted dog, and it must idle quietly rather than logging a
    /// failure every thirty seconds forever.
    #[tokio::test]
    async fn a_dog_with_no_targets_ticks_quietly() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(tick(&Ready, home.path(), config()).await.is_empty());
    }

    /// fails if the loop stops sleeping the configured interval between
    /// ticks. A loop that polls continuously fetches continuously, which
    /// hammers every remote it watches and reads as a hung dog.
    ///
    /// `start_paused` so a five-minute interval costs no wall clock: the
    /// same idiom the deploy tests use to run out a real verification
    /// budget instantly.
    #[tokio::test(start_paused = true)]
    async fn ticks_are_spaced_by_the_configured_interval() {
        let home = tempfile::tempdir().expect("tempdir");
        let counter = Counting::new();
        let began = tokio::time::Instant::now();

        // Three ticks' worth, then stop.
        let _ = tokio::time::timeout(
            Duration::from_secs(305),
            run(&counter, home.path(), DogConfig { interval: Duration::from_secs(150), retention: 5 }),
        )
        .await;

        assert_eq!(counter.ticks(), 3, "one at t=0, then every 150s");
        assert!(began.elapsed() >= Duration::from_secs(300));
    }
}
```

Two doubles here. `Ready` answers `describe` with a flock that has already turned over, so any target whose git side is sound deploys successfully and the test is about the loop rather than about the deploy. `Counting` records how many times `describe` was asked and answers an unchanging flock, so `run` loops without ever deploying; its `ticks()` is what the interval test asserts on.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --locked a_manual_target_is_never_polled`
Expected: FAIL, `due` not defined.

- [ ] **Step 3: Implement**

```rust
/// Whether the poll loop should deploy this target on its own.
///
/// The only thing `manual` changes. Releases, shared linking, the atomic
/// swap, probe verification, auto-rollback and retention all apply to a
/// manual target identically; the trigger is the whole of the difference.
/// Two cases it serves: somebody who wants the convenience without a deploy
/// on every commit, and pausing a target during an incident without
/// rehoming the dog, which is the same switch and matters more when
/// something is already going wrong.
const fn due(state: &State) -> bool {
    matches!(state.watch, Watch::Auto)
}

/// One pass over every target, deploying each `auto` one that has moved.
///
/// Answers with one row per target it attempted, in name order, rather than
/// stopping at the first failure. A dog that gave up because one of five
/// remotes was unreachable would stop deploying the other four, and nothing
/// would restart it with a reason anybody could read.
///
/// Targets are re-read from disk every tick rather than cached, so a target
/// created, retargeted with `git checkout stable`, or switched to `manual`
/// while the dog runs is picked up on the next pass without a restart.
async fn tick<D: Daemon>(
    daemon: &D,
    shep_home: &Path,
    config: DogConfig,
) -> Vec<(String, Result<Outcome, Error>)> {
    let Ok(names) = paths::targets(shep_home) else {
        return Vec::new();
    };

    let mut results = Vec::new();
    for name in names {
        let tree = Tree::for_sheep(shep_home, &name);
        let Ok(mut state) = State::read(&tree.state_file()) else {
            continue;
        };
        if !due(&state) {
            continue;
        }
        let outcome = deploy::deploy(daemon, &tree, &mut state, config.retention).await;
        results.push((name, outcome));
    }
    results
}

/// Polls forever, deploying what has moved.
///
/// Serial by construction: a tick never begins while the previous one is
/// still running, so two deploys of one sheep cannot come from this loop.
/// A push that lands mid-build is picked up on the NEXT tick rather than
/// aborting the build in flight, which the design asks for and this defers:
/// the cost is one build of latency before a hotfix lands, not a wrong
/// outcome. `crate::deploy`'s own race guard therefore stays a guard, for
/// a second operator invocation, rather than becoming load-bearing here.
///
/// # Errors
/// Never returns `Ok`. It returns only when the process is asked to stop,
/// which the caller treats as a clean exit.
pub async fn run<D: Daemon>(
    daemon: &D,
    shep_home: &Path,
    config: DogConfig,
) -> Result<(), Error> {
    loop {
        for (sheep, outcome) in tick(daemon, shep_home, config).await {
            match outcome {
                Ok(Outcome::UpToDate) => {}
                Ok(Outcome::Deployed { sha }) => println!("{sheep} deployed {sha}"),
                Ok(Outcome::RolledBack { to, why }) => {
                    println!("{sheep} rolled back to {to}: {why}");
                }
                Err(err) => eprintln!("{sheep}: {err}"),
            }
        }
        sleep(config.interval).await;
    }
}
```

`Outcome::UpToDate` prints nothing at all, deliberately: it is the answer to almost every tick of almost every target, and a dog that logged a line per target per thirty seconds would bury the deploys nobody wants to miss under its own heartbeat.

- [ ] **Step 4: Wire the supervised mode**

Replace the placeholder in `main.rs`:

```rust
        // The supervised mode: no argv at all, per the dog contract.
        [] => {
            let daemon = Live::new(Client::connect(&socket()?).await?);
            let config = config::read(&daemon).await?;
            tokio::select! {
                result = poll::run(&daemon, &shep_home()?, config) => result.map(|()| 0),
                () = terminate() => {
                    println!("shep-deploy: stopping");
                    Ok(0)
                }
            }
        }
```

with `terminate()` awaiting whichever of `SIGTERM` or `SIGINT` arrives first, using the `signal` feature already enabled in `Cargo.toml`. **Stopping mid-deploy is not interrupted:** `tokio::select!` cancels the `poll::run` future at its next await point, which can be inside a deploy. That is acceptable and worth saying in the doc rather than leaving for somebody to discover, for one reason: every step is either before the swap, where nothing has been disturbed, or inside `land`, where the shepherd has already been told what to do and finishes it on its own. What a cancellation can lose is the `state.write` after a verified deploy, which leaves `deploy.toml` naming the previous release and the next tick redeploying the same sha. That costs one rebuild and repairs itself, which is the right shape of failure for a signal that means "stop now".

- [ ] **Step 5: Run the tests, then mutation-check**

| mutation | expected |
|---|---|
| `due` to `true` unconditionally | `a_manual_target_is_never_polled` red |
| `?` on the deploy result inside `tick` instead of collecting | `one_targets_failure_does_not_stop_the_others` red |
| drop the `sleep(config.interval)` | `ticks_are_spaced_by_the_configured_interval` red |
| return `Err` from `tick` when `paths::targets` fails | `a_dog_with_no_targets_ticks_quietly` red |
| move `sleep` above the `tick` call | the interval test still passes on tick count but the elapsed assertion moves. **Check it**, and if it does not move, assert the FIRST tick happens at t=0: a dog that waits thirty seconds before its first poll is a dog that looks broken for thirty seconds after every restart. |

- [ ] **Step 6: README**

Rewrite `## Status` entirely, since it is the crates.io page and currently describes an earlier crate:

```markdown
## Status

Working: the deploy sequence, the operator commands, opt-in, the poll loop, retention, and restore on removal. Tested against a real shepherd.

Not built: smits, which need a shep-side wire change, and Windows.
```

and add to Usage. The outer fence here is four backticks so the `toml` block nests inside it; the README itself gets the ordinary three:

````markdown
Adopted as a dog, `shep-deploy` takes no arguments and polls instead. Every 30 seconds by default, it deploys any `watch = "auto"` target whose branch has moved. Configure it in `shep.toml`:

```toml
[dog.deploy]
interval = "30s"
retention = 5
```

`retention` is how many releases each target keeps. It cannot be below 2: the release a failed deploy rolls back to is the second newest, so anything lower would silently disable rollback, and it is refused rather than clamped.
````

- [ ] **Step 7: Commit**

```bash
git add src/poll.rs src/main.rs README.md
git commit -F- <<'EOF'
feat: the poll loop, which is what watch = auto means

Serial by construction: a tick never begins while the previous one runs, so
two deploys of one sheep cannot come from this loop, and deploy's own race
guard stays a guard for a second operator invocation rather than becoming
load-bearing here.

No mid-deploy abort, and that is a deliberate deferral rather than an
oversight. The design asks for a push landing mid-deploy to abort the build
in flight and start again at the newer commit; this picks it up on the next
tick instead. Aborting means threading cancellation through build::run's
child and fetching concurrently with a build, and the cost of not doing it
is one build of latency before a hotfix lands, not a wrong outcome. Rin's
own argument for abort over queue was about waste rather than correctness.

A manual target is skipped entirely and everything else still applies to it.
Releases, shared linking, the atomic swap, probe verification, auto-rollback
and retention are all identical; the trigger is the whole difference.
set_watch already had a test that SETTING the mode does not deploy, and this
adds the other half, that holding it does not either. Pausing a target
during an incident is the case that matters most, and it is exactly when a
deploy must not fire.

One target's failure never stops the tick. A dog that gave up because one of
five remotes was unreachable would stop deploying the other four, and
nothing would restart it with a reason anybody could read.

UpToDate prints nothing. It is the answer to almost every tick of almost
every target, and a line per target per thirty seconds would bury the
deploys nobody wants to miss under the dog's own heartbeat.

Targets are re-read every tick rather than cached, so a target created,
retargeted with git checkout, or switched to manual while the dog runs is
picked up without a restart.

SIGTERM and SIGINT cancel at the next await point, which can be inside a
deploy. That is acceptable and the doc says why rather than leaving it to be
discovered: every step is either before the swap, where nothing has been
disturbed, or inside land, where the shepherd already has its instructions
and finishes on its own. What a cancellation can lose is the state.write
after a verified deploy, which costs one rebuild on the next tick and
repairs itself.
EOF
```

---

## Task 12: Publishing the smit (BLOCKED on shep prerequisite 6)

**Do not start this task until shep has shipped smits (prerequisite 6) and a `shep-client` carrying the request is on crates.io.** It is the only task in this plan that cannot be implemented today. Everything else, including the smit string itself, is finishable against `shep-client` 0.1.0.

**Files:**
- Modify: `src/daemon.rs`, `src/smit.rs`, `src/poll.rs`, `Cargo.toml`, `README.md`

**Interfaces:**
- Produces, on `Daemon`:
  ```rust
  async fn set_smit(&self, sheep: &str, text: &str) -> Result<(), Error>;
  ```
  and in `smit`:
  ```rust
  pub async fn publish<D: Daemon>(daemon: &D, sheep: &str, state: &State) -> Result<(), Error>;
  ```

**Carry these three requirements into the shep-side work, because they are conditions on the mechanism rather than on this crate:**

1. **Smits are ephemeral and owned by the dog that published them.** Held in memory by the daemon, never persisted. When a dog stops for any reason, disabled, rehomed, crashed or lost to a daemon restart, its smits go with it and it republishes on its next poll. Persisting them means a rehomed dog leaves `shep flock` showing a mark attributed to a dog that no longer exists, and removing that orphan class then costs cleanup logic on every path that can stop a dog.
2. **The smit sits low in `shep flock`'s adaptive drop order and may be dropped on a narrow terminal, and may NEVER be dropped at full width.** Rin's permission was conditional on the regular full-width sighting being what makes dropping it acceptable, so a later change that lets something else crowd it out on a wide terminal reopens the question. Pin both ends with an exact-string test: present at full width, absent at narrow.
3. **`output.astro` hard-codes the flock table** as `ID NAME STATUS PID RESTARTS EXIT CPU MEM UPTIME FOLD` and states that ID, NAME and STATUS never drop. A SMIT column lands in that sample, and the same page is where its position in the drop order gets written down.

- [ ] **Step 1: Bump `shep-client` and add the trait method**

```rust
    /// Attach this dog's short string to `sheep`, for `shep flock` to
    /// paint.
    ///
    /// Ephemeral and owned by this dog: the daemon holds it in memory and
    /// drops it when this process stops, for any reason, which is why the
    /// poll loop republishes on every tick rather than only on change.
    ///
    /// # Errors
    /// As [`Self::dog_config`].
    async fn set_smit(&self, sheep: &str, text: &str) -> Result<(), Error>;
```

Implement on `Live` against whatever request shep ships, add the response to `named`, and add the method to every test double.

- [ ] **Step 2: Write the failing test**

```rust
    /// fails if the smit stops being republished every tick. The daemon
    /// holds smits in memory and drops them when the dog stops, so a dog
    /// that published once and then only on change would show nothing at
    /// all after a daemon restart until the next deploy, which for a
    /// healthy target could be weeks.
    #[tokio::test]
    async fn every_tick_republishes_every_targets_smit() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target(home.path(), "bpm", Watch::Auto, Some("a1b2c3d4e5f6"));
        write_target(home.path(), "ctm", Watch::Manual, Some("f6e5d4c3b2a1"));
        let daemon = SmitRecording::default();

        tick(&daemon, home.path(), config()).await;
        tick(&daemon, home.path(), config()).await;

        assert_eq!(
            daemon.smits(),
            vec![
                ("bpm".to_owned(), "▲ main@a1b2c3".to_owned()),
                ("ctm".to_owned(), "⏸ main@f6e5d4".to_owned()),
                ("bpm".to_owned(), "▲ main@a1b2c3".to_owned()),
                ("ctm".to_owned(), "⏸ main@f6e5d4".to_owned()),
            ]
        );
    }

    /// fails if a MANUAL target loses its smit. `due` skips manual targets
    /// for deploying, and the smit is exactly how an operator sees that a
    /// target is paused, so skipping the smit too would hide the state the
    /// smit exists to show.
    #[tokio::test]
    async fn a_manual_target_still_gets_a_smit() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target(home.path(), "ctm", Watch::Manual, Some("f6e5d4c3b2a1"));
        let daemon = SmitRecording::default();
        tick(&daemon, home.path(), config()).await;
        assert_eq!(daemon.smits().len(), 1);
    }

    /// fails if a smit that could not be published takes the tick down with
    /// it. A daemon that refuses one is a daemon this dog can still deploy
    /// through, and a cosmetic call must never cost a deploy.
    #[tokio::test]
    async fn a_refused_smit_does_not_stop_the_tick() {
        let home = tempfile::tempdir().expect("tempdir");
        write_target_ready(home.path(), "fine", Watch::Auto);
        let results = tick(&RefusingSmits, home.path(), config()).await;
        assert!(results[0].1.is_ok(), "the deploy still ran");
    }
```

`SmitRecording` is a `Daemon` double whose `set_smit` appends `(sheep, text)` to a list its `smits()` returns, and which otherwise behaves as `Ready` does in Task 11. `RefusingSmits` is the same with `set_smit` answering an `RpcError`, so the deploy still runs and only the cosmetic call fails.

- [ ] **Step 3: Publish from the tick, above the `due` check**

In `poll::tick`, after reading the state and **before** `if !due(&state)`:

```rust
        // Above the `due` check on purpose: a manual target's smit is
        // exactly how an operator sees that it is paused, so skipping it
        // would hide the state the smit exists to show.
        //
        // Republished every tick rather than on change, because the daemon
        // holds smits in memory and drops them whenever this dog stops. A
        // publish-on-change dog would show nothing after a daemon restart
        // until its next deploy, which for a healthy target could be weeks.
        //
        // A failure is ignored: this is cosmetic, and a daemon that refuses
        // it is one this dog can still deploy through.
        if let Err(err) = smit::publish(daemon, &name, &state).await {
            eprintln!("{name}: could not publish its smit: {err}");
        }
```

- [ ] **Step 4: Run the tests and mutation-check**

| mutation | expected |
|---|---|
| publish only when `state.deployed` changed | `every_tick_republishes_every_targets_smit` red on the second tick's rows |
| move the publish below the `due` check | `a_manual_target_still_gets_a_smit` red |
| change the ignored failure to `?` | `a_refused_smit_does_not_stop_the_tick` red |

- [ ] **Step 5: README and commit**

Move smits out of the README's "Not built" line and into Usage with the two rendered rows.

```bash
git add src/daemon.rs src/smit.rs src/poll.rs Cargo.toml Cargo.lock README.md
git commit -F- <<'EOF'
feat: publish each target's smit on every poll

Every tick, not on change, and the reason is the daemon's lifecycle rather
than laziness. Smits are ephemeral and owned by the dog that published them:
the daemon holds them in memory and drops them whenever the dog stops, for
any reason, disabled, rehomed, crashed or lost to a daemon restart. A
publish-on-change dog would therefore show nothing at all after a daemon
restart until its next deploy, which for a healthy target could be weeks.

Published above the due check, so a MANUAL target still gets one. That is
not an oversight to tidy up later: the smit is exactly how an operator sees
that a target is paused, and skipping it for manual targets would hide the
state the smit exists to show.

A refused smit is ignored with a notice. It is cosmetic, and a daemon that
will not take one is a daemon this dog can still deploy through, so it must
never cost a deploy.
EOF
```

---

## Task 13: Integration against a real shepherd

**Files:**
- Modify: `tests/integration.rs`

**Interfaces:**
- Consumes: the whole crate, through the two real binaries.

**Two tests, and only two.** The tier costs about 31 seconds already and every test here drives a real daemon. Each of these covers something **no unit test with an honest fake can reach**, which is the bar; anything a fake could carry stays a unit test.

**Test one: opt-in end to end.** It proves the three facts Task 8 is built on are still true of the real shepherd rather than only of this crate's fakes: that `Start` on a registered name adds an instance beside the old one, that `Delete` by id removes only the one named, and above all **that the sheep registered with an explicit `cwd` of `current` actually follows a later swap**. That last is the one that matters. It is the fact a fake cannot check, it fails silently exactly one release after the mistake, and it is why the test deploys twice: opt-in, then a second release, then asserts the running process is executing the second one.

**Test two: a poll tick deploys without being asked.** `watch = auto` means nothing until something proves the supervised binary, given no argv at all, connects, reads its own config, finds its targets and deploys one. That is four things a unit test stubs out, including `adopted_name`'s pid lookup against a real flock listing, which is the one piece of self-identification no fake can be wrong about in the same way a real daemon can.

**What deliberately stays out.** Retention, restore, survey and the smit string are all covered by unit tests against honest fakes, and none of them depends on the daemon's real timing or real failure shapes. Adding them here would buy repetition at about 20 seconds each.

- [ ] **Step 1: Write the opt-in test**

```rust
/// fails if a sheep taken over by `setup` does not follow later swaps.
/// This is the fact no fake can check and the one that fails silently
/// exactly one release after the mistake: a Flockfile app's DEFAULTED cwd
/// is resolved at REGISTRATION, so a sheep registered from inside a release
/// is pinned to that release forever and every later swap moves a symlink
/// it no longer reaches. Measured on a real shepherd before this crate
/// existed: the reload after a swap re-ran the OLD release's script.
///
/// It deploys TWICE for that reason. One deploy proves nothing here,
/// because a sheep pinned to release one and a sheep following `current`
/// are indistinguishable while release one IS current.
///
/// It also pins something no unit test can reach: that the cutover deleted
/// the ORIGINAL instance and not the newcomer. Both are real rows under one
/// name on a real shepherd, and an id read from the wrong side of the
/// `Start` deletes the release that was just deployed while every count
/// assertion still passes. The surviving pid is what tells them apart.
#[test]
fn a_sheep_taken_over_by_setup_follows_a_later_swap() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(/* version */ "v1", Readiness::Probe);

    // The sheep as the operator already runs it: registered from their own
    // checkout, with no deploy tree anywhere.
    register_from_checkout(&shepherd, origin.path());
    wait_until("web to come up from the checkout", || {
        described_online(&shepherd, "web")
    });
    let before = described_pid(&shepherd, "web").expect("a pid");

    let setup = shepherd.deploy_args(&["setup", "web"]);
    assert!(setup.status.success(), "{}", String::from_utf8_lossy(&setup.stderr));

    // One instance again, and it must be the NEWCOMER. Two rows shared this
    // name for the length of the cutover, so this is the assertion that
    // catches an id taken from the wrong side of the `Start`.
    wait_until("the cutover to settle", || {
        described_instances(&shepherd, "web") == 1
    });
    assert_ne!(
        described_pid(&shepherd, "web"),
        Some(before),
        "the surviving instance must be the newcomer, not the one it replaced"
    );

    // Now the half a single deploy cannot prove.
    write_run_script(origin.path(), "v2");
    git(origin.path(), &["commit", "-qam", "v2"]);
    let deployed = shepherd.deploy("web");
    assert!(deployed.status.success(), "{}", String::from_utf8_lossy(&deployed.stderr));

    wait_until("the second release to be serving", || {
        last_line(&out_file(&shepherd, "web")).as_deref() == Some("v2")
    });
}
```

Three helpers are new here; the rest (`origin_with_app`, `build_tree`, `write_state`, `register_web`, `last_line`, `wait_until`, `described_online`, `described_pid`, `write_run_script`) already exist in `tests/integration.rs` and are unchanged.

- `register_from_checkout(shepherd, checkout)` writes a Flockfile into the origin checkout and runs `shep start` against it with **no `cwd` key at all**, which is the ordinary way an operator registers an app and precisely the case the explicit-`cwd` rule exists for.
- `described_instances(shepherd, sheep)` counts `"id":` occurrences in the JSON describe, the same way `described_pid` reads a pid without pulling in a JSON dependency.
- `out_file(shepherd, sheep)` returns `<home>/logs/<sheep>-0-out.log`, the path `last_line` reads. Instance `0` specifically: these tests assert on which release is executing, and slot 0 is the one a single-instance app always occupies.

- [ ] **Step 2: Write the poll test**

```rust
/// fails if the supervised mode does not deploy on its own. `watch = auto`
/// means nothing until the binary, given NO ARGV AT ALL as the dog contract
/// requires, connects, works out its own adopted name from its own pid in a
/// real flock listing, reads its own [dog.<name>] section, finds its
/// targets on disk and deploys one. Every one of those is stubbed in the
/// unit tests, and the pid lookup in particular can only be wrong against a
/// real daemon.
#[test]
fn the_supervised_dog_deploys_a_moved_branch_without_being_asked() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app("v1", Readiness::Probe);
    let sha = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &sha, "probed");
    register_web(&shepherd, Readiness::Probe, "");
    wait_until("web to come up", || described_online(&shepherd, "web"));

    // A one-second interval, so the test is bounded by the deploy rather
    // than by the poll.
    fs::write(
        shepherd.home().join("shep.toml"),
        "[dog.deploy]\ninterval = \"1s\"\nretention = 5\n",
    )
    .expect("write shep.toml");

    write_run_script(origin.path(), "v2");
    git(origin.path(), &["commit", "-qam", "v2"]);

    // Adopted and supervised, with no argv, exactly as the contract says.
    shepherd.ok(&["adopt", DEPLOY_BIN, "--style", "bare"]);

    wait_until("the poll loop to deploy v2 on its own", || {
        last_line(&out_file(&shepherd, "web")).as_deref() == Some("v2")
    });
}
```

- [ ] **Step 3: Run the tier**

```bash
cargo install shep --locked
SHEP_BIN="$(command -v shep)" cargo test --features integration --locked
```

Expected: 7 passed (5 carried plus 2 new), and the tier's total wall clock near 31 seconds, since both new tests run inside the drain-window test's own thirty.

- [ ] **Step 4: Prove both are non-vacuous**

Neither test is worth having if it passes against the bug it names. Run both mutations and confirm the failure, then restore.

| mutation | expected |
|---|---|
| in `optin::cut_over`, set `registering.cwd` to `Some(tree.release(&sha).display().to_string())` | `a_sheep_taken_over_by_setup_follows_a_later_swap` fails at the `v2` wait, with the log still reading `v1`. **This is the decisive one**: it is the real mistake, it passes every fake, and it survives the first deploy. |
| in `poll::due`, return `false` unconditionally | `the_supervised_dog_deploys_a_moved_branch_without_being_asked` times out at the wait |
| in `optin::cut_over`, take the ids from a describe AFTER the `Start` | `a_sheep_taken_over_by_setup_follows_a_later_swap` fails: the cutover deletes the newcomer, the surviving pid is the original's, and the `v2` wait then times out. **Run this one against a real shepherd**, because two rows only share a name once a real `Start` has been accepted, which is exactly the condition no fake reproduces on its own. |

Record the measured elapsed time of each test in the commit message, so a later change that makes one of them vacuous by finishing suspiciously fast has a number to be compared against. Plan one's drift test learned that the hard way: a test that had gone vacuous ran at 12.4 seconds where the real one took 28.4.

- [ ] **Step 5: Commit**

```bash
git add tests/integration.rs
git commit -F- <<'EOF'
test: opt-in and the poll loop, against a real shepherd

Two tests, and only two. The tier already costs about 31 seconds and every
test here drives a real daemon, so the bar is whether a unit test with an
honest fake could carry it. Retention, restore, survey and the smit string
all could, and stay where they are.

The opt-in test deploys TWICE, and that is the whole design of it. A
Flockfile app's DEFAULTED cwd is resolved at REGISTRATION, so a sheep
registered from inside a release is pinned to it forever and every later
swap moves a symlink the app no longer reaches. One deploy cannot see that:
a sheep pinned to release one and a sheep following current are
indistinguishable while release one IS current. It also pins the two facts
the cutover is built on, that Start adds an instance beside the old one and
that Delete by id removes only the one named.

The poll test gives the binary NO ARGV AT ALL, as the dog contract requires,
and then asks for a deploy nobody typed. Four things the unit tests stub are
real here, and adopted_name's pid lookup against a live flock listing is the
one that can only be wrong against a real daemon.

Both proved non-vacuous by mutation before committing. The decisive one is
setting the registered cwd to the release rather than to current: it is the
real mistake, it passes every fake in the crate, and it survives the first
deploy, which is exactly why this test needs a second one.
EOF
```

---

## What this plan does NOT cover

- **A mid-deploy abort.** Deferred with its reasoning above.
- **`--commit <sha>`.** Deferred in the spec, not rejected: deploying a named commit asks whether the target is now pinned there or whether the next poll moves it off, and that wants answering deliberately rather than guessing.
- **`watch = major | minor | patch`.** Rin's idea, worked through to a resolved design in the engine plan's ledger (they are ceilings, not filters; the anchor is the deployed version; prereleases excluded by default; a project that does not tag gets a loud refusal). It is a different watch TARGET, tags rather than a branch, and it wants its own plan. Measured 2026-08-26: ReactMap has 308 tags of which 215 are prereleases, Koji 25 of which 12 are, so tag-watching is viable and the prerelease exclusion is not optional.
- **Webhooks.** Deferred in the spec: un-exposing a port after the fact is harder than adding one later.
- **Windows.** See Global Constraints.
- **`shep install` confinement.** A dog holds full socket authority and can issue `Delete` as easily as `Restart`. Nothing here changes that, and it is why the confinement work recorded under `shep install` in shep's `deferred.md` matters more once this exists.

---

## Self-review

Run against the spec with fresh eyes, then re-run after a pre-execution review found two blockers this pass had missed.

### What the pre-execution review found, and what it says about this document

**Two blockers, both in Task 8, both measured against a live shepherd, and both invisible to plan-reading.** That last part is the lesson worth keeping.

- **The cutover's verification verified nothing.** `is_new(info) && Online` on a `Start` establishes only that `listen_timeout` elapsed, because shep marks a fresh spawn `Online` regardless of the probe and aborts only a reload replacement. Measured: `CutOver::Done` in 15.6 milliseconds on a dead release, after which the healthy original is deleted. **This is the engine plan's round-3 blocker recurring on a different path**, and the way it recurred is instructive: this plan's own doc comment spotted the sibling trap ("any instance is `Online` would pass on the OLD instance") and closed only that half. Recognising a pattern is not the same as re-deriving it on new ground.
- **An aborted cutover permanently poisoned shep's muster roll**, and `Error::CutOver`'s text asserted the opposite. `FlockRegistry` is name-keyed and records on every accepted `Start`; deleting the newcomer does not undo it while the original keeps the name alive. A reboot then resurrects the rejected release, and the dog's own `roll::registered` reads poisoned values from that moment.

Both are now fixed, with the design reasoning written into Task 8 rather than only the patch. Three important findings were taken in the same pass: Task 9's restore could silently delete a sheep and report it as a mere failure; Task 3 corrected the spec's stale `CARGO_TARGET_DIR` prose but not the crate's own, which would have shipped two contradictory rustdocs; and the ordering paragraph's "everything else is independent" would have dispatched Task 11 before Task 6 into a compile failure.

**The pattern across all five is one thing: a claim about a system this crate cannot see.** Every one was settled by reading shep's source or running a shepherd, and none by reasoning about the plan. That is the same failure mode the engine plan's ledger records five times, and it is why Task 8 now cites `supervisor.rs` and `snapshot.rs` by line rather than describing their behaviour.

### What the second and third review rounds changed

Three decisions from Rin and two measured facts, plus one review claim this plan does not accept.

- **The cutover deletes the original, once the alive check passes.** This went out and came back: Rin first ruled that the original should be left running with the operator given the exact `shep delete <id>`, then reversed it on evidence the plan itself produced. In the window before the operator ran that command, a deploy reloads every instance of the name and replaces each from its own spec, so the leftover was respawned from the pre-adoption config and served checkout code **while being actively kept alive**. The detour was not wasted: it is what surfaced the roll poisoning, and it is why the plan now states plainly that `Online` after a `Start` carries no readiness information. Task 8 records the reversal with its reasoning rather than quietly reverting, because the argument for the abandoned version is still a good argument and the next person to have it should meet the counter-example.
- **The `shep <dogname>` passthrough is already shipped** in 0.1.1, so prerequisite 3 leaves the outstanding list, and the measured argv answer settles the question this plan flagged: shep does not forward the dog's own name. That gives two argv shapes for one operation, now handled in a dedicated section and a `route` function whose ordering is testable.
- **Exit code 10 was proposed and is rejected, with evidence.** The claim was that shep's taxonomy runs 0 to 9. `crates/shep-cli/src/exit.rs` runs to 11, and `FlockEmpty = 11`'s own doc says it exists because "Codes 0-10 were all spoken for". Worse, 10 is `DaemonAlreadyRunning`, whose constant is public **in this crate's own dependency** (`shep_client::spawn::DAEMON_ALREADY_RUNNING = 10`) and documented as a deliberate cross-process contract. The plan keeps 12 and records the evidence at Task 1, because the next reader will meet the same summary.

**That last one is the round's lesson and it is the same one as the round before.** Both review rounds turned on a claim about a system this crate cannot import, and in both directions: the first found two real blockers by measuring shep, and this one produced one wrong number by summarising it. Read `exit.rs`, `supervisor.rs` and `snapshot.rs`, cite the line, and do not trust a range anybody states from memory, this plan included.

**The third round reversed the cutover decision and closed the document.** Rin's reversal is recorded in Task 8 with the counter-example that produced it rather than as a quiet revert, because the argument for the abandoned version is still a good argument and the next person to have it should meet the evidence rather than the conclusion.

A final read of the whole document after three rounds of edits found four things the rounds had left dangling, all now fixed:

- **Task 9's `a_failure_is_reported_and_the_rest_still_run` asserted `Restored::Failed` against a double that refuses every `start`.** Once round two added the fallback re-registration, that double produces `Lost` instead. The test would have failed on its first run, and the fix is a double that refuses only the first start, which is also the more representative failure.
- **Task 8 named eight fixtures against one sentence of prose.** They are now a table, built from one parameterised double rather than eight functions, each row naming the shape it exists to reach.
- **Eleven test helpers were used and never defined**, several across task boundaries. They are now a "Shared test helpers" section, so two tasks cannot invent the same name with different behaviour.
- **The doubles in Tasks 11 and 12 (`Ready`, `Counting`, `SmitRecording`, `RefusingSmits`) were unnamed.** Described where they are first used.

**Every one of those is the same species: an edit that was locally correct and left a neighbour false.** That is the failure mode the engine plan's whole-branch review named ("no single round's diff is wrong, the composition is"), reappearing in the plan document instead of in the code. A plan edited across rounds needs the same whole-document pass a branch does, and the four above are what it caught.

### 1. Spec coverage

Every section of the spec is implemented by a task here, implemented already by the engine plan, or listed above as deliberately not covered. **The spec has been corrected four times since this plan's first draft**, and re-reading it against the plan resolved three of the four things this document had flagged as departures: `CARGO_TARGET_DIR`, concurrency, and the survey's `needs setup` wording are all now simply the spec.

| spec section | where |
|---|---|
| Smits | Tasks 10 and 12, plus shep prerequisite 6 |
| Layout, `cache/` | Task 3 |
| `[dog.<name>]` configuration | Task 2 |
| `watch` enum, `--watch` flag | engine plan; the `manual` skip is Task 11 |
| `.shepignore` and shared linking | engine plan; the `target` collision is Task 3 |
| The deploy sequence, steps 1 to 7 | engine plan |
| The deploy sequence, step 8 (retention) | Task 4 |
| Verify, both modes | engine plan; the cutover's weaker check is Task 8 |
| Build env and artifacts | engine plan; the `CARGO_TARGET_DIR` correction is Task 3, in the spec **and** in this crate's own rustdoc |
| Bootstrap and first opt-in, including the cutover | Tasks 5, 6, 7, 8 |
| Retention and teardown | Task 4 |
| Security posture | engine plan |
| Removal and what it restores | Task 9 |
| Concurrency | Task 11, now matching the corrected spec rather than departing from it |
| Prerequisites | named with the spec's own numbering, not implemented |
| Documentation shep owes | shep's work, named |
| Open questions, all four closed | poll interval Task 2, `CARGO_TARGET_DIR` Task 3, hostile `.shepignore` accepted, smit width carried to prerequisite 6 |

**One gap the spec itself has**, surfaced by the blocker above rather than by this table: it says the first cutover "may have downtime" and never says how the dog decides a freshly started release is healthy. Task 8 answers it, and the answer is weaker than the deploy path's.

### 2. Placeholder scan

No TBD, no TODO, no "add error handling", no "similar to Task N". Every code step carries real code, and every test carries a stated failure mode in its doc comment.

The first pass found three steps describing an implementation in prose rather than showing it: `render`'s padding, `attempt`'s polling body and `restore::all`'s loop. The instinct was to defend that as a judgement about where repeating obvious code helps a reader. It was not a judgement, it was the failure mode the skill names, and **the two that mattered were where both blockers were hiding.** Writing them out surfaced:

- `render`'s column widths must be `widest + 2` to match its own exact-string test.
- `attempt` needs three items that are private today, now named with an instruction to widen each under its existing name rather than duplicate it.
- `attempt`'s budget is not `deploy::budget`, because a cutover spawns one instance and drains nothing.
- `restore::all` reads the roll once for all targets, so its double records only the two writes.

### 3. Type consistency

Checked against the engine plan and the current source rather than from memory.

- `Tree` gains `cache`/`cache_target` in Task 3, used in 4, 6, 7, 8, 9, 11.
- `State`'s fields are the engine plan's, unchanged. `origin_cwd` is `Option<PathBuf>` and `origin_script` is `Option<String>`; `AppConfig::cwd` is `Option<String>` and `AppConfig::script` is `String`, which is why Tasks 8 and 9 convert explicitly.
- `Prepared` carries `app` **and** `previous_config`, both `AppConfig` and easy to confuse. `app` is the release's definition and is what gets registered; `previous_config` is the operator's and exists only to repair the roll. Task 7 names the local `previous_config` rather than `app` for that reason, and derives `Clone` because Task 8's tests hold the value past the call.
- `Daemon` gains `save_roll` (Task 5), `delete` (Task 8) and `set_smit` (Task 12). Each task adds its method to every existing double.
- `Error` gains `CutOver { sheep, why, repaired }` in Task 8 only. Task 1 matches on `RolledBack`, `Config` and `Connect`, all of which exist today.
- `cut_over` returns `Result<String, Error>`, the sha. An intermediate revision returned a struct carrying leftover instance ids for the caller to print; that is gone with the leftover, and no vestige of it remains.
- `route<'a>(args: &[&'a str]) -> Route<'a>` is new in Task 1 and is the only place the argv shapes are decided. Every later task that adds a verb adds it to `Route` and to `route`'s match, above the two bare-name arms.
- `Restored` gains a fourth variant, `Lost`, in Task 9. `report` matches all four with no wildcard, so a fifth would force a decision about its wording rather than falling into a generic arm.
- `deploy::deploy` gains a fourth parameter, `keep: usize`, in Task 4, so **Task 4 must update the call site Task 1 wrote**.
- **Six items widen to `pub(crate)` in Task 8** and one is added, which is why its Files list includes `src/verify.rs` and `src/deploy.rs`: `Generation::is_new`, `Generation::holds`, `verify::is_alive`, `verify::DWELL`, `verify::POLL` and `deploy::RELOAD_DEADLINE_SLACK`, plus the new `Generation::of_infos`. Each keeps its existing name; none is duplicated. `Generation::instances` is already `pub` and needs nothing.
- `ProcessInfo::id` is `u32`, which `Daemon::delete` takes; `ProcessInfo::pid` is `Option<u32>` and is `Generation`'s business. The cutover deletes by id and compares by pid, and mixing them would compile.

### 4. Ordering

**Stated as a graph rather than as prose, because a subagent-driven executor parallelises whatever the plan calls independent.** An earlier draft named three constraints and then said "everything else is independent", which would have dispatched Task 11 before Task 6 straight into a compile failure: `paths::targets` is defined in Task 6 and called by Task 11.

| task | must land after | why |
|---|---|---|
| 4 | 3 | prunes release worktrees the cache layout sits beside |
| 6 | 5 | `survey` reads the roll |
| 7 | 5, 6 | reads the roll, and `prepare`'s refusal depends on `paths::targets` semantics |
| 8 | 7 | consumes `Prepared`, including `previous_config` |
| 9 | 5, 6 | reads the roll and enumerates targets |
| 11 | 2, 4, 6 | `DogConfig`; `deploy`'s `keep` argument; **`paths::targets`** |
| 12 | 10, 11 | publishes from `tick`, using the string |
| 13 | 8, 11 | drives opt-in and the poll loop end to end |

Tasks 1, 2, 3 and 5 have no predecessors and may run in parallel. Everything else follows this table, and **a task's absence from a row means it has no predecessors, never that it may run whenever.**
