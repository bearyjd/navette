//! Freedesktop icon lookup, PNG only.
//!
//! Not a full icon-theme implementation: no `index.theme` parsing, no
//! inherited themes, no SVG. The drawer wants one raster image per app that
//! looks right at tile size, and every distribution ships one under
//! `hicolor` or `pixmaps`, so that is all this looks at.

use std::env;
use std::path::{Path, PathBuf};

/// Largest first among the sizes that are common enough to be worth
/// checking. 128 and 96 downscale cleanly to a phone tile; 256 is rarer
/// and costlier to ship, so it comes after the small-but-sharp 64 and 48.
const HICOLOR_SIZES: [&str; 6] = ["128x128", "96x96", "64x64", "48x48", "256x256", "32x32"];
const FLATPAK_EXPORTS: &str = "/var/lib/flatpak/exports/share";
const DEFAULT_XDG_DATA_DIRS: &str = "/usr/local/share:/usr/share";

/// Finds a PNG for a `.desktop` `Icon=` value.
///
/// An absolute `.png` path is returned as-is if it exists. A bare name is
/// looked up as `icons/hicolor/<size>/apps/<name>.png` in every data dir,
/// sizes in [`HICOLOR_SIZES`] order, then `pixmaps/<name>.png`. A trailing
/// `.png` or `.svg` on the name is stripped first, since some entries carry
/// one despite the spec. Anything with a path separator that is not
/// absolute is refused rather than joined: an `Icon=` value is a name, and a
/// relative path in one is malformed.
pub fn resolve_icon(name: &str, data_dirs: &[PathBuf]) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.is_absolute() {
        return (candidate.extension().is_some_and(|ext| ext == "png") && candidate.is_file())
            .then(|| candidate.to_path_buf());
    }
    let stem = strip_image_extension(name);
    if stem.is_empty() || stem.contains('/') {
        return None;
    }
    let file = format!("{stem}.png");
    data_dirs.iter().find_map(|dir| {
        HICOLOR_SIZES
            .iter()
            .map(|size| {
                dir.join("icons/hicolor")
                    .join(size)
                    .join("apps")
                    .join(&file)
            })
            .chain(std::iter::once(dir.join("pixmaps").join(&file)))
            .find(|path| path.is_file())
    })
}

fn strip_image_extension(name: &str) -> &str {
    name.strip_suffix(".png")
        .or_else(|| name.strip_suffix(".svg"))
        .unwrap_or(name)
}

