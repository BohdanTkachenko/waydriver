//! Per-session GSettings isolation via GIO's keyfile backend.
//!
//! A session can run mutter and the application against a **private,
//! per-session GSettings store** instead of the host user's shared dconf
//! database. This is what lets the compositor enable mutter's
//! `scale-monitor-framebuffer` experimental feature (needed for fractional
//! HiDPI scales) without touching — or being affected by — the host's real
//! desktop settings, and lets a caller seed arbitrary settings the app reads
//! (e.g. `org.gnome.desktop.interface text-scaling-factor`).
//!
//! ## Why the keyfile backend and not dconf
//!
//! A D-Bus bus only routes messages; it does **not** scope where GSettings
//! persists. dconf stores everything in a per-user database keyed by the dconf
//! *profile* + `HOME`, so writing a key — even over a session's private bus —
//! lands in the shared `~/.config/dconf/user` and leaks across sessions and to
//! the host. GIO's keyfile backend, selected with `GSETTINGS_BACKEND=keyfile`,
//! sidesteps dconf entirely: each process reads a plain-text keyfile at
//! `$XDG_CONFIG_HOME/glib-2.0/settings/keyfile`. Point `XDG_CONFIG_HOME` at a
//! per-session directory and the store is fully isolated — no daemon, no
//! shared database, no host pollution. (`GSETTINGS_BACKEND=memory` does not
//! work here: the host dconf daemon ignores it and the value never reaches a
//! freshly spawned mutter.)
//!
//! ## Shared store
//!
//! Mutter and the app both use [`config_dir`] under the session's runtime dir,
//! so a single keyfile written by the compositor before mutter starts is read
//! by both. The compositor writes the **complete** file at launch (it runs
//! first and mutter needs the file in place); the app just inherits the same
//! `XDG_CONFIG_HOME`.
//!
//! ## Live updates
//!
//! [`live_write`] rewrites the keyfile **in place** after launch — the path
//! behind [`crate::Session::set_setting`]. GIO's keyfile backend runs a
//! `GFileMonitor` on the file, so an in-place rewrite (same inode, via
//! [`write_keyfile`]'s truncating `std::fs::write`) makes the already-running
//! app re-emit its GSettings `changed` signal and re-apply the new value live.
//! The rewrite preserves every existing entry — the compositor's
//! `experimental-features` seed and any launch-time seeds — by reading the
//! current file back through [`parse_keyfile`] first. It's best-effort w.r.t.
//! an app that also writes the same keyfile, but in practice apps only read
//! these keys.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Value of `GSETTINGS_BACKEND` that selects GIO's keyfile backend.
pub const KEYFILE_BACKEND: &str = "keyfile";

/// One GSettings entry to seed into a session's isolated keyfile store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GSettingEntry {
    /// Dotted schema id, e.g. `"org.gnome.desktop.interface"`.
    pub schema: String,
    /// Key within the schema, e.g. `"text-scaling-factor"`.
    pub key: String,
    /// The value in **GVariant text form**, written verbatim into the
    /// keyfile: numbers bare (`"1.5"`, `"2"`), strings single-quoted
    /// (`"'prefer-dark'"`), arrays bracketed
    /// (`"['scale-monitor-framebuffer']"`). This is the same syntax
    /// `gsettings set` accepts, so a caller can copy a known-good value
    /// straight in.
    pub value: String,
}

impl GSettingEntry {
    /// Convenience constructor taking string-likes for all three fields.
    pub fn new(
        schema: impl Into<String>,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            schema: schema.into(),
            key: key.into(),
            value: value.into(),
        }
    }
}

/// Per-session GSettings isolation configuration.
#[derive(Debug, Clone)]
pub struct GSettingsConfig {
    /// When `true` (the default), mutter and the app run against a private
    /// per-session keyfile store. When `false`, both inherit the host's
    /// normal GSettings/dconf — useful for debugging against a real desktop's
    /// live settings, at the cost of fractional-scale support unless the host
    /// has `scale-monitor-framebuffer` enabled itself.
    pub isolated: bool,
    /// Entries seeded into the keyfile when `isolated`. Ignored otherwise.
    /// Backends may prepend their own required entries (the mutter backend
    /// seeds `org.gnome.mutter experimental-features`); a later entry for the
    /// same schema+key overrides an earlier one, so caller-supplied values
    /// win over backend defaults.
    pub initial: Vec<GSettingEntry>,
}

impl Default for GSettingsConfig {
    fn default() -> Self {
        Self {
            isolated: true,
            initial: Vec::new(),
        }
    }
}

