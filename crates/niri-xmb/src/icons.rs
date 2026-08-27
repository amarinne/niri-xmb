//! XDG app icon resolver.
//!
//! Lookup pipeline for a given `app_id` (Wayland app_id / Niri window class):
//!   1. Scan `{XDG_DATA_DIRS}/applications/*.desktop` → app_id/wmclass → icon name
//!   2. Find a PNG or SVG file in XDG icon themes or pixmaps directories
//!   3. Decode to RGBA bytes and scale to `icon_size × icon_size`
//!   4. Cache result (None = not found — falls through to glyph / letter fallback)
//!
//! Desktop file scanning is deferred to first lookup. After that every hit and
//! miss is cached so subsequent renders cost only a HashMap lookup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::debug;

/// Decoded icon: (width, height, RGBA bytes).
pub type IconData = (u32, u32, Vec<u8>);

#[derive(Debug, Clone)]
struct DesktopApp {
    icon: String,
    name: String,
    match_score: u8,
}

pub struct IconCache {
    /// app_id (lowercase) → pixel data (None = confirmed not found)
    cache: HashMap<String, Option<IconData>>,
    /// icon_name → pixel data (shared across multiple apps that use the same icon)
    file_cache: HashMap<String, Option<IconData>>,
    /// Keys that map to icon names: wm_class, exec_basename, desktop_name, etc.
    desktop_map: HashMap<String, DesktopApp>,
    desktop_scanned: bool,
    icon_size: u32,
}

impl Default for IconCache {
    fn default() -> Self {
        Self {
            cache: HashMap::new(),
            file_cache: HashMap::new(),
            desktop_map: HashMap::new(),
            desktop_scanned: false,
            icon_size: 128,
        }
    }
}

impl IconCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return RGBA bytes for the best available icon for `app_id`, or `None`.
    /// Resolution order:
    ///   1. XDG .desktop + hicolor + pixmaps
    pub fn get(&mut self, app_id: &str) -> Option<&IconData> {
        let key = app_id.to_lowercase();

        // Return cached result (including confirmed misses).
        if self.cache.contains_key(&key) {
            return self.cache.get(&key).and_then(|v| v.as_ref());
        }

        self.ensure_desktop_scanned();
        let icon_name = self.resolve_icon_name(&key);
        let data = icon_name.and_then(|name| self.load_icon(&name));

        self.cache.insert(key.clone(), data);
        self.cache.get(&key).and_then(|v| v.as_ref())
    }

    /// Human-readable application name from the matching desktop entry.
    pub fn app_label(&mut self, app_id: &str) -> Option<String> {
        self.ensure_desktop_scanned();
        self.resolve_desktop_app(&app_id.to_lowercase())
            .map(|app| app.name.clone())
    }

    fn ensure_desktop_scanned(&mut self) {
        if self.desktop_scanned {
            return;
        }
        self.desktop_map = scan_desktop_files();
        self.desktop_scanned = true;
        debug!("icons: scanned {} desktop entries", self.desktop_map.len());
    }

    fn resolve_desktop_app(&self, app_id: &str) -> Option<&DesktopApp> {
        app_id_candidates(app_id)
            .into_iter()
            .find_map(|candidate| self.desktop_map.get(&candidate))
    }

    fn resolve_icon_name(&self, app_id: &str) -> Option<String> {
        self.resolve_desktop_app(app_id)
            .map(|app| app.icon.clone())
            .or_else(|| Some(app_id.to_string()))
    }

    fn load_icon(&mut self, icon_name: &str) -> Option<IconData> {
        let key = icon_name.to_lowercase();

        if self.file_cache.contains_key(&key) {
            return self.file_cache.get(&key).and_then(|v| v.clone());
        }

        let result =
            find_icon_file(icon_name).and_then(|path| load_icon_rgba(&path, self.icon_size));

        self.file_cache.insert(key, result.clone());
        result
    }
}

