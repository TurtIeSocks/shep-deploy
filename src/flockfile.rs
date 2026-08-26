//! Reading the release's own Flockfile, merging the operator's override over
//! it, and producing the [`AppConfig`] for one sheep.
//!
//! Two files, and it matters which of them is trusted:
//!
//! - `Flockfile.toml`, committed in the release. Upstream owns it - it is
//!   whatever the tracked branch happened to contain at this release's sha.
//! - `Flockfile.override.toml`, gitignored, never committed. The operator
//!   owns it, and it deep merges over the committed file with the override
//!   winning on every key it names. A sheep with no override simply follows
//!   upstream on every deploy; a sheep whose operator pins `script` or `cwd`
//!   in an override keeps that value even when a pull changes upstream's
//!   own copy.
//!
//! ## The one real boundary here
//!
//! Everything above is defence in depth, not a boundary: a compromised
//! upstream already runs arbitrary code the moment this dog runs `bun
//! install`'s postinstall or `make build`, and pinning `script` in an
//! override does not stop that - it only stops a *later* pull from moving
//! the pointer. `user` and `group` are different in kind, not degree. A
//! build cannot escalate to another unix user on its own; the daemon
//! choosing which user a sheep's process runs as is the one privilege a
//! Flockfile can grant that the build itself never could. So a *committed*
//! Flockfile is refused outright the moment it sets either field on any app
//! it declares - see [`refuse_repo_privilege`] - and the check runs against
//! the committed document alone, before the override is even read, so an
//! override cannot make the refusal disappear by also touching the same
//! key. The override itself is under no such restriction: it is the
//! operator's own file, and pinning `user`/`group` there is exactly the
//! "pin it so upstream can never change it" mechanism every other field
//! already gets.

use std::fs;
use std::io;
use std::path::Path;

use shep_client::shep_core::prelude::AppConfig;
use toml::{Table, Value};

use crate::error::Error;

/// The [`AppConfig`] for `sheep`, built from `release`'s own Flockfile.
///
/// Reads `Flockfile.toml` (required) and `Flockfile.override.toml`
/// (optional - a sheep with none simply has nothing to merge), refuses if
/// the committed file sets `user` or `group` on any app, deep merges the
/// override over the committed document with arrays of apps matched by
/// `name` rather than position, then selects and deserialises the entry
/// named `sheep`.
///
/// # Errors
/// [`Error::Io`], naming the failing path, if `Flockfile.toml` cannot be
/// read or `Flockfile.override.toml` exists but cannot be read.
/// [`Error::Config`] if either file is not valid TOML, if the committed
/// file sets `user` or `group` on any app, if no app named `sheep` exists
/// after merging, or if that app's table does not match [`AppConfig`]'s
/// schema.
pub fn app_config(release: &Path, sheep: &str) -> Result<AppConfig, Error> {
    let committed = read_required(&release.join("Flockfile.toml"))?;
    refuse_repo_privilege(&committed)?;

    let override_doc = read_optional(&release.join("Flockfile.override.toml"))?;
    let merged = deep_merge(committed, override_doc);

    let app = select_app(&merged, sheep)?;
    app.try_into().map_err(|source: toml::de::Error| {
        Error::Config(format!(
            "app {sheep:?} does not match shep's app schema: {source}"
        ))
    })
}

/// Reads and parses a Flockfile that must exist.
///
/// # Errors
/// [`Error::Io`] if `path` cannot be read at all. [`Error::Config`] if its
/// contents are not valid TOML.
fn read_required(path: &Path) -> Result<Value, Error> {
    let text = fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    parse(path, &text)
}