/// The directories icons are looked up in, most specific first:
/// `$XDG_DATA_HOME` (default `~/.local/share`), then `$XDG_DATA_DIRS`
/// (default `/usr/local/share:/usr/share`), then the flatpak export tree if
/// it was not already listed. Fedora puts it in `XDG_DATA_DIRS`; other
/// distributions do not, and a flatpak app whose icon is missing from the
/// drawer is a worse failure than one duplicate lookup.
pub fn default_data_dirs() -> Vec<PathBuf> {
    let data_home = env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let data_dirs = env::var_os("XDG_DATA_DIRS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_XDG_DATA_DIRS.into());
    let mut dirs: Vec<PathBuf> = data_home
        .into_iter()
        .chain(env::split_paths(&data_dirs).filter(|dir| !dir.as_os_str().is_empty()))
        .collect();
    let flatpak = PathBuf::from(FLATPAK_EXPORTS);
    if !dirs.contains(&flatpak) {
        dirs.push(flatpak);
    }
    dirs
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"\x89PNG").unwrap();
    }

    #[test]
    fn an_absolute_png_path_that_exists_is_returned_as_is() {
        let temp = TempDir::new().unwrap();
        let icon = temp.path().join("custom/app.png");
        write(&icon);

        assert_eq!(
            resolve_icon(icon.to_str().unwrap(), &[]),
            Some(icon.clone())
        );
        assert_eq!(
            resolve_icon(temp.path().join("missing.png").to_str().unwrap(), &[]),
            None
        );
        let svg = temp.path().join("custom/app.svg");
        write(&svg);
        assert_eq!(resolve_icon(svg.to_str().unwrap(), &[]), None);
    }

    #[test]
    fn hicolor_sizes_are_preferred_in_order() {
        let temp = TempDir::new().unwrap();
        let dirs = vec![temp.path().to_path_buf()];
        let at = |size: &str| {
            temp.path()
                .join(format!("icons/hicolor/{size}/apps/fooapp.png"))
        };

        write(&at("32x32"));
        assert_eq!(resolve_icon("fooapp", &dirs), Some(at("32x32")));
        write(&at("256x256"));
        assert_eq!(resolve_icon("fooapp", &dirs), Some(at("256x256")));
        write(&at("48x48"));
        assert_eq!(resolve_icon("fooapp", &dirs), Some(at("48x48")));
        write(&at("64x64"));
        assert_eq!(resolve_icon("fooapp", &dirs), Some(at("64x64")));
        write(&at("96x96"));
        assert_eq!(resolve_icon("fooapp", &dirs), Some(at("96x96")));
        write(&at("128x128"));
        assert_eq!(resolve_icon("fooapp", &dirs), Some(at("128x128")));
    }

    #[test]
    fn an_earlier_data_dir_wins_over_a_later_one_at_any_size() {
        let home = TempDir::new().unwrap();
        let system = TempDir::new().unwrap();
        let small = home.path().join("icons/hicolor/32x32/apps/fooapp.png");
        let large = system.path().join("icons/hicolor/128x128/apps/fooapp.png");
        write(&small);
        write(&large);

        let dirs = vec![home.path().to_path_buf(), system.path().to_path_buf()];
        assert_eq!(resolve_icon("fooapp", &dirs), Some(small));
    }

    #[test]
    fn pixmaps_is_the_fallback_after_hicolor() {
        let temp = TempDir::new().unwrap();
        let dirs = vec![temp.path().to_path_buf()];
        let pixmap = temp.path().join("pixmaps/fooapp.png");
        write(&pixmap);

        assert_eq!(resolve_icon("fooapp", &dirs), Some(pixmap));

        let hicolor = temp.path().join("icons/hicolor/32x32/apps/fooapp.png");
        write(&hicolor);
        assert_eq!(resolve_icon("fooapp", &dirs), Some(hicolor));
    }

    #[test]
    fn a_trailing_image_extension_on_the_name_is_stripped() {
        let temp = TempDir::new().unwrap();
        let dirs = vec![temp.path().to_path_buf()];
        let icon = temp.path().join("icons/hicolor/64x64/apps/fooapp.png");
        write(&icon);

        assert_eq!(resolve_icon("fooapp.png", &dirs), Some(icon.clone()));
        assert_eq!(resolve_icon("fooapp.svg", &dirs), Some(icon));
    }

    #[test]
    fn svg_only_and_unknown_icons_resolve_to_none() {
        let temp = TempDir::new().unwrap();
        let dirs = vec![temp.path().to_path_buf()];
        write(&temp.path().join("icons/hicolor/scalable/apps/vector.svg"));
        write(&temp.path().join("icons/hicolor/64x64/apps/vector.svg"));

        assert_eq!(resolve_icon("vector", &dirs), None);
        assert_eq!(resolve_icon("nothing-here", &dirs), None);
        assert_eq!(resolve_icon("", &dirs), None);
    }

    #[test]
    fn a_relative_path_is_never_joined_into_the_lookup() {
        let temp = TempDir::new().unwrap();
        let dirs = vec![temp.path().to_path_buf()];
        write(&temp.path().join("pixmaps/fooapp.png"));

        assert_eq!(resolve_icon("../pixmaps/fooapp", &dirs), None);
        assert_eq!(resolve_icon("sub/fooapp", &dirs), None);
    }
}