fn app_id_candidates(app_id: &str) -> Vec<String> {
    let app_id = app_id.to_lowercase();
    let mut candidates = vec![app_id.clone()];
    let mut push = |candidate: String| {
        if !candidate.is_empty() && !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    };

    for suffix in ["-terminal", "_terminal", ".terminal", "-window", "_window"] {
        if let Some(base) = app_id.strip_suffix(suffix) {
            push(base.to_string());
        }
    }

    if app_id.contains('.') {
        let parts: Vec<&str> = app_id.split('.').collect();
        if let Some(last) = parts.last() {
            push((*last).to_string());
        }
        if parts.len() >= 2 {
            push(format!(
                "{}-{}",
                parts[parts.len() - 2],
                parts[parts.len() - 1]
            ));
        }
    }

    candidates
}

// ── .desktop file scanning ─────────────────────────────────────────────────────

fn xdg_data_dirs(home: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut push = |dir: String| {
        if !dir.is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };

    match std::env::var("XDG_DATA_HOME") {
        Ok(dir) if !dir.is_empty() => push(dir),
        _ => push(format!("{home}/.local/share")),
    }
    push(format!("{home}/.local/share/flatpak/exports/share"));

    if let Ok(value) = std::env::var("XDG_DATA_DIRS") {
        for dir in value.split(':').filter(|dir| !dir.is_empty()) {
            push(dir.into());
        }
    }

    push("/var/lib/flatpak/exports/share".into());
    push("/usr/local/share".into());
    push("/usr/share".into());
    dirs
}

fn scan_desktop_files() -> HashMap<String, DesktopApp> {
    let mut map: HashMap<String, DesktopApp> = HashMap::new();

    let home = home_dir();
    let search_dirs: Vec<String> = xdg_data_dirs(&home)
        .into_iter()
        .map(|dir| format!("{dir}/applications"))
        .collect();

    for dir in &search_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            parse_desktop_file(&path, &mut map);
        }
    }

    map
}

fn parse_desktop_file(path: &Path, map: &mut HashMap<String, DesktopApp>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };

    let mut in_entry = false;
    let mut icon_name: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut keys: Vec<(String, u8)> = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }

        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());

        match k {
            "Icon" => {
                icon_name = Some(v.to_string());
            }
            "Name" => {
                display_name = Some(v.to_string());
                keys.push((v.to_lowercase(), 10));
            }
            "StartupWMClass" => {
                keys.push((v.to_lowercase(), 90));
            }
            "Exec" => {
                // Strip path and flags to get the bare executable name
                if let Some(exe) = v.split_whitespace().next() {
                    let base = exe.rsplit('/').next().unwrap_or(exe);
                    // Strip common wrappers (env, flatpak run, etc.)
                    let base = base.trim_end_matches(".sh");
                    // Prefer the main launcher over helper entries that reuse
                    // its executable with a fixed mode such as
                    // `gnome-disks --restore-disk-image %U`.
                    let fixed_args = v
                        .split_whitespace()
                        .skip(1)
                        .filter(|arg| !arg.starts_with('%'))
                        .count();
                    let score = 60u8.saturating_sub(fixed_args.min(20) as u8);
                    keys.push((base.to_lowercase(), score));
                }
            }
            _ => {}
        }
    }

    // Also register the .desktop filename stem (e.g. "org.gnome.Files" → "org.gnome.files")
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        keys.push((stem.to_lowercase(), 100));
        // And short form
        if stem.contains('.')
            && let Some(last) = stem.rsplit('.').next()
        {
            keys.push((last.to_lowercase(), 80));
        }
    }

    if let Some(icon) = icon_name {
        let name = display_name.unwrap_or_else(|| icon.clone());
        for (key, match_score) in keys {
            if !key.is_empty() {
                let app = DesktopApp {
                    icon: icon.clone(),
                    name: name.clone(),
                    match_score,
                };
                match map.entry(key) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(app);
                    }
                    std::collections::hash_map::Entry::Occupied(mut entry)
                        if match_score > entry.get().match_score =>
                    {
                        entry.insert(app);
                    }
                    std::collections::hash_map::Entry::Occupied(_) => {}
                }
            }
        }
    }
}

// ── icon file lookup ──────────────────────────────────────────────────────────