/// As [`read_required`], but a missing file is not an error - it is treated
/// as an empty document, the same "absent means zero configuration"
/// precedent [`crate::shared::shepignore_patterns`] already sets for
/// `.shepignore`. `Flockfile.override.toml` is optional by design.
///
/// # Errors
/// [`Error::Io`] if `path` exists but cannot be read. [`Error::Config`] if
/// its contents are not valid TOML.
fn read_optional(path: &Path) -> Result<Value, Error> {
    match fs::read_to_string(path) {
        Ok(text) => parse(path, &text),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(Value::Table(Table::new())),
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Parses `text` as a generic TOML document, naming `path` in the error if
/// it does not parse.
fn parse(path: &Path, text: &str) -> Result<Value, Error> {
    toml::from_str(text).map_err(|source| Error::Config(format!("{}: {source}", path.display())))
}

/// The one real boundary this module enforces: refuses if any app declared
/// in `committed` sets `user` or `group` at all, regardless of what value it
/// names.
///
/// Takes the *committed* document specifically, never the merged one. If
/// this ran against the merged document instead, a legitimate use of the
/// mechanism - an operator pinning `user` in their own override - would
/// look identical to the thing being refused, since the merged document
/// would carry the key either way. Checking the committed document alone,
/// before the override is even read, keeps the two apart: the committed
/// file can never carry the key, full stop, and the override remains free
/// to set it precisely because it is the operator's own file and never came
/// from the repo.
///
/// # Errors
/// [`Error::Config`] naming the field and the app, if any app in `committed`
/// sets `user` or `group`.
fn refuse_repo_privilege(committed: &Value) -> Result<(), Error> {
    for app in apps(committed) {
        let Some(table) = app.as_table() else {
            continue;
        };
        for field in ["user", "group"] {
            if table.contains_key(field) {
                let name = table
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("<unnamed>");
                return Err(Error::Config(format!(
                    "Flockfile.toml sets `{field}` on app {name:?} - a committed Flockfile can \
                     never set user or group, on any app, because privilege is the one thing a \
                     compromised build cannot escalate to on its own. Pin `{field}` in \
                     Flockfile.override.toml instead - it is gitignored and never comes from the \
                     repo."
                )));
            }
        }
    }
    Ok(())
}

/// `doc`'s `[[app]]` array, or an empty slice if `doc` is not a table or has
/// no `app` key at all - an empty Flockfile has no apps to iterate rather
/// than being a shape error at this layer.
fn apps(doc: &Value) -> &[Value] {
    doc.as_table()
        .and_then(|table| table.get("app"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Deep merges `over` onto `base`, `over` winning on every key it names.
///
/// Two tables merge key by key, recursing into any key both sides declare
/// as a table so a partial override only replaces the fields it actually
/// names - an override setting only `script` leaves every other field of
/// that app, and every other app, untouched. The `app` key is special-cased
/// to [`merge_apps`] rather than falling into that same table-of-tables
/// recursion, because it is not a table keyed by name - it is an array, and
/// the two Flockfiles are under no obligation to list their apps in the
/// same order. Anything else - two scalars, or a table on one side and a
/// scalar on the other - has `over` win outright, which is the base case
/// every recursive branch above eventually bottoms out at.
fn deep_merge(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Table(mut base), Value::Table(over)) => {
            for (key, over_value) in over {
                let merged = if key == "app" {
                    merge_apps(
                        base.remove(&key).unwrap_or(Value::Array(Vec::new())),
                        over_value,
                    )
                } else {
                    match base.remove(&key) {
                        Some(base_value) => deep_merge(base_value, over_value),
                        None => over_value,
                    }
                };
                base.insert(key, merged);
            }
            Value::Table(base)
        }
        (_, over) => over,
    }
}

/// Merges two `[[app]]` arrays by matching each entry's `name`, not its
/// position.
///
/// An override entry whose `name` matches a committed entry deep-merges
/// onto it in place, at the committed entry's own position, so a partial
/// override still leaves every field it does not name untouched - the same
/// property [`deep_merge`] gives every other table in the document. An
/// override entry whose `name` matches nothing in `base` is appended as a
/// new app. Position carries no meaning: the committed file and the
/// override are free to list the same apps in different orders and still
/// merge onto the same entries.
fn merge_apps(base: Value, over: Value) -> Value {
    let mut base_apps = match base {
        Value::Array(apps) => apps,
        _ => Vec::new(),
    };
    let over_apps = match over {
        Value::Array(apps) => apps,
        _ => Vec::new(),
    };

    for over_app in over_apps {
        let name = app_name(&over_app).map(str::to_owned);
        let existing = name
            .as_deref()
            .and_then(|name| base_apps.iter().position(|app| app_name(app) == Some(name)));

        match existing {
            Some(index) => {
                let base_app = base_apps.remove(index);
                base_apps.insert(index, deep_merge(base_app, over_app));
            }
            None => base_apps.push(over_app),
        }
    }

    Value::Array(base_apps)
}

/// `app`'s `name` field, if it has one and it is a string.
fn app_name(app: &Value) -> Option<&str> {
    app.as_table()?.get("name")?.as_str()
}

/// The merged document's app entry named `sheep`.
///
/// # Errors
/// [`Error::Config`] if no app in `merged` is named `sheep`.
fn select_app(merged: &Value, sheep: &str) -> Result<Value, Error> {
    apps(merged)
        .iter()
        .find(|app| app_name(app) == Some(sheep))
        .cloned()
        .ok_or_else(|| Error::Config(format!("no app named {sheep:?} in Flockfile.toml")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Builds a release directory containing the given files - typically
    /// `Flockfile.toml` and/or `Flockfile.override.toml` - for one test.
    fn fixture_release(files: &[(&str, &str)]) -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, contents) in files {
            fs::write(dir.path().join(name), contents).expect("write fixture file");
        }
        dir
    }

    /// fails if the override stops winning. The override is the user's file
    /// and the committed one is upstream's; a user who pins script must not
    /// have it changed underneath them by a pull.
    #[test]
    fn the_override_wins_on_merge() {
        let rel = fixture_release(&[
            (
                "Flockfile.toml",
                "[[app]]\nname='web'\nscript='upstream.js'\n",
            ),
            (
                "Flockfile.override.toml",
                "[[app]]\nname='web'\nscript='mine.js'\n",
            ),
        ]);
        let app = app_config(rel.path(), "web").expect("merges");
        assert_eq!(app.script, "mine.js");
    }

    /// fails if a committed Flockfile can set the user a process runs as.
    /// Privilege is not a recommendation, and it is the one thing a
    /// compromised build genuinely cannot escalate on its own. This refusal
    /// is the boundary.
    #[test]
    fn a_committed_flockfile_cannot_set_user() {
        let rel = fixture_release(&[(
            "Flockfile.toml",
            "[[app]]\nname='web'\nscript='x.js'\nuser='root'\n",
        )]);
        let err = app_config(rel.path(), "web").expect_err("refuses");
        assert!(err.to_string().contains("user"));
    }

    /// fails if `group` is not checked as its own clause. `user` and `group`
    /// are two separate `contains_key` checks in `refuse_repo_privilege`,
    /// not one - the test above only proves the first field is guarded, and
    /// a version of this function that checked `user` and forgot `group`
    /// would still pass it.
    #[test]
    fn a_committed_flockfile_cannot_set_group() {
        let rel = fixture_release(&[(
            "Flockfile.toml",
            "[[app]]\nname='web'\nscript='x.js'\ngroup='wheel'\n",
        )]);
        let err = app_config(rel.path(), "web").expect_err("refuses");
        assert!(err.to_string().contains("group"));
    }

    /// fails if the presence of an override makes the committed-file
    /// refusal disappear. The override here overwrites `user` to a
    /// different value entirely - if the check ran against the *merged*
    /// document's value instead of the committed document itself, this is
    /// exactly the shape that would look laundered: the dangerous value
    /// `root` is gone from the merged result, replaced by `nobody`. The
    /// refusal must fire anyway, because it was decided by reading the
    /// committed file alone, before the override was ever opened.
    #[test]
    fn an_override_present_does_not_launder_a_committed_user_field() {
        let rel = fixture_release(&[
            (
                "Flockfile.toml",
                "[[app]]\nname='web'\nscript='x.js'\nuser='root'\n",
            ),
            (
                "Flockfile.override.toml",
                "[[app]]\nname='web'\nuser='nobody'\n",
            ),
        ]);
        let err = app_config(rel.path(), "web").expect_err("still refuses");
        assert!(err.to_string().contains("user"));
    }

    /// fails if apps stop merging by `name` and fall back to merging by
    /// position instead. The committed file and the override list the same
    /// two apps in opposite orders; a positional merge would splice `web`'s
    /// committed entry with `worker`'s override entry (and vice versa),
    /// producing a merged app with a mismatched name/script pairing instead
    /// of leaving each app's own fields untouched aside from what its own
    /// override actually names.
    #[test]
    fn apps_in_different_files_merge_by_name_not_position() {
        let rel = fixture_release(&[
            (
                "Flockfile.toml",
                "[[app]]\nname='web'\nscript='web.js'\n\n[[app]]\nname='worker'\nscript='worker.js'\n",
            ),
            (
                "Flockfile.override.toml",
                "[[app]]\nname='worker'\nscript='worker-mine.js'\n\n[[app]]\nname='web'\nscript='web-mine.js'\n",
            ),
        ]);
        let web = app_config(rel.path(), "web").expect("merges web");
        let worker = app_config(rel.path(), "worker").expect("merges worker");
        assert_eq!(web.script, "web-mine.js");
        assert_eq!(worker.script, "worker-mine.js");
    }

    /// fails if `merge_apps` stops appending an override app that names
    /// nothing in the committed file - the other half of "matched by name":
    /// a name match merges in place, and a name miss must still make the
    /// app available rather than being silently dropped.
    #[test]
    fn an_override_can_add_a_new_app_not_in_the_committed_file() {
        let rel = fixture_release(&[
            ("Flockfile.toml", "[[app]]\nname='web'\nscript='web.js'\n"),
            (
                "Flockfile.override.toml",
                "[[app]]\nname='sidecar'\nscript='sidecar.js'\n",
            ),
        ]);
        let sidecar = app_config(rel.path(), "sidecar").expect("the override's own app merges");
        assert_eq!(sidecar.script, "sidecar.js");
    }

    /// fails if asking for an app nobody declared silently produces
    /// something instead of a named refusal.
    #[test]
    fn app_config_refuses_an_unknown_sheep_name() {
        let rel = fixture_release(&[("Flockfile.toml", "[[app]]\nname='web'\nscript='x.js'\n")]);
        let err = app_config(rel.path(), "ghost").expect_err("no such app");
        assert!(err.to_string().contains("ghost"));
    }
}
