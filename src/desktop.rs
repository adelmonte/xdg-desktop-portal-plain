//! Desktop entries and mimeapps.list: app names and default apps.

use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn env_dirs(var: &str, default: &str) -> Vec<PathBuf> {
    let v = std::env::var(var).ok().filter(|v| !v.is_empty());
    v.as_deref().unwrap_or(default).split(':').filter(|d| !d.is_empty()).map(PathBuf::from).collect()
}

fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/share"))];
    dirs.extend(env_dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share"));
    dirs.push(home().join(".local/share/flatpak/exports/share"));
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    let mut seen = Vec::new();
    dirs.retain(|d| if seen.contains(d) { false } else { seen.push(d.clone()); true });
    dirs
}

fn config_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"))];
    dirs.extend(env_dirs("XDG_CONFIG_DIRS", "/etc/xdg"));
    dirs
}

/// The value of `key` in `[group]` of an ini-style file's text.
fn ini_get<'a>(text: &'a str, group: &str, key: &str) -> Option<&'a str> {
    let mut in_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == format!("[{group}]");
        } else if in_group
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim());
        }
    }
    None
}

/// An app's name from its desktop entry, `id` without `.desktop`.
pub fn name(id: &str) -> Option<String> {
    if id.is_empty() || id.contains('/') {
        return None;
    }
    data_dirs().iter().find_map(|d| {
        let text = std::fs::read_to_string(d.join("applications").join(format!("{id}.desktop"))).ok()?;
        ini_get(&text, "Desktop Entry", "Name").map(String::from)
    })
}

/// An app's name, or its id when it has none.
pub fn label(id: &str) -> String {
    name(id).unwrap_or_else(|| id.to_string())
}

fn defaults_in(file: &Path, content_type: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(file) else { return Vec::new() };
    ini_get(&text, "Default Applications", content_type)
        .map(|v| v.split(';').map(str::trim).filter(|s| !s.is_empty())
            .map(|s| s.strip_suffix(".desktop").unwrap_or(s).to_string()).collect())
        .unwrap_or_default()
}

/// The default apps for a content type from every mimeapps.list, most preferred first.
pub fn defaults(content_type: &str) -> Vec<String> {
    let files = config_dirs().into_iter().map(|d| d.join("mimeapps.list"))
        .chain(data_dirs().into_iter().map(|d| d.join("applications/mimeapps.list")));
    files.flat_map(|f| defaults_in(&f, content_type)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini() {
        let text = "[Desktop Entry]\nName=Firefox\nName[de]=Feuerfuchs\n[Desktop Action new]\nName=New\n";
        assert_eq!(ini_get(text, "Desktop Entry", "Name"), Some("Firefox"));
        assert_eq!(ini_get(text, "Desktop Action new", "Name"), Some("New"));
        assert_eq!(ini_get(text, "Desktop Entry", "Exec"), None);
    }

    #[test]
    fn mimeapps() {
        let dir = std::env::temp_dir().join(format!("xdpp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("mimeapps.list");
        std::fs::write(&f, "[Added Associations]\napplication/pdf=x.desktop\n[Default Applications]\napplication/pdf=zathura.desktop;org.mozilla.firefox.desktop;\n").unwrap();
        assert_eq!(defaults_in(&f, "application/pdf"), vec!["zathura", "org.mozilla.firefox"]);
        assert!(defaults_in(&f, "image/png").is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