fn find_icon_file(icon_name: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(icon_name);
    if direct.is_absolute() && direct.is_file() {
        return Some(direct);
    }

    let home = home_dir();

    // Preferred sizes in order. Scalable icons are rendered through ImageMagick
    // when it is available, while PNG remains a dependency-free fast path.
    let sizes = [
        "128x128",
        "256x256",
        "512x512",
        "1024x1024",
        "192x192",
        "96x96",
        "72x72",
        "64x64",
        "scalable",
        "48x48",
        "36x36",
        "32x32",
        "24x24",
        "22x22",
        "16x16",
        "0x0",
    ];
    let mut themes = Vec::new();
    if let Some(theme) = gtk_icon_theme() {
        themes.push(theme);
    }
    themes.extend(["hicolor".into(), "Adwaita".into(), "HighContrast".into()]);
    themes.dedup();

    let mut roots = Vec::new();
    for directory in xdg_data_dirs(&home) {
        let icons = PathBuf::from(directory).join("icons");
        let icons = icons.to_string_lossy().into_owned();
        if !roots.contains(&icons) {
            roots.push(icons);
        }
    }
    let mut names = vec![icon_name.to_string()];
    let lowercase = icon_name.to_lowercase();
    if lowercase != icon_name {
        names.push(lowercase);
    }

    find_icon_file_in_roots(icon_name, &roots, &themes, &sizes).or_else(|| {
        for directory in xdg_data_dirs(&home)
            .into_iter()
            .map(|directory| format!("{directory}/pixmaps"))
        {
            for name in &names {
                for extension in ["png", "svg"] {
                    let path = PathBuf::from(&directory).join(format!("{name}.{extension}"));
                    if path.is_file() {
                        return Some(path);
                    }
                }
                let path = PathBuf::from(&directory).join(name);
                if path.is_file() {
                    return Some(path);
                }
            }
        }

        None
    })
}

