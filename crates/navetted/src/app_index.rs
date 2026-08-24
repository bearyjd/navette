use std::collections::{BTreeMap, HashSet};
use std::env;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use freedesktop_desktop_entry::{
    DesktopEntry, Iter, current_desktop, default_paths, get_languages_from_env,
};
use navette_protocol::App;

#[derive(Clone, Debug, Default)]
pub struct AppIndex {
    apps: BTreeMap<String, App>,
}

impl AppIndex {
    pub fn from_apps(apps: impl IntoIterator<Item = App>) -> Self {
        Self {
            apps: apps.into_iter().map(|app| (app.id.clone(), app)).collect(),
        }
    }

    pub fn load() -> Self {
        let locales = get_languages_from_env();
        let desktops = current_desktop().unwrap_or_default();
        let path = env::var_os("PATH").unwrap_or_default();
        Self::load_from_paths(default_paths(), &locales, &desktops, &path)
    }

    pub fn load_from_paths<I>(
        paths: I,
        locales: &[String],
        desktops: &[String],
        executable_path: &std::ffi::OsStr,
    ) -> Self
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let mut apps = BTreeMap::new();
        let entries = Iter::new(paths.into_iter()).entries(Some(locales));

        for entry in entries {
            let id = entry.id().to_string();
            if apps.contains_key(&id) || !entry_is_visible(&entry, desktops) {
                continue;
            }
            let Some(name) = entry.name(locales).filter(|name| !name.is_empty()) else {
                continue;
            };
            let Ok(exec) = entry.parse_exec() else {
                continue;
            };
            if exec.is_empty()
                || entry
                    .try_exec()
                    .is_some_and(|candidate| !command_exists(candidate, executable_path))
            {
                continue;
            }

            apps.insert(
                id.clone(),
                App {
                    id,
                    name: name.into_owned(),
                    icon: entry
                        .icon()
                        .filter(|icon| !icon.is_empty())
                        .map(str::to_string),
                    categories: entry
                        .categories()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|category| !category.is_empty())
                        .map(str::to_string)
                        .collect(),
                    exec,
                    terminal: entry.terminal(),
                },
            );
        }

        Self { apps }
    }

    pub fn get(&self, id: &str) -> Option<&App> {
        self.apps.get(id)
    }

    pub fn list(&self) -> Vec<App> {
        let mut apps = self.apps.values().cloned().collect::<Vec<_>>();
        apps.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        apps
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }
}

fn entry_is_visible(entry: &DesktopEntry, desktops: &[String]) -> bool {
    if entry.type_() != Some("Application") || entry.hidden() || entry.no_display() {
        return false;
    }

    let current = desktops
        .iter()
        .map(|desktop| desktop.to_lowercase())
        .collect::<HashSet<_>>();

    if entry.only_show_in().is_some_and(|allowed| {
        !allowed
            .into_iter()
            .any(|desktop| current.contains(&desktop.to_lowercase()))
    }) {
        return false;
    }

    !entry.not_show_in().is_some_and(|denied| {
        denied
            .into_iter()
            .any(|desktop| current.contains(&desktop.to_lowercase()))
    })
}

fn command_exists(candidate: &str, executable_path: &std::ffi::OsStr) -> bool {
    let candidate = Path::new(candidate);
    if candidate.components().count() > 1 {
        return is_executable(candidate);
    }

    env::split_paths(executable_path).any(|directory| is_executable(&directory.join(candidate)))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write_entry(root: &Path, name: &str, body: &str) {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join(name), body).unwrap();
    }

    fn entry(name: &str, extra: &str) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName={name}\nExec=/bin/echo %U --flag\n{extra}\n"
        )
    }

    fn load(paths: Vec<PathBuf>) -> AppIndex {
        AppIndex::load_from_paths(paths, &[], &["kde".into()], "/bin".as_ref())
    }

    #[test]
    fn loads_visible_application_and_strips_uri_field_code() {
        let temp = TempDir::new().unwrap();
        write_entry(
            temp.path(),
            "browser.desktop",
            &entry(
                "Browser",
                "Icon=browser\nCategories=Network;WebBrowser;\nTerminal=true",
            ),
        );

        let index = load(vec![temp.path().to_path_buf()]);
        let app = index.get("browser").unwrap();
        assert_eq!(app.name, "Browser");
        assert_eq!(app.exec, ["/bin/echo", "--flag"]);
        assert_eq!(app.icon.as_deref(), Some("browser"));
        assert_eq!(app.categories, ["Network", "WebBrowser"]);
        assert!(app.terminal);
    }

    #[test]
    fn filters_hidden_no_display_wrong_type_and_invalid_entries() {
        let temp = TempDir::new().unwrap();
        write_entry(
            temp.path(),
            "hidden.desktop",
            &entry("Hidden", "Hidden=true"),
        );
        write_entry(
            temp.path(),
            "nodisplay.desktop",
            &entry("No display", "NoDisplay=true"),
        );
        write_entry(
            temp.path(),
            "link.desktop",
            "[Desktop Entry]\nType=Link\nName=Link\nURL=https://example.com\n",
        );
        write_entry(
            temp.path(),
            "missing-name.desktop",
            "[Desktop Entry]\nType=Application\nExec=/bin/echo\n",
        );
        write_entry(
            temp.path(),
            "missing-exec.desktop",
            "[Desktop Entry]\nType=Application\nName=Missing exec\n",
        );

        assert!(load(vec![temp.path().to_path_buf()]).is_empty());
    }

    #[test]
    fn higher_priority_duplicate_wins() {
        let high = TempDir::new().unwrap();
        let low = TempDir::new().unwrap();
        write_entry(high.path(), "same.desktop", &entry("High", ""));
        write_entry(low.path(), "same.desktop", &entry("Low", ""));

        let index = load(vec![high.path().to_path_buf(), low.path().to_path_buf()]);
        assert_eq!(index.get("same").unwrap().name, "High");
    }

    #[test]
    fn honors_desktop_visibility_rules() {
        let temp = TempDir::new().unwrap();
        write_entry(temp.path(), "kde.desktop", &entry("KDE", "OnlyShowIn=KDE;"));
        write_entry(
            temp.path(),
            "gnome.desktop",
            &entry("GNOME", "OnlyShowIn=GNOME;"),
        );
        write_entry(
            temp.path(),
            "denied.desktop",
            &entry("Denied", "NotShowIn=KDE;"),
        );

        let index = load(vec![temp.path().to_path_buf()]);
        assert!(index.get("kde").is_some());
        assert!(index.get("gnome").is_none());
        assert!(index.get("denied").is_none());
    }

    #[test]
    fn filters_missing_try_exec() {
        let temp = TempDir::new().unwrap();
        write_entry(
            temp.path(),
            "missing.desktop",
            &entry("Missing", "TryExec=definitely-not-installed-navette-test"),
        );
        write_entry(
            temp.path(),
            "present.desktop",
            &entry("Present", "TryExec=echo"),
        );

        let index = load(vec![temp.path().to_path_buf()]);
        assert!(index.get("missing").is_none());
        assert!(index.get("present").is_some());
    }

    #[test]
    fn list_is_sorted_by_display_name() {
        let temp = TempDir::new().unwrap();
        write_entry(temp.path(), "z.desktop", &entry("alpha", ""));
        write_entry(temp.path(), "a.desktop", &entry("Zulu", ""));

        let names = load(vec![temp.path().to_path_buf()])
            .list()
            .into_iter()
            .map(|app| app.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["alpha", "Zulu"]);
    }
}