/// The directory used as `XDG_CONFIG_HOME` for the isolated keyfile backend,
/// derived from a session's runtime dir. Both the compositor and the app point
/// here so they share one keyfile.
pub fn config_dir(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("config")
}

/// Absolute path of the keyfile GIO reads under [`config_dir`]. GIO hardcodes
/// the `glib-2.0/settings/keyfile` suffix relative to `XDG_CONFIG_HOME`.
fn keyfile_path(runtime_dir: &Path) -> PathBuf {
    config_dir(runtime_dir).join("glib-2.0/settings/keyfile")
}

/// Render `entries` into GKeyfileSettingsBackend keyfile text.
///
/// Groups by schema path (dots → slashes, as GIO expects); within a group a
/// later entry for the same key replaces an earlier one (last-wins), which is
/// how backend-seeded defaults get overridden by caller entries appended after
/// them. Groups and keys come out in stable sorted order so the file is
/// deterministic (handy for tests and diffs).
pub fn render_keyfile(entries: &[GSettingEntry]) -> String {
    // schema-path -> (key -> value), both sorted, last write wins.
    let mut groups: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for e in entries {
        let group = e.schema.replace('.', "/");
        groups
            .entry(group)
            .or_default()
            .insert(e.key.clone(), e.value.clone());
    }

    let mut out = String::new();
    for (i, (group, kvs)) in groups.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push('[');
        out.push_str(group);
        out.push_str("]\n");
        for (k, v) in kvs {
            out.push_str(k);
            out.push('=');
            out.push_str(v);
            out.push('\n');
        }
    }
    out
}

/// Write the keyfile for a session under [`config_dir`], creating parent
/// directories. Call once, before launching mutter, when isolation is on.
pub fn write_keyfile(runtime_dir: &Path, entries: &[GSettingEntry]) -> std::io::Result<()> {
    let path = keyfile_path(runtime_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, render_keyfile(entries))
}

/// Parse keyfile text back into entries — the inverse of [`render_keyfile`].
///
/// Tolerant line parser: a `[group/path]` header maps back to a dotted schema
/// (`group.path`), a `key=value` line splits on the **first** `=` with the
/// value kept verbatim (GVariant text can itself contain `=`), and blank lines
/// plus `#` comments are skipped. Lines before any group header, and headers
/// that aren't bracketed, are ignored rather than erroring — the file is
/// machine-written by [`render_keyfile`], so this only has to be lenient enough
/// to round-trip our own output and survive light hand edits.
pub fn parse_keyfile(text: &str) -> Vec<GSettingEntry> {
    let mut entries = Vec::new();
    let mut schema: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(group) = line.strip_prefix('[').and_then(|g| g.strip_suffix(']')) {
            schema = Some(group.replace('/', "."));
            continue;
        }
        if let (Some(schema), Some((key, value))) = (&schema, line.split_once('=')) {
            entries.push(GSettingEntry::new(schema.clone(), key.trim(), value));
        }
    }
    entries
}

