# Changelog

All notable changes to this crate are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.3] - 2026-09-04

### Changed

- One spelling for an Io error's map_err
- Seal the provenance list so only to_link can mint it

### Fixed

- Read ignored paths unquoted, honour .shepignore's gitignore spellings, kill groups with killpg
- Put `--` before a remote, and `--detach` on every worktree add
- Refuse a deploy.toml whose values cannot work, and write it durably
- An unreadable record is its own standing, never an invitation to setup
- Give `alive` the app's own budget to reach Online, count rows, cap every wait
- Floor the durations at a second and validate passthrough names
- Create the lock file owner-only
- Name an unknown response variant without printing its body
- Drop control characters from a smit before publishing it
- Report a target the dog cannot name, and name the markers directory
- Say something when nothing can be restored, and compare paths by what they resolve to
- Survive a panic in one target, notice the deploy directory going, mute a refused smit
- Close the repository's routes into the dog's own process, and the state gaps around a swap
- List an orphaned tree, count swallowed repeats, note two limits
- Round two of the review, on the changes round one made
- Rounds three to six of the review, each on the round before
- Run every git call and the artifact copy off the runtime's thread
- Refuse every committed field the shepherd acts on at its own uid
- Keep the pre-adoption app, watch a verified flock after its turnover, hold a race
- Keep the interpreter a sheep already runs under when its Flockfile names none
- Speak protocol 3, which the published shep now requires
- Read the dog's section from dogs.toml, where shep keeps it since 0.1.32
- Compare paths by what they resolve to, and the rest of the first review round
- Stage the record through shep-core's atomic file helper, and refuse a half-recorded origin
- Lead the build's process group with a holder, so a reaped pid is never signalled
- Hold a record lock across every read-modify-write of deploy.toml
- Keep a verify edited during the cutover, and lock the record's first write too
- Drain a scaled original's repair whole, however its rows arrive
- Wait for the repair's own rows, by the ids the shepherd answered with


## [0.2.2] - 2026-09-03

### Fixed

- Connect the supervised dog as a dog, so shep records its handshake


## [0.2.1] - 2026-08-31

### Fixed

- Refresh the lockfile, and follow shep-client's RpcError


## [0.2.0] - 2026-08-29

### Added

- One deploy at a time per sheep, with an advisory flock
- Move the build block to `[dog.deploy.build]` **(BREAKING)**

### Changed

- One fixtures module instead of six copies of the same helper
- Read the control socket from ShepPaths rather than re-joining it

### Fixed

- Refuse an artifact path that escapes the release
- Bound git fetch and stop the build inheriting the dog's environment
- Do not let one transient describe cost a live reload
- Refuse a typo in deploy.toml, and say what Error::Config covers
- Drain the pipes, or a chatty git looks exactly like a hung one
- Signal the process group, and never join past the budget
- Check where an artifact really lands, not how it is spelled
- A committed override could grant a unix user, and two smaller holes
- Retry the post-dwell describe too, not just the one in turnover
- Take provenance from the share list, and stop trusting a path twice
- Stop an artifact truncating the file it is copying
- A committed .shepignore can no longer drop the operator's override
- Re-resolve an artifact's destination immediately before opening it
- A half-checked-out release is redone rather than deployed
- A prepare that died before writing its record can be run again
- Only real releases take a keep slot, and one failure does not strand the rest
- A cutover that never landed is not "restored"
- A straggler cannot mark a sha another process already deployed
- A sheep name must be a name, and a verb without one is a usage error
- Bound the build command, so one hung build cannot stop the dog
- An artifact the build did not produce fails the deploy
- Ask the shepherd whether a cutover ran, not just the record
- The dog's completion marker is never shared in from a checkout
- Tell contention apart from a lock that failed for another reason
- Say so when the dog's marker displaces an operator's file
- A replacement shep left at `starting` has not come up
- Count the flock at the dwell, not just check who is in it
- Keep the artifact's permission bits, and prove the group kill
- Keep the completion marker where the repository cannot write it
- A flock that came back smaller has not turned over
- Kill the build's process group however the build ended
- Do not copy setuid, setgid or the sticky bit onto an artifact


## [0.1.1] - 2026-08-28

