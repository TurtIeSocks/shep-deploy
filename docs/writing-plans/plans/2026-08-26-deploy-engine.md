# shep-deploy: deploy engine implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `shep-deploy deploy <sheep>` performs one complete verified deploy of an already-configured target: fetch, worktree, link shared files, build, atomically swap, reload, verify against the sheep's readiness probe, and roll back if it does not come up.

**Architecture:** A Rust binary consuming `shep-client` (which re-exports `shep-core`) from crates.io. It drives shep only over the existing socket and owns everything on disk under `$SHEP_HOME/deploy/<sheep>/`. Every module that touches the shepherd goes through one `Daemon` trait so the whole engine is unit-testable without a running daemon, mirroring how `shep-log-rotate` is built.

**Tech Stack:** Rust 2024, MSRV 1.88, `shep-client` 0.1.0, `tokio` (current-thread), `toml`, `serde`. Git operations shell out to `git` rather than linking a git library.

**Source of truth:** [the design spec](../../brainstorming/specs/2026-08-26-deploy-dog-design.md). Where this plan and the spec disagree, the spec wins and the plan is wrong.

## Global Constraints

- **MSRV 1.88, edition 2024.** Match shep's.
- **`shep-client = "0.1.0"`** from crates.io. It re-exports `shep_core`, so do not depend on `shep-core` separately.
- **License `MIT OR Apache-2.0`**, matching shep and `shep-log-rotate`.
- **`#![forbid(unsafe_code)]`** at the crate root.
- **One small crate-level error enum with a manual `Display`** (shep's IR-18 and IR-19), living in `src/error.rs` and gaining variants as tasks need them. No `anyhow`. `shep-log-rotate`'s `src/error.rs` is the exact shape to copy: it has a single `Error` for the whole binary, and this plan follows it rather than splitting per module.
- **Every public item needs docs and a deliberate `Debug` decision.** Redact anything carrying env or secrets, and pin the redaction with an exact-string test (shep's IR-41).
- **The dog never writes to the user's checkout.** It reads from it and symlinks into releases. Any code path that would write there is a bug.
- **`user`/`group` are never taken from a repo-supplied Flockfile.** See spec, "Pinning".
- **Prerequisites live in shep, not here.** Tasks note which they depend on. Nothing in this plan is blocked on all five.

---

## File structure

| file | responsibility |
|---|---|
| `src/main.rs` | binary entry, operator argument surface, wiring |
| `src/error.rs` | the crate's error enum and `Display` |
| `src/daemon.rs` | the `Daemon` trait, the `Live` impl over `Client`, `adopted_name` |
| `src/paths.rs` | the `$SHEP_HOME/deploy/<sheep>/` layout, one place that knows it |
| `src/state.rs` | `deploy.toml` read/write |
| `src/shared.rs` | ignored-file enumeration, `.shepignore` subtraction, symlinking |
| `src/git.rs` | fetch, branch resolution, worktree lifecycle |
| `src/flockfile.rs` | repo Flockfile parse, override deep merge, `AppConfig` production |
| `src/build.rs` | build command execution, env, artifact copying |
| `src/swap.rs` | atomic symlink swap and its inverse |
| `src/verify.rs` | waiting for `Online`, or for stayed-alive |
| `src/deploy.rs` | the orchestration that calls the rest in order |

---

## Task 1: Cargo scaffold, error type, CI

**Files:**
- Create: `Cargo.toml`, `src/main.rs`, `src/error.rs`, `.github/workflows/test.yml`
- Modify: `README.md`

**Interfaces:**
- Produces: `Error` enum with variants `Connect(ConnectError)`, `Request(RequestError)`, `Protocol(String)`, `Config(String)`, `Io { path: PathBuf, source: std::io::Error }`, `Git { command: String, status: Option<i32>, stderr: String }`. Every later task adds no new top-level error type; they add variants here.

- [ ] **Step 1: Write `Cargo.toml`**

```toml
[package]
name = "shep-deploy"
version = "0.1.0"
edition = "2024"
rust-version = "1.88"
license = "MIT OR Apache-2.0"
description = "A deploy dog for shep: watches a git branch, builds a release, swaps to it, and rolls back if it does not come up"
repository = "https://github.com/TurtIeSocks/shep-deploy"
keywords = ["shep", "process-manager", "deploy", "git"]
categories = ["command-line-utilities"]

[dependencies]
shep-client = "0.1.0"
tokio = { version = "1", default-features = false, features = ["rt", "macros", "time", "process", "signal"] }
toml = "0.8"
serde = { version = "1", features = ["derive"] }

[features]
integration = []
```

- [ ] **Step 2: Write the failing test for `Display`**

In `src/error.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// fails if an Io error stops naming the path it failed on. A deploy
    /// touches many paths and "permission denied" without one is unactionable.
    #[test]
    fn an_io_error_names_the_path() {
        let err = Error::Io {
            path: PathBuf::from("/srv/x/releases/abc/dist"),
            source: std::io::Error::other("permission denied"),
        };
        assert!(err.to_string().contains("/srv/x/releases/abc/dist"));
    }

    /// fails if a git failure hides stderr. git's own message is the only
    /// useful thing when a fetch or worktree add fails.
    #[test]
    fn a_git_error_carries_stderr() {
        let err = Error::Git {
            command: "git fetch origin".to_string(),
            status: Some(128),
            stderr: "fatal: could not read Username".to_string(),
        };
        let shown = err.to_string();
        assert!(shown.contains("git fetch origin"));
        assert!(shown.contains("could not read Username"));
    }
}
```

- [ ] **Step 3: Run it and watch it fail**

Run: `cargo test -p shep-deploy an_io_error_names_the_path`
Expected: FAIL, `Error` not defined.

- [ ] **Step 4: Write `src/error.rs`**

Manual `Display` via `f.write_str`/`write!`, never `#[derive]`. Implement `core::error::Error` (not `std::error::Error`) with `source()` returning the wrapped error for `Connect`, `Request` and `Io`. Copy the doc-comment density of `shep-log-rotate/src/error.rs`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p shep-deploy`
Expected: 2 passed.

- [ ] **Step 6: Mutation-check both tests**

Remove the path from `Io`'s `Display` arm, confirm `an_io_error_names_the_path` goes red, restore. Same for `stderr` and the git test. A guard test that passes with its guard removed is worthless.

- [ ] **Step 7: Write `.github/workflows/test.yml`**

Mirror `shep-log-rotate`'s jobs exactly: `lint`, `docs`, `test` (ubuntu + macos), `msrv` pinned to 1.88, and `integration`. The integration job installs shep with `cargo install shep --locked` (no `--version` flag; shep is on a normal release now) and runs `cargo test --features integration --locked`.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/ .github/ README.md
git commit -F- <<'EOF'
feat: crate skeleton, error type and CI

Mirrors shep-log-rotate's shape deliberately: one small error enum with a
manual Display per IR-19, core::error::Error rather than std, and the same
five CI jobs including an integration tier that installs shep and runs
against a real shepherd.

Both error tests mutation-checked by removing the field from the Display arm
and confirming they go red.
EOF
```

---

## Task 2: The `Daemon` trait, `Live`, and self-identification

**Files:**
- Create: `src/daemon.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `Error` from Task 1.
- Produces:
  ```rust
  pub trait Daemon {
      async fn dog_config(&self, name: &str) -> Result<String, Error>;
      async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error>;
      async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error>;
      async fn start(&self, apps: Vec<AppConfig>) -> Result<(), Error>;
      async fn reload(&self, sheep: &str) -> Result<(), Error>;
      async fn restart(&self, sheep: &str) -> Result<(), Error>;
  }
  pub struct Live(Client);
  impl Live { pub fn new(client: Client) -> Self }
  pub async fn adopted_name<D: Daemon>(daemon: &D) -> Option<String>;
  ```

- [ ] **Step 1: Write the failing test for self-identification**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct Flock(Vec<ProcessInfo>);
    impl Daemon for Flock {
        async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> { Ok(self.0.clone()) }
        // remaining methods: unimplemented!() — this double only answers list_flock
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
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p shep-deploy the_dog_finds_its_own_name_by_pid`
Expected: FAIL, `adopted_name` not defined.

- [ ] **Step 3: Implement**

```rust
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
```

The `info.dog.is_some()` check is what the second test guards: a sheep that happens to report this pid is not this dog.

- [ ] **Step 4: Run the tests**

Expected: both pass.

- [ ] **Step 5: Mutation-check**

Drop `info.dog.is_some()` and confirm `a_sheep_sharing_the_pid_is_not_the_dog` goes red. Restore.

- [ ] **Step 6: Implement `Live` over `Client`**

Each trait method maps to one request and converts the response, returning `Error::Protocol` with a description when the shepherd answers with something else. Do not add trait methods speculatively; add them in the task that needs them.

- [ ] **Step 7: Commit**

```bash
git add src/daemon.rs src/main.rs
git commit -F- <<'EOF'
feat: the Daemon trait, Live, and finding our own adopted name

Mirrors shep-log-rotate's structure: one trait for everything that touches
the shepherd, a Live impl over Client, and test doubles for the rest. The
whole engine stays unit-testable with no daemon running.

adopted_name is the documented workaround for a dog being unable to learn
the name it was adopted under: it is spawned with no argv and one env entry,
so it finds its own pid in list_flock. The dog.is_some() filter is
load-bearing and mutation-checked; without it a sheep reporting the same pid
would be mistaken for this dog.
EOF
```

---

## Task 3: The layout, and `deploy.toml`

**Files:**
- Create: `src/paths.rs`, `src/state.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Tree { root: PathBuf }
  impl Tree {
      pub fn for_sheep(shep_home: &Path, sheep: &str) -> Self;
      pub fn git(&self) -> PathBuf;          // <root>/git
      pub fn releases(&self) -> PathBuf;     // <root>/releases
      pub fn release(&self, sha: &str) -> PathBuf;
      pub fn current(&self) -> PathBuf;      // <root>/current
      pub fn state_file(&self) -> PathBuf;   // <root>/deploy.toml
  }

  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  pub struct State {
      pub remote: String,
      pub branch: String,
      pub deployed: Option<String>,          // sha
      pub verify: Verify,
      pub origin_cwd: Option<PathBuf>,       // pre-adoption, for restore
      pub origin_script: Option<String>,     // pre-adoption, for restore
      pub checkout: PathBuf,                 // the user's own checkout
  }

  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "lowercase")]
  pub enum Verify { Probed, Alive }
  ```
  `Verify::default()` is `Probed`.

- [ ] **Step 1: Write the failing round-trip test**

```rust
/// fails if state does not survive a write-then-read. This file is the only
/// record of how a sheep ran BEFORE the dog took over, so losing a field
/// means removal cannot restore the sheep and the operator is left with an
/// app running from a path under $SHEP_HOME they have no reason to know about.
#[test]
fn state_round_trips_through_toml() {
    let original = State {
        remote: "https://github.com/WatWowMap/ReactMap".into(),
        branch: "main".into(),
        deployed: Some("a1b2c3d".into()),
        verify: Verify::Probed,
        origin_cwd: Some(PathBuf::from("/srv/reactmap")),
        origin_script: Some("bun .".into()),
        checkout: PathBuf::from("/srv/reactmap"),
    };
    let text = toml::to_string(&original).expect("serialises");
    let back: State = toml::from_str(&text).expect("parses");
    assert_eq!(back, original);
}

/// fails if `verify` stops defaulting to the safe value. An absent verify key
/// must mean "wait for the readiness probe", never "deploy without checking".
#[test]
fn an_absent_verify_defaults_to_probed() {
    let text = r#"
        remote = "https://example.com/x"
        branch = "main"
        checkout = "/srv/x"
    "#;
    let state: State = toml::from_str(text).expect("parses");
    assert_eq!(state.verify, Verify::Probed);
}
```

- [ ] **Step 2: Run and watch both fail**

Run: `cargo test -p shep-deploy state_round_trips`
Expected: FAIL, `State` not defined.

- [ ] **Step 3: Implement `Tree` and `State`**

`Tree` is the one place that knows the layout; nothing else builds these paths by string concatenation. `State` uses `#[serde(default)]` on `verify` so the default applies.

- [ ] **Step 4: Run the tests**

Expected: both pass.

- [ ] **Step 5: Mutation-check the default**

Change `Verify`'s default to `Alive`, confirm `an_absent_verify_defaults_to_probed` goes red, restore.

- [ ] **Step 6: Add an atomic write**

`State::write` writes to `deploy.toml.tmp` in the same directory and `rename(2)`s it into place, so an interrupted write cannot truncate the only record of how to restore the sheep. Test it by writing twice and confirming no `.tmp` survives.

- [ ] **Step 7: Commit**

```bash
git add src/paths.rs src/state.rs
git commit -F- <<'EOF'
feat: the deploy tree layout and deploy.toml

Tree is the single place that knows the $SHEP_HOME/deploy/<sheep>/ layout, so
no other module builds those paths by concatenation.

State lives in the tree rather than in [dog.<name>] deliberately. Keying it to
the dog's name means renaming or re-adopting the dog destroys the record of
every deployment it manages, and those are unrelated things. In the tree it
survives rehoming and makes each tree self-describing.

origin_cwd and origin_script are the pre-adoption values, and they are why
removal can put a sheep back where its operator will look for it.

verify defaults to Probed, mutation-checked, because an absent key must never
silently mean "deploy without checking". Writes go through a temp file and
rename so an interruption cannot truncate the only record of how to restore.
EOF
```

---

## Task 4: Shared-file enumeration

**Files:**
- Create: `src/shared.rs`

**Interfaces:**
- Produces:
  ```rust
  pub fn ignored_present(checkout: &Path) -> Result<Vec<PathBuf>, Error>;
  pub fn shepignore_patterns(checkout: &Path) -> Result<Vec<String>, Error>;
  pub fn to_link(checkout: &Path) -> Result<Vec<PathBuf>, Error>;
  pub fn link_into(release: &Path, checkout: &Path, paths: &[PathBuf]) -> Result<(), Error>;
  ```

- [ ] **Step 1: Write the failing tests**

```rust
/// fails if enumeration stops using git's own answer. Parsing .gitignore by
/// hand gets negations (!server/src/configs/.gitkeep), anchored globs
/// (/docker-compose.yml) and nested ignore files wrong; `git status --ignored`
/// gets all three right because it is git deciding.
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

/// fails if a .shepignore entry is still linked. This is the whole reason
/// .shepignore exists: symlinking a build output means release B's build
/// writes through the link and replaces what release A is currently serving,
/// which kills blue/green and rollback in one line.
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
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p shep-deploy shepignored_paths_are_not_linked`
Expected: FAIL, `to_link` not defined.

- [ ] **Step 3: Implement**

`ignored_present` runs `git status --ignored --porcelain` in the checkout and keeps lines beginning `!! `. `shepignore_patterns` reads `.shepignore` if present, ignoring blank lines and `#` comments. `to_link` subtracts the second from the first. `link_into` creates parent directories in the release then `symlink`s each path back to the checkout.

- [ ] **Step 4: Run the tests**

Expected: both pass.

- [ ] **Step 5: Mutation-check**

Make `to_link` skip the subtraction and confirm `shepignored_paths_are_not_linked` goes red. Restore.

- [ ] **Step 6: Add the absent-`.shepignore` test**

```rust
/// fails if a repo with no .shepignore stops sharing everything ignored.
/// That is the zero-configuration case and the common one.
#[test]
fn no_shepignore_means_share_everything_ignored() {
    let repo = fixture_repo(&[(".gitignore", "config/\n"), ("config/local.json", "{}")]);
    assert!(!to_link(repo.path()).expect("computes").is_empty());
}
```

- [ ] **Step 7: Commit**

```bash
git add src/shared.rs
git commit -F- <<'EOF'
feat: derive shared files from git, minus .shepignore

Enumeration asks git via `git status --ignored --porcelain` rather than
parsing .gitignore. Parsing gets negations, anchored globs and nested ignore
files wrong; git gets them right because it is git deciding.

.shepignore subtracts from that set, and the test that it does is the most
important one here. .gitignore conflates config, caches and build outputs
because git ignores all three for the same reason. Symlinking a build output
means release B's build writes through the link and replaces what release A
is currently serving, which kills blue/green and rollback together.

Absent .shepignore shares everything ignored, which is the zero-configuration
case and the common one.
EOF
```

---

## Task 5: Git operations

**Files:**
- Create: `src/git.rs`

**Interfaces:**
- Produces:
  ```rust
  pub fn remote_url(checkout: &Path) -> Result<String, Error>;
  pub fn current_branch(checkout: &Path) -> Result<String, Error>;
  pub fn fetch(git_dir: &Path, remote: &str) -> Result<(), Error>;
  pub fn remote_head(git_dir: &Path, branch: &str) -> Result<String, Error>;
  pub fn worktree_add(git_dir: &Path, at: &Path, sha: &str) -> Result<(), Error>;
  pub fn worktree_remove(git_dir: &Path, at: &Path) -> Result<(), Error>;
  pub fn worktree_prune(git_dir: &Path) -> Result<(), Error>;
  ```

- [ ] **Step 1: Write the failing tests**

```rust
/// fails if a detached HEAD is treated as a branch. There is no branch to
/// track, so the dog must refuse with a message naming the problem rather
/// than deploy something arbitrary.
#[test]
fn a_detached_head_is_refused_by_name() {
    let repo = fixture_repo_with_commits(2);
    detach_head(&repo);
    let err = current_branch(repo.path()).expect_err("refuses");
    assert!(err.to_string().to_lowercase().contains("detached"));
}

/// fails if worktree removal stops forcing. A built worktree is ALWAYS dirty,
/// so plain `git worktree remove` always refuses and retention would silently
/// never reclaim anything.
#[test]
fn worktree_removal_forces_because_built_trees_are_dirty() {
    let repo = fixture_repo_with_commits(1);
    let at = repo.path().join("../rel-abc");
    worktree_add(repo.path(), &at, "HEAD").expect("adds");
    std::fs::write(at.join("build-output.txt"), "x").expect("writes");
    worktree_remove(repo.path(), &at).expect("removes a dirty tree");
    assert!(!at.exists());
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p shep-deploy worktree_removal_forces`
Expected: FAIL, `worktree_add` not defined.

- [ ] **Step 3: Implement**

Every function shells out to `git` and maps a non-zero exit to `Error::Git` carrying the command, the status and stderr. `current_branch` uses `git symbolic-ref --short HEAD` so a detached HEAD fails rather than returning `HEAD`. `worktree_remove` passes `--force`.

- [ ] **Step 4: Run the tests**

Expected: both pass.

- [ ] **Step 5: Mutation-check**

Remove `--force` from `worktree_remove` and confirm the dirty-tree test goes red. Restore.

- [ ] **Step 6: Commit**

```bash
git add src/git.rs
git commit -F- <<'EOF'
feat: fetch, branch resolution and worktree lifecycle

Shells out to git rather than linking a git library. The operations needed
here are few and shelling out means inheriting the user's own auth exactly:
if git can reach a private repo, so does this, and there is no credential
handling of our own to get wrong.

Two behaviours are mutation-checked because both fail silently otherwise.
worktree_remove passes --force, since a built tree is always dirty and plain
remove always refuses, which would make retention quietly reclaim nothing.
current_branch uses symbolic-ref so a detached HEAD is refused by name rather
than returning the string HEAD and deploying something arbitrary.
EOF
```

---

## Task 6: Flockfile parse and override merge

**Files:**
- Create: `src/flockfile.rs`

**Interfaces:**
- Produces: `pub fn app_config(release: &Path, sheep: &str) -> Result<AppConfig, Error>` — reads `Flockfile.toml` from the release, deep merges `Flockfile.override.toml` over it when present, selects the app named `sheep`, and refuses if the committed file sets `user` or `group`.

- [ ] **Step 1: Write the failing tests**

```rust
/// fails if the override stops winning. The override is the user's file and
/// the committed one is upstream's; a user who pins script must not have it
/// changed underneath them by a pull.
#[test]
fn the_override_wins_on_merge() {
    let rel = fixture_release(&[
        ("Flockfile.toml", "[[app]]\nname='web'\nscript='upstream.js'\n"),
        ("Flockfile.override.toml", "[[app]]\nname='web'\nscript='mine.js'\n"),
    ]);
    let app = app_config(rel.path(), "web").expect("merges");
    assert_eq!(app.script, "mine.js");
}

/// fails if a committed Flockfile can set the user a process runs as.
/// Privilege is not a recommendation, and it is the one thing a compromised
/// build genuinely cannot escalate on its own. This refusal is the boundary.
#[test]
fn a_committed_flockfile_cannot_set_user() {
    let rel = fixture_release(&[
        ("Flockfile.toml", "[[app]]\nname='web'\nscript='x.js'\nuser='root'\n"),
    ]);
    let err = app_config(rel.path(), "web").expect_err("refuses");
    assert!(err.to_string().contains("user"));
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p shep-deploy a_committed_flockfile_cannot_set_user`
Expected: FAIL, `app_config` not defined.

- [ ] **Step 3: Implement**

Parse both files as `toml::Value`, deep merge (override wins per key, arrays of apps matched by `name`), then deserialize the selected app into `shep_core`'s `AppConfig`. Check for `user`/`group` in the committed document *before* merging and refuse there, so the override cannot launder them either.

- [ ] **Step 4: Run the tests**

Expected: both pass.

- [ ] **Step 5: Mutation-check**

Remove the `user`/`group` refusal and confirm that test goes red. Reverse the merge precedence and confirm the other goes red. Restore both.

- [ ] **Step 6: Commit**

```bash
git add src/flockfile.rs
git commit -F- <<'EOF'
feat: parse the repo Flockfile and merge the user's override over it

Deep merge with the override winning is the whole pinning mechanism. The
committed file is upstream's and the override is the user's, so a user who
pins script and cwd is protected from those changing under a pull.

Worth being honest in the code as well as the spec: pinning is defence in
depth and not a boundary. A compromised upstream already runs arbitrary code
in bun install's postinstall or in make build and does not need the Flockfile
to do it.

The one real boundary is here and it is mutation-checked: a committed
Flockfile cannot set user or group at all. Privilege is not a recommendation,
and it is the single thing a build cannot escalate on its own. The check runs
before the merge so an override cannot launder the value either.
EOF
```

---

## Task 7: Build execution

**Files:**
- Create: `src/build.rs`

**Interfaces:**
- Produces: `pub async fn run(release: &Path, spec: &BuildSpec, as_user: Option<&str>) -> Result<(), Error>` where `BuildSpec { command: Option<String>, env: BTreeMap<String, String>, artifacts: Vec<PathBuf> }`.

- [ ] **Step 1: Write the failing tests**

```rust
/// fails if a failing build is treated as success. This is the guard that
/// keeps a broken build from ever reaching the swap: current never moves and
/// the running app is untouched, because it lives in a different directory.
#[tokio::test]
async fn a_failing_build_is_an_error() {
    let rel = fixture_release(&[]);
    let spec = BuildSpec { command: Some("exit 3".into()), ..Default::default() };
    assert!(run(rel.path(), &spec, None).await.is_err());
}

/// fails if an absent build command is an error rather than a no-op.
/// ReactMap run as `bun .` compiles its client at startup and declares no
/// build at all; the readiness probe covers it.
#[tokio::test]
async fn an_absent_build_command_is_not_an_error() {
    let rel = fixture_release(&[]);
    let spec = BuildSpec::default();
    assert!(run(rel.path(), &spec, None).await.is_ok());
}

/// fails if declared artifacts are not copied into the release. With
/// CARGO_TARGET_DIR pointed at a shared cache, the binary lands outside the
/// release, and `script = ./target/release/koji` would resolve to nothing.
#[tokio::test]
async fn declared_artifacts_are_copied_into_the_release() {
    let rel = fixture_release(&[]);
    let cache = tempdir();
    std::fs::create_dir_all(cache.path().join("release")).unwrap();
    std::fs::write(cache.path().join("release/koji"), b"binary").unwrap();
    let spec = BuildSpec {
        command: Some("true".into()),
        env: [("CARGO_TARGET_DIR".into(), cache.path().display().to_string())].into(),
        artifacts: vec![PathBuf::from("target/release/koji")],
    };
    run(rel.path(), &spec, None).await.expect("builds");
    assert!(rel.path().join("target/release/koji").exists());
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p shep-deploy a_failing_build_is_an_error`
Expected: FAIL, `run` not defined.

- [ ] **Step 3: Implement**

Spawn the command with the release as cwd and `spec.env` applied. When `as_user` is `Some`, drop to that user for the child. A non-zero exit is `Error::Git`-shaped but its own variant: add `Error::Build { status: Option<i32> }`. After a successful build, copy each declared artifact from `CARGO_TARGET_DIR` (or wherever the env points) into its path inside the release.

- [ ] **Step 4: Run the tests**

Expected: all three pass.

- [ ] **Step 5: Mutation-check**

Make `run` ignore the exit status and confirm the failing-build test goes red. Restore.

- [ ] **Step 6: Commit**

```bash
git add src/build.rs
git commit -F- <<'EOF'
feat: run the build, in the release, as the sheep's user

The build is the dangerous step: it executes code from a repository, and
postinstall scripts are the most common supply-chain vector in the Node
ecosystem. So it runs as the target sheep's user and never as the shepherd's.
A compromised ReactMap build gets ReactMap's privileges and nothing more.

A failing build is an error, mutation-checked, because that is what keeps a
broken build from reaching the swap. current never moves and the running app
is untouched, since it lives in a different directory entirely. This is the
part that replaces a hardcoded sleep between a build and a restart.

An absent build command is a no-op rather than an error: ReactMap run as
`bun .` compiles its client at startup and declares no build, and the
readiness probe already covers that.

Artifacts are copied because a shared CARGO_TARGET_DIR keeps Rust compilation
warm across releases but lands the binary outside the release, where
`script = ./target/release/koji` would resolve to nothing.
EOF
```

---

## Task 8: The swap

**Files:**
- Create: `src/swap.rs`

**Interfaces:**
- Produces:
  ```rust
  pub fn point_at(current: &Path, release: &Path) -> Result<(), Error>;
  pub fn resolve(current: &Path) -> Result<Option<PathBuf>, Error>;
  ```

- [ ] **Step 1: Write the failing test**

```rust
/// fails if the swap is not atomic. Removing the old symlink and creating a
/// new one leaves a window where `current` points at nothing, and a sheep
/// restarting in that window fails to start for a reason nobody will
/// reproduce. The fix is rename(2) over a temporary link.
#[test]
fn the_swap_never_leaves_current_dangling() {
    let root = tempdir();
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let current = root.path().join("current");

    point_at(&current, &a).expect("first");
    assert_eq!(resolve(&current).unwrap().as_deref(), Some(a.as_path()));

    point_at(&current, &b).expect("swap over an existing link");
    assert_eq!(resolve(&current).unwrap().as_deref(), Some(b.as_path()));
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p shep-deploy the_swap_never_leaves_current_dangling`
Expected: FAIL, `point_at` not defined.

- [ ] **Step 3: Implement**

Create the new symlink at `current.tmp`, then `std::fs::rename` it onto `current`. `rename(2)` over an existing symlink is atomic and replaces it in one step. Never `remove_file` then `symlink`.

- [ ] **Step 4: Run the test**

Expected: pass.

- [ ] **Step 5: Mutation-check**

Replace the implementation with remove-then-create. The test still passes, which proves it does not actually test atomicity. **This is the point:** add an assertion that no `current.tmp` survives, and document in the test that atomicity itself is not directly observable from a single-threaded test, so the guard is the implementation shape plus review. Do not claim a test proves something it cannot.

- [ ] **Step 6: Commit**

```bash
git add src/swap.rs
git commit -F- <<'EOF'
feat: swap current with rename(2), never remove-then-create

rename(2) over an existing symlink replaces it in one step. Removing the old
link and creating a new one leaves a window where current points at nothing,
and a sheep restarting inside that window fails to start for a reason nobody
will reproduce.

Honest note about the test: a single-threaded test cannot observe atomicity,
so it pins the observable parts, that the swap works over an existing link
and leaves no temp file behind, and the atomicity itself rests on the
implementation shape and on review. Recorded rather than claimed, because a
test named for a property it does not check is worse than no test.
EOF
```

---

## Task 9: Verification

**Files:**
- Create: `src/verify.rs`

**Interfaces:**
- Produces: `pub async fn wait<D: Daemon>(daemon: &D, sheep: &str, mode: Verify, timeout: Duration) -> Result<bool, Error>`

- [ ] **Step 1: Write the failing tests**

```rust
/// fails if Starting is mistaken for success. ProcStatus::Starting means
/// "spawned, not yet ready" and Online means "running and (if configured)
/// ready", so Online is the ONLY status that means the probe passed.
#[tokio::test]
async fn starting_is_not_success() {
    let daemon = Statuses(vec![ProcStatus::Starting, ProcStatus::Starting]);
    let ok = wait(&daemon, "web", Verify::Probed, Duration::from_millis(50)).await.unwrap();
    assert!(!ok);
}

/// fails if reaching Online is not treated as success.
#[tokio::test]
async fn reaching_online_is_success() {
    let daemon = Statuses(vec![ProcStatus::Starting, ProcStatus::Online]);
    assert!(wait(&daemon, "web", Verify::Probed, Duration::from_secs(5)).await.unwrap());
}

/// fails if Verify::Alive starts demanding a probe. Alive is the deliberate,
/// visible downgrade for a sheep with no probe: still running after the
/// window is enough.
#[tokio::test]
async fn alive_accepts_a_still_running_process() {
    let daemon = Statuses(vec![ProcStatus::Starting, ProcStatus::Starting]);
    assert!(wait(&daemon, "web", Verify::Alive, Duration::from_millis(50)).await.unwrap());
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p shep-deploy starting_is_not_success`
Expected: FAIL, `wait` not defined.

- [ ] **Step 3: Implement**

Poll `describe` until the timeout. `Probed` succeeds only on `Online`. `Alive` succeeds if the process is still present at the end of the window and has not exited.

- [ ] **Step 4: Run the tests**

Expected: all three pass.

- [ ] **Step 5: Mutation-check**

Make `Probed` accept `Starting` and confirm `starting_is_not_success` goes red. Restore.

- [ ] **Step 6: Commit**

```bash
git add src/verify.rs
git commit -F- <<'EOF'
feat: verify a deploy by watching for Online

This is the thing that makes the dog more than a shell script, and it needs
no new machinery: shep already reports ProcStatus::Starting as "spawned, not
yet ready" and Online as "running and (if configured) ready". Watching for
Online IS reading the readiness probe's verdict.

Starting must never count as success, and that is mutation-checked, because
accepting it would mean declaring a deploy good the moment the process
existed, which is exactly the failure a hardcoded sleep already makes.

Verify::Alive is the deliberate downgrade for a sheep with no probe. Still
running after the window is a weak guarantee, and it is named honestly rather
than dressed up as a strong one.
EOF
```

---

## Task 10: The deploy orchestration and the operator command

**Files:**
- Create: `src/deploy.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: `pub async fn deploy<D: Daemon>(daemon: &D, tree: &Tree, state: &mut State) -> Result<Outcome, Error>` where `Outcome` is `UpToDate`, `Deployed { sha: String }`, or `RolledBack { to: String, why: String }`.

- [ ] **Step 1: Write the failing test for the rollback path**

```rust
/// fails if a deploy that never comes up leaves the new release live. This is
/// the whole promise of the feature: verification without rollback is just a
/// slower way to be broken.
#[tokio::test]
async fn a_release_that_never_comes_up_is_rolled_back() {
    let (tree, mut state) = fixture_tree_with_previous_release("old-sha");
    let daemon = NeverReady;

    let outcome = deploy(&daemon, &tree, &mut state).await.expect("completes");

    assert!(matches!(outcome, Outcome::RolledBack { .. }));
    assert_eq!(swap::resolve(&tree.current()).unwrap(), Some(tree.release("old-sha")));
    assert_eq!(state.deployed.as_deref(), Some("old-sha"));
}

/// fails if an unchanged remote head still does the work. Polling must be
/// cheap; a deploy per tick would rebuild constantly.
#[tokio::test]
async fn an_unchanged_head_does_nothing() {
    let (tree, mut state) = fixture_tree_at_head("same-sha");
    let outcome = deploy(&Ready, &tree, &mut state).await.expect("completes");
    assert!(matches!(outcome, Outcome::UpToDate));
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p shep-deploy a_release_that_never_comes_up_is_rolled_back`
Expected: FAIL, `deploy` not defined.

- [ ] **Step 3: Implement the sequence**

In order, exactly as the spec's "The deploy sequence" section states: fetch, compare, worktree add, link shared, build, swap, `Reload`, verify, and on failure swap back and reload again. Update `state.deployed` only after a successful verify. Steps one to five must never touch the running app.

- [ ] **Step 4: Run the tests**

Expected: both pass.

- [ ] **Step 5: Mutation-check the rollback**

Remove the swap-back on verify failure and confirm the rollback test goes red. Restore.

- [ ] **Step 6: Add the operator command to `main.rs`**

```
shep-deploy deploy <sheep>
```

Supervised invocation still takes no argv, per the dog contract. This is the second invocation mode. **Depends on shep prerequisite 3** for `shep deploy <sheep>` to reach it, but the binary is directly runnable meanwhile.

- [ ] **Step 7: Run the full gate**

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

- [ ] **Step 8: Commit**

```bash
git add src/deploy.rs src/main.rs
git commit -F- <<'EOF'
feat: the deploy sequence, and shep-deploy deploy <sheep>

Fetch, compare, worktree, link, build, swap, reload, verify, and swap back on
failure. Steps one through five never touch the running app, which is the
property that makes a failed build harmless: it happens in a directory the
live release does not share.

Reload rather than Restart, because shep's reload already runs SpawnNew then
AwaitReady then DrainOld then ReapOld. The replacement reaches readiness
before the old instance drains, so the old release serves throughout and the
new one's startup cost is paid while the old one is still answering.

The rollback is mutation-checked. Verification without rollback is just a
slower way to be broken, so removing the swap-back has to turn a test red.

state.deployed advances only after a successful verify, so an interrupted
deploy leaves the record pointing at what is actually serving.
EOF
```

---

## Task 11: Integration against a real shepherd

**Files:**
- Create: `tests/integration.rs`

**Interfaces:**
- Consumes: the whole engine.

- [ ] **Step 1: Write the test**

Behind `#[cfg(feature = "integration")]`, and `$SHEP_HOME` must be a temporary directory in every test, exactly as `shep-log-rotate/tests/integration.rs` does it. That isolation is load-bearing: these tests start a real shepherd, and pointing one at a developer's own `~/.shep` would supervise their real services.

Cover: a real deploy of a fixture repo end to end, and a deploy whose build fails leaving the previous release serving.

- [ ] **Step 2: Run it**

```bash
cargo install shep --locked
SHEP_BIN="$(command -v shep)" cargo test --features integration --locked
```

- [ ] **Step 3: Commit**

```bash
git add tests/integration.rs
git commit -F- <<'EOF'
feat: the integration tier, against a real shepherd

Five unit-tested modules do not prove the dog can talk to a daemon. This tier
starts a real shepherd, deploys a fixture repo end to end, and checks that a
failing build leaves the previous release serving.

$SHEP_HOME is a temporary directory in every test, and that is load-bearing
rather than tidy: these start a real shepherd, and one pointed at a
developer's own ~/.shep would supervise their real services.
EOF
```

---

## What this plan does NOT cover

Deliberately deferred to a second plan, because the engine above is working, testable software without them:

- The poll loop and `[dog.<name>]` config (interval, retention)
- Flock survey and the ready/eligible/not-eligible report
- Publishing smits (**depends on shep prerequisite for smits**)
- Bootstrap and first opt-in, including the cutover
- On-remove restore (**depends on shep prerequisites 4 and 5**)
- Retention pruning of old worktrees

## Prerequisites in shep, tracked elsewhere

1. `shep adopt` path resolution (name on PATH, `~/`) — in flight
2. `shep adopt --name` flag with `shep-` stripping — in flight
3. `shep <dogname> [args]` passthrough — in flight
4. `shep rehome` must stop deleting `[dog.<name>]` — unassigned
5. Dog on-remove lifecycle hook — unassigned

Plus the documentation block in the spec's "Documentation shep owes".

## Self-review

**Spec coverage.** Tasks 1-11 cover the layout, `deploy.toml`, shared-file derivation, `.shepignore`, git and worktrees, Flockfile merging with the `user`/`group` refusal, the build with env and artifacts, the atomic swap, probe-based verification with both modes, the deploy sequence, and rollback. Uncovered spec sections are listed above and belong to plan two, not forgotten.

**Placeholder scan.** No TBD, TODO, or "add error handling". Every code step carries real code and every test carries a stated failure mode.

**Type consistency.** `Verify` is defined once in Task 3 and used in Tasks 9 and 10. `Daemon` is defined in Task 2 and consumed in 9 and 10. `Tree` is defined in Task 3 and used in 10 and 11. `BuildSpec` is defined in Task 7 and used in 10. `Error` gains variants in Tasks 1, 5 and 7 and is never redefined.