/// Apply a single GSettings change to a session's keyfile **in place**, after
/// launch — the storage half of [`crate::Session::set_setting`].
///
/// Reads the current keyfile (treated as empty if it doesn't exist yet),
/// appends `entry`, and rewrites via [`write_keyfile`]. Because the new entry
/// is appended last, [`render_keyfile`]'s last-wins merge upserts the target
/// key while preserving every other entry (the mutter `experimental-features`
/// seed, launch-time seeds, and any prior live writes). The truncating in-place
/// write keeps the file's inode, so the running app's `GFileMonitor` fires and
/// GIO re-emits the GSettings `changed` signal — see the module-level
/// "Live updates" note for why this reaches an already-running app.
pub fn live_write(runtime_dir: &Path, entry: &GSettingEntry) -> std::io::Result<()> {
    let path = keyfile_path(runtime_dir);
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut entries = parse_keyfile(&existing);
    entries.push(entry.clone());
    write_keyfile(runtime_dir, &entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_isolated_with_no_seeds() {
        let cfg = GSettingsConfig::default();
        assert!(cfg.isolated);
        assert!(cfg.initial.is_empty());
    }

    #[test]
    fn config_dir_is_config_subdir_of_runtime() {
        let dir = config_dir(Path::new("/run/user/1000/wd-session-abc"));
        assert_eq!(dir, PathBuf::from("/run/user/1000/wd-session-abc/config"));
    }

    #[test]
    fn render_groups_by_schema_path_with_dots_to_slashes() {
        let out = render_keyfile(&[
            GSettingEntry::new("org.gnome.mutter", "experimental-features", "['x']"),
            GSettingEntry::new("org.gnome.desktop.interface", "text-scaling-factor", "1.5"),
        ]);
        // Sorted: interface group before mutter group.
        assert_eq!(
            out,
            "[org/gnome/desktop/interface]\ntext-scaling-factor=1.5\n\n\
             [org/gnome/mutter]\nexperimental-features=['x']\n"
        );
    }

    #[test]
    fn render_last_write_wins_for_same_schema_key() {
        let out = render_keyfile(&[
            GSettingEntry::new("org.gnome.mutter", "experimental-features", "['default']"),
            GSettingEntry::new("org.gnome.mutter", "experimental-features", "['override']"),
        ]);
        assert_eq!(
            out,
            "[org/gnome/mutter]\nexperimental-features=['override']\n"
        );
    }

    #[test]
    fn render_empty_is_empty_string() {
        assert_eq!(render_keyfile(&[]), "");
    }

    #[test]
    fn write_keyfile_creates_nested_path() {
        let dir = tempfile::tempdir().unwrap();
        write_keyfile(
            dir.path(),
            &[GSettingEntry::new(
                "org.gnome.mutter",
                "experimental-features",
                "['x']",
            )],
        )
        .unwrap();
        let written = std::fs::read_to_string(keyfile_path(dir.path())).unwrap();
        assert_eq!(written, "[org/gnome/mutter]\nexperimental-features=['x']\n");
    }

    #[test]
    fn parse_round_trips_render() {
        let entries = [
            GSettingEntry::new(
                "org.gnome.mutter",
                "experimental-features",
                "['scale-monitor-framebuffer']",
            ),
            GSettingEntry::new("org.gnome.desktop.interface", "text-scaling-factor", "1.5"),
        ];
        // render → parse recovers both entries (order is render's sorted order).
        let parsed = parse_keyfile(&render_keyfile(&entries));
        assert_eq!(parsed.len(), 2);
        assert!(parsed.contains(&entries[0]));
        assert!(parsed.contains(&entries[1]));
    }

    #[test]
    fn parse_skips_blanks_and_comments() {
        let text = "# leading comment\n\n[org/gnome/mutter]\n# inline comment\n\
                    experimental-features=['x']\n";
        assert_eq!(
            parse_keyfile(text),
            vec![GSettingEntry::new(
                "org.gnome.mutter",
                "experimental-features",
                "['x']"
            )]
        );
    }

    #[test]
    fn parse_keeps_value_verbatim_past_first_equals() {
        // A value that itself contains '=' must only split on the first one.
        assert_eq!(
            parse_keyfile("[a/b]\nkey=a=b=c\n"),
            vec![GSettingEntry::new("a.b", "key", "a=b=c")]
        );
    }

    #[test]
    fn live_write_upserts_target_key_preserving_others() {
        let dir = tempfile::tempdir().unwrap();
        // Seed as the compositor does: the mutter feature flag + an initial value.
        write_keyfile(
            dir.path(),
            &[
                GSettingEntry::new(
                    "org.gnome.mutter",
                    "experimental-features",
                    "['scale-monitor-framebuffer']",
                ),
                GSettingEntry::new("org.gnome.desktop.interface", "text-scaling-factor", "1.0"),
            ],
        )
        .unwrap();

        // Live-update only the one key, post-launch.
        live_write(
            dir.path(),
            &GSettingEntry::new("org.gnome.desktop.interface", "text-scaling-factor", "2.0"),
        )
        .unwrap();

        let parsed = parse_keyfile(&std::fs::read_to_string(keyfile_path(dir.path())).unwrap());
        // The mutter seed is preserved …
        assert!(parsed.contains(&GSettingEntry::new(
            "org.gnome.mutter",
            "experimental-features",
            "['scale-monitor-framebuffer']",
        )));
        // … and the target key holds the new value with no duplicate left behind.
        let scaling: Vec<_> = parsed
            .iter()
            .filter(|e| e.key == "text-scaling-factor")
            .collect();
        assert_eq!(scaling.len(), 1);
        assert_eq!(scaling[0].value, "2.0");
    }

    #[test]
    fn live_write_creates_keyfile_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        live_write(
            dir.path(),
            &GSettingEntry::new(
                "org.gnome.desktop.interface",
                "color-scheme",
                "'prefer-dark'",
            ),
        )
        .unwrap();
        assert_eq!(
            parse_keyfile(&std::fs::read_to_string(keyfile_path(dir.path())).unwrap()),
            vec![GSettingEntry::new(
                "org.gnome.desktop.interface",
                "color-scheme",
                "'prefer-dark'"
            )]
        );
    }
}