fn find_icon_file_in_roots(
    icon_name: &str,
    roots: &[String],
    themes: &[String],
    sizes: &[&str],
) -> Option<PathBuf> {
    let mut names = vec![icon_name.to_string()];
    let lowercase = icon_name.to_lowercase();
    if lowercase != icon_name {
        names.push(lowercase);
    }
    // Freedesktop themes group icons by context. Application desktop files
    // can legitimately name device or action icons, so do not restrict lookup
    // to `apps/`.
    let contexts = ["apps", "devices", "actions", "places", "mimetypes", "status"];

    for root in roots {
        for theme in &themes {
            for size in &sizes {
                for context in contexts {
                    for name in &names {
                        for extension in ["png", "svg"] {
                            let path = PathBuf::from(root)
                                .join(theme)
                                .join(size)
                                .join(context)
                                .join(format!("{name}.{extension}"));
                            if path.is_file() {
                                return Some(path);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// Read the GTK icon theme name from the user's GTK settings file.
fn gtk_icon_theme() -> Option<String> {
    let home = home_dir();
    let candidates = [
        format!("{home}/.config/gtk-3.0/settings.ini"),
        format!("{home}/.config/gtk-4.0/settings.ini"),
    ];
    for path in &candidates {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("gtk-icon-theme-name")
                && let Some(v) = rest.split('=').nth(1)
            {
                let name = v.trim().trim_matches('"').trim().to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    None
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/root".into())
}

// ── image loading ─────────────────────────────────────────────────────────────

fn load_icon_rgba(path: &Path, icon_size: u32) -> Option<IconData> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) if extension.eq_ignore_ascii_case("png") => load_png_rgba(path),
        Some(extension) if extension.eq_ignore_ascii_case("svg") => load_svg_rgba(path, icon_size),
        _ => None,
    }
}

fn load_svg_rgba(path: &Path, icon_size: u32) -> Option<IconData> {
    let geometry = format!("{icon_size}x{icon_size}");
    let output = Command::new("magick")
        .args(["-background", "none"])
        .arg(path)
        .args([
            "-resize", &geometry, "-gravity", "center", "-extent", &geometry, "-depth", "8",
            "rgba:-",
        ])
        .output()
        .ok()?;
    let expected = icon_size as usize * icon_size as usize * 4;
    if !output.status.success() || output.stdout.len() != expected {
        return None;
    }
    Some((icon_size, icon_size, output.stdout))
}

fn load_png_rgba(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let file = std::fs::File::open(path).ok()?;
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().ok()?;
    let buf_size = reader.output_buffer_size();
    let mut buf = vec![0u8; buf_size];
    let info = reader.next_frame(&mut buf).ok()?;

    let w = info.width;
    let h = info.height;
    let n = (w * h) as usize;

    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..n * 4].to_vec(),
        png::ColorType::Rgb => {
            let src = &buf[..n * 3];
            let mut out = Vec::with_capacity(n * 4);
            for chunk in src.as_chunks::<3>().0 {
                out.push(chunk[0]);
                out.push(chunk[1]);
                out.push(chunk[2]);
                out.push(255);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let src = &buf[..n * 2];
            let mut out = Vec::with_capacity(n * 4);
            for chunk in src.as_chunks::<2>().0 {
                let v = chunk[0];
                out.push(v);
                out.push(v);
                out.push(v);
                out.push(chunk[1]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let src = &buf[..n];
            let mut out = Vec::with_capacity(n * 4);
            for &v in src {
                out.push(v);
                out.push(v);
                out.push(v);
                out.push(255);
            }
            out
        }
        // Indexed (palette) not handled — too rare in practice for app icons
        _ => return None,
    };

    Some((w, h, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_class_resolves_desktop_icon_and_label() {
        let mut cache = IconCache {
            desktop_scanned: true,
            ..IconCache::default()
        };
        cache.desktop_map.insert(
            "btop".into(),
            DesktopApp {
                icon: "btop".into(),
                name: "btop++".into(),
                match_score: 100,
            },
        );

        assert_eq!(
            cache.resolve_icon_name("btop-terminal").as_deref(),
            Some("btop")
        );
        assert_eq!(cache.app_label("btop-terminal").as_deref(), Some("btop++"));
    }

    #[test]
    fn data_dirs_include_flatpak_exports_and_system_fallbacks() {
        let dirs = xdg_data_dirs("/home/test");

        assert!(dirs.contains(&"/home/test/.local/share/flatpak/exports/share".into()));
        assert!(dirs.contains(&"/var/lib/flatpak/exports/share".into()));
        assert!(dirs.contains(&"/usr/local/share".into()));
        assert!(dirs.contains(&"/usr/share".into()));
    }

    #[test]
    fn launcher_with_no_fixed_arguments_wins_exec_alias_collision() {
        let dir = std::env::temp_dir().join(format!("niri-xmb-icons-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let helper = dir.join("helper.desktop");
        let launcher = dir.join("launcher.desktop");
        std::fs::write(
            &helper,
            "[Desktop Entry]\nName=Disk Image Writer\nExec=gnome-disks --restore-disk-image %U\nIcon=drive-removable-media\n",
        )
        .unwrap();
        std::fs::write(
            &launcher,
            "[Desktop Entry]\nName=Disks\nExec=gnome-disks %U\nIcon=org.gnome.DiskUtility\n",
        )
        .unwrap();

        let mut map = HashMap::new();
        parse_desktop_file(&helper, &mut map);
        parse_desktop_file(&launcher, &mut map);

        assert_eq!(map["gnome-disks"].icon, "org.gnome.DiskUtility");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn icon_lookup_accepts_large_sizes_and_non_app_contexts() {
        let root = std::env::temp_dir().join(format!("niri-xmb-theme-{}", std::process::id()));
        let large = root.join("hicolor/1024x1024/apps/openlogi.png");
        let device = root.join("Adwaita/scalable/devices/drive-removable-media.svg");
        std::fs::create_dir_all(large.parent().unwrap()).unwrap();
        std::fs::create_dir_all(device.parent().unwrap()).unwrap();
        std::fs::write(&large, b"icon").unwrap();
        std::fs::write(&device, b"icon").unwrap();
        let roots = vec![root.to_string_lossy().into_owned()];
        let themes = vec!["hicolor".into(), "Adwaita".into()];
        let sizes = ["128x128", "1024x1024", "scalable"];

        assert_eq!(
            find_icon_file_in_roots("openlogi", &roots, &themes, &sizes),
            Some(large)
        );
        assert_eq!(
            find_icon_file_in_roots("drive-removable-media", &roots, &themes, &sizes),
            Some(device)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
