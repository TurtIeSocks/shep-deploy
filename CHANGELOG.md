# Changelog

All notable changes to this crate are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

