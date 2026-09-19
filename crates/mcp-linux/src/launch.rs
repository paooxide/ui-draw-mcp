//! Resolving an application name to something that can be started.
//!
//! On Linux "an application" is a `.desktop` file. Agents name apps the way
//! people do (`Files`, `Text Editor`, `Firefox`), the accessibility bus names
//! them by binary or bus name (`org.gnome.Nautilus`, `ptyxis`), and the
//! desktop id is a third spelling (`org.gnome.TextEditor`). This module
//! accepts any of them.

use std::path::{Path, PathBuf};

/// A desktop entry, as much of it as launching needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// `org.gnome.TextEditor`, the file name without `.desktop`.
    pub id: String,
    pub name: String,
    pub exec: String,
    /// `StartupWMClass`, when set; a second name the app may use.
    pub wm_class: Option<String>,
    pub dbus_activatable: bool,
    pub no_display: bool,
    pub path: PathBuf,
}

/// The directories desktop files live in, user first so an override wins.
pub fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    {
        dirs.push(home.join("applications"));
    }
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in system.split(':').filter(|s| !s.is_empty()) {
        dirs.push(PathBuf::from(d).join("applications"));
    }
    // Flatpak exports are not always on XDG_DATA_DIRS.
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/flatpak/exports/share/applications"));
    }
    dirs
}

/// Parse the `[Desktop Entry]` group of one file. Localised keys
/// (`Name[fr]`) are ignored; the bare key is what `gtk-launch` shows too.
pub fn parse_desktop_entry(path: &Path, text: &str) -> Option<DesktopEntry> {
    let mut in_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut wm_class = None;
    let mut dbus = false;
    let mut no_display = false;
    let mut hidden = false;
    let mut kind_ok = true;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "Name" => name = Some(v.trim().to_string()),
            "Exec" => exec = Some(v.trim().to_string()),
            "StartupWMClass" => wm_class = Some(v.trim().to_string()),
            "DBusActivatable" => dbus = v.trim() == "true",
            "NoDisplay" => no_display = v.trim() == "true",
            "Hidden" => hidden = v.trim() == "true",
            "Type" => kind_ok = v.trim() == "Application",
            _ => {}
        }
    }
    if hidden || !kind_ok {
        return None;
    }
    let id = path.file_stem()?.to_string_lossy().to_string();
    Some(DesktopEntry {
        id,
        name: name?,
        exec: exec?,
        wm_class,
        dbus_activatable: dbus,
        no_display,
        path: path.to_path_buf(),
    })
}

/// Every desktop entry visible on this machine, first spelling of an id wins.
pub fn all_entries(dirs: &[PathBuf]) -> Vec<DesktopEntry> {
    let mut out: Vec<DesktopEntry> = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "desktop"))
            .collect();
        paths.sort();
        for p in paths {
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            if let Some(entry) = parse_desktop_entry(&p, &text) {
                if !out.iter().any(|e| e.id == entry.id) {
                    out.push(entry);
                }
            }
        }
    }
    out
}

/// Find the entry a name refers to. Exact id first, then exact name, then
/// WM class, then a case-insensitive name prefix, then a substring; visible
/// entries beat `NoDisplay` ones at every step. Returns `None` when nothing
/// plausible matches, and never guesses between two equally good matches
/// of different apps: the first in directory order wins, which is the user
/// override directory.
pub fn resolve<'a>(entries: &'a [DesktopEntry], query: &str) -> Option<&'a DesktopEntry> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    let ql = q.to_lowercase();
    let stem = q.strip_suffix(".desktop").unwrap_or(q);
    let stem_l = stem.to_lowercase();
    let tiers: [&dyn Fn(&DesktopEntry) -> bool; 6] = [
        &|e| e.id == stem,
        &|e| e.id.to_lowercase() == stem_l,
        &|e| e.name.eq_ignore_ascii_case(q),
        &|e| {
            e.wm_class
                .as_deref()
                .is_some_and(|w| w.eq_ignore_ascii_case(q))
        },
        &|e| {
            e.name.to_lowercase().starts_with(&ql)
                || e.id.to_lowercase().ends_with(&format!(".{stem_l}"))
        },
        &|e| e.name.to_lowercase().contains(&ql) || e.id.to_lowercase().contains(&stem_l),
    ];
    for tier in tiers {
        if let Some(e) = entries.iter().find(|e| !e.no_display && tier(e)) {
            return Some(e);
        }
        if let Some(e) = entries.iter().find(|e| tier(e)) {
            return Some(e);
        }
    }
    None
}

/// Does an accessibility-bus application name belong to this entry? The
/// a11y name is the binary name or the bus name, so compare against the
/// id, its last segment, the WM class and the Exec binary.
pub fn entry_matches_a11y_name(entry: &DesktopEntry, a11y_name: &str) -> bool {
    let n = a11y_name.to_lowercase();
    if n.is_empty() {
        return false;
    }
    let id = entry.id.to_lowercase();
    if id == n || id.rsplit('.').next() == Some(n.as_str()) {
        return true;
    }
    if entry
        .wm_class
        .as_deref()
        .is_some_and(|w| w.eq_ignore_ascii_case(a11y_name))
    {
        return true;
    }
    if entry.name.eq_ignore_ascii_case(a11y_name) {
        return true;
    }
    let bin = entry
        .exec
        .split_whitespace()
        .next()
        .and_then(|b| {
            Path::new(b)
                .file_name()
                .map(|f| f.to_string_lossy().to_lowercase())
        })
        .unwrap_or_default();
    bin == n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str, exec: &str) -> DesktopEntry {
        DesktopEntry {
            id: id.into(),
            name: name.into(),
            exec: exec.into(),
            wm_class: None,
            dbus_activatable: false,
            no_display: false,
            path: PathBuf::from(format!("/usr/share/applications/{id}.desktop")),
        }
    }

    #[test]
    fn parses_the_desktop_entry_group_only() {
        let text = "[Desktop Entry]\nName=Text Editor\nName[fr]=Éditeur\nExec=gnome-text-editor %U\nDBusActivatable=true\nStartupWMClass=org.gnome.TextEditor\nType=Application\n\n[Desktop Action new]\nName=New Window\nExec=other\n";
        let e = parse_desktop_entry(Path::new("/x/org.gnome.TextEditor.desktop"), text).unwrap();
        assert_eq!(e.id, "org.gnome.TextEditor");
        assert_eq!(e.name, "Text Editor");
        assert_eq!(e.exec, "gnome-text-editor %U");
        assert!(e.dbus_activatable);
        assert_eq!(e.wm_class.as_deref(), Some("org.gnome.TextEditor"));
    }

    #[test]
    fn hidden_links_and_incomplete_entries_are_skipped() {
        assert!(parse_desktop_entry(
            Path::new("/x/a.desktop"),
            "[Desktop Entry]\nName=A\nExec=a\nHidden=true\n"
        )
        .is_none());
        assert!(parse_desktop_entry(
            Path::new("/x/a.desktop"),
            "[Desktop Entry]\nName=A\nExec=a\nType=Link\n"
        )
        .is_none());
        assert!(
            parse_desktop_entry(Path::new("/x/a.desktop"), "[Desktop Entry]\nName=A\n").is_none()
        );
        assert!(parse_desktop_entry(Path::new("/x/a.desktop"), "").is_none());
        assert!(parse_desktop_entry(Path::new("/x/a.desktop"), "Name=A\nExec=a\n").is_none());
    }

    #[test]
    fn resolution_prefers_exact_then_name_then_prefix_and_visible_over_hidden() {
        let mut hidden = entry(
            "org.gnome.Nautilus.Hidden",
            "Files Helper",
            "nautilus-helper",
        );
        hidden.no_display = true;
        let entries = vec![
            entry("org.gnome.Calculator", "Calculator", "gnome-calculator"),
            hidden,
            entry("org.gnome.Nautilus", "Files", "nautilus --new-window %U"),
            entry("firefox", "Firefox Web Browser", "firefox %u"),
        ];
        assert_eq!(
            resolve(&entries, "org.gnome.Nautilus").unwrap().id,
            "org.gnome.Nautilus"
        );
        assert_eq!(
            resolve(&entries, "org.gnome.nautilus.desktop").unwrap().id,
            "org.gnome.Nautilus"
        );
        assert_eq!(resolve(&entries, "files").unwrap().id, "org.gnome.Nautilus");
        assert_eq!(resolve(&entries, "Fire").unwrap().id, "firefox");
        assert_eq!(
            resolve(&entries, "nautilus").unwrap().id,
            "org.gnome.Nautilus"
        );
        assert_eq!(
            resolve(&entries, "calc").unwrap().id,
            "org.gnome.Calculator"
        );
        assert!(resolve(&entries, "").is_none());
        assert!(resolve(&entries, "   ").is_none());
        assert!(resolve(&entries, "nosuchapp").is_none());
    }

    #[test]
    fn a11y_names_match_by_id_segment_wm_class_or_binary() {
        let mut e = entry("org.gnome.Nautilus", "Files", "nautilus --new-window %U");
        assert!(entry_matches_a11y_name(&e, "org.gnome.Nautilus"));
        assert!(entry_matches_a11y_name(&e, "nautilus"));
        assert!(entry_matches_a11y_name(&e, "Files"));
        assert!(!entry_matches_a11y_name(&e, "ptyxis"));
        assert!(!entry_matches_a11y_name(&e, ""));
        e.wm_class = Some("gnome-files".into());
        assert!(entry_matches_a11y_name(&e, "GNOME-Files"));
        let e = entry(
            "org.gnome.Ptyxis",
            "Ptyxis",
            "/usr/bin/ptyxis --gapplication-service",
        );
        assert!(entry_matches_a11y_name(&e, "ptyxis"));
    }

    #[test]
    fn all_entries_reads_a_directory_and_dedups_by_id() {
        let dir = std::env::temp_dir().join(format!("agentctl-desktop-{}", std::process::id()));
        let user = dir.join("user");
        let sys = dir.join("sys");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::write(
            user.join("a.desktop"),
            "[Desktop Entry]\nName=User A\nExec=a\n",
        )
        .unwrap();
        std::fs::write(
            sys.join("a.desktop"),
            "[Desktop Entry]\nName=System A\nExec=a\n",
        )
        .unwrap();
        std::fs::write(sys.join("b.desktop"), "[Desktop Entry]\nName=B\nExec=b\n").unwrap();
        std::fs::write(sys.join("junk.txt"), "not a desktop file").unwrap();
        let entries = all_entries(&[user.clone(), sys.clone(), dir.join("missing")]);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].name, "User A",
            "user directory overrides the system one"
        );
        assert_eq!(entries[1].id, "b");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(all_entries(&[]).is_empty());
    }
}
