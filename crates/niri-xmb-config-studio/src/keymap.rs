//! Read-only Niri keybind map for the Config Studio dock.
//!
//! Niri keybinds live in the compositor's KDL config, not in niri-xmb's TOML,
//! so the dock reads them directly. Parsing follows Niri's own rules: includes
//! resolve relative to the including file, and a later bind replaces an
//! earlier one with the same key. Two identical keys in one `binds` block are
//! an error that stops Niri from loading the config, so they are flagged.
//!
//! The keyboard geometry is a compact ANSI layout modeled on NiriMod's
//! keyboard visualizer (MIT). The bind parser and merge rules are derived from
//! Niri's own `niri-config` implementation (GPL-3.0, same as this crate).

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};

pub const CONFIG_PATH: &str = "~/.config/niri/config.kdl";
const MAX_INCLUDE_DEPTH: usize = 16;

/// One bind row from the Niri config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NiriBind {
    /// Raw hotkey node name, e.g. `Mod+Shift+Q`.
    pub hotkey: String,
    /// Canonical modifier names: `Mod`, `Ctrl`, `Shift`, `Alt`, `Super`, ...
    pub modifiers: Vec<String>,
    /// Lowercased trigger name, e.g. `q`, `left`, `xf86audiomute`.
    pub trigger: String,
    /// Layout key id when the trigger exists on the drawn keyboard.
    pub key_id: Option<&'static str>,
    pub action: String,
    pub args: Vec<String>,
    pub title: Option<String>,
    /// Path of the file that defined this bind.
    pub file: PathBuf,
    /// A later bind replaced this one. Niri keeps only the later definition.
    pub shadowed: bool,
    /// The same key appears twice in one file; Niri refuses to start.
    pub duplicate: bool,
}

impl NiriBind {
    /// Human-readable action, with arguments for `spawn` and friends.
    pub fn action_text(&self) -> String {
        if self.args.is_empty() {
            self.action.clone()
        } else {
            format!("{} {}", self.action, self.args.join(" "))
        }
    }

    /// Full hotkey with canonical modifier spelling.
    pub fn hotkey_text(&self) -> String {
        let mut parts = self.modifiers.clone();
        parts.push(trigger_label(&self.trigger));
        parts.join("+")
    }
}

/// Everything the dock needs from the Niri config.
#[derive(Debug, Default, Clone)]
pub struct NiriKeymap {
    pub binds: Vec<NiriBind>,
    pub files: Vec<PathBuf>,
    pub errors: Vec<String>,
}

impl NiriKeymap {
    /// Load the live Niri config, falling back to the standard path.
    pub fn load_default() -> Self {
        let path = env::var_os("NIRI_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| expand_home(CONFIG_PATH));
        Self::load(&path)
    }

    pub fn load(path: &Path) -> Self {
        let mut loader = Loader::default();
        loader.load_file(path, 0);
        loader.finish()
    }

    /// Binds Niri would actually load, in source order.
    pub fn effective(&self) -> impl Iterator<Item = &NiriBind> {
        self.binds.iter().filter(|bind| !bind.shadowed)
    }

    pub fn effective_count(&self) -> usize {
        self.effective().count()
    }
}

#[derive(Default)]
struct Loader {
    binds: Vec<NiriBind>,
    files: Vec<PathBuf>,
    errors: Vec<String>,
    stack: Vec<PathBuf>,
}

impl Loader {
    fn load_file(&mut self, path: &Path, depth: usize) {
        if depth > MAX_INCLUDE_DEPTH {
            self.errors
                .push(format!("include depth limit reached at {}", path.display()));
            return;
        }
        if self.stack.iter().any(|seen| seen == path) {
            self.errors
                .push(format!("recursive include of {}", path.display()));
            return;
        }
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                self.errors.push(format!("{}: {error}", path.display()));
                return;
            }
        };
        let document = match KdlDocument::parse(&text) {
            Ok(document) => document,
            Err(error) => {
                self.errors.push(format!("{}: {error}", path.display()));
                return;
            }
        };
        if !self.files.iter().any(|seen| seen == path) {
            self.files.push(path.to_path_buf());
        }
        self.stack.push(path.to_path_buf());
        self.walk(&document, path, depth);
        self.stack.pop();
    }

    fn walk(&mut self, document: &KdlDocument, file: &Path, depth: usize) {
        let base = file.parent().unwrap_or_else(|| Path::new("."));
        for node in document.nodes() {
            match node.name().value() {
                "include" => self.include(node, base, depth),
                "binds" => self.collect_binds(node, file),
                _ => {}
            }
        }
    }

    fn include(&mut self, node: &KdlNode, base: &Path, depth: usize) {
        let Some(entry) = node.entries().iter().find(|entry| entry.name().is_none()) else {
            self.errors.push("include without a path".into());
            return;
        };
        let Some(path) = entry.value().as_string() else {
            self.errors.push("include path is not a string".into());
            return;
        };
        let optional = node.entries().iter().any(|entry| {
            entry.name().is_some_and(|name| name.value() == "optional")
                && entry.value().as_bool().unwrap_or(false)
        });
        let path = if let Some(rest) = path.strip_prefix('~') {
            expand_home(rest.trim_start_matches('/'))
        } else {
            base.join(path)
        };
        if optional && !path.exists() {
            return;
        }
        self.load_file(&path, depth + 1);
    }

    fn collect_binds(&mut self, node: &KdlNode, file: &Path) {
        let Some(children) = node.children() else {
            return;
        };
        let first = self.binds.len();
        for bind in children.nodes() {
            let hotkey = bind.name().value().to_string();
            let (modifiers, trigger) = split_hotkey(&hotkey);
            let key_id = layout_id(&trigger);
            let title = bind
                .entries()
                .iter()
                .find(|entry| {
                    entry
                        .name()
                        .is_some_and(|name| name.value() == "hotkey-overlay-title")
                })
                .and_then(property_string);
            let action_node = bind.children().and_then(|doc| doc.nodes().first());
            let action = action_node
                .map(|node| node.name().value().to_string())
                .unwrap_or_default();
            let args = action_node
                .map(|node| {
                    node.entries()
                        .iter()
                        .filter(|entry| entry.name().is_none())
                        .map(entry_text)
                        .collect()
                })
                .unwrap_or_default();
            self.binds.push(NiriBind {
                hotkey,
                modifiers,
                trigger,
                key_id,
                action,
                args,
                title,
                file: file.to_path_buf(),
                shadowed: false,
                duplicate: false,
            });
        }
        // Niri rejects a repeated key inside one `binds` block. Separate blocks
        // (and included files) merge instead, so only check this block.
        let mut counts: HashMap<(Vec<String>, String), Vec<usize>> = HashMap::new();
        for index in first..self.binds.len() {
            counts
                .entry((
                    self.binds[index].modifiers.clone(),
                    self.binds[index].trigger.clone(),
                ))
                .or_default()
                .push(index);
        }
        for indices in counts.values() {
            if indices.len() > 1 {
                for index in indices {
                    self.binds[*index].duplicate = true;
                }
            }
        }
    }

    fn finish(mut self) -> NiriKeymap {
        // Niri replaces an earlier bind with a later one that has the same key.
        // Walk backwards so the surviving bind is the last definition.
        let mut seen: HashMap<(Vec<String>, String), usize> = HashMap::new();
        let mut shadowed = vec![false; self.binds.len()];
        for index in (0..self.binds.len()).rev() {
            let key = (
                self.binds[index].modifiers.clone(),
                self.binds[index].trigger.clone(),
            );
            if seen.insert(key, index).is_some() {
                shadowed[index] = true;
            }
        }
        for (bind, shadowed) in self.binds.iter_mut().zip(shadowed) {
            bind.shadowed = shadowed;
        }
        NiriKeymap {
            binds: self.binds,
            files: self.files,
            errors: self.errors,
        }
    }
}

fn property_string(entry: &KdlEntry) -> Option<String> {
    match entry.value() {
        KdlValue::String(value) => Some(value.clone()),
        KdlValue::Null => None,
        other => Some(other.to_string()),
    }
}

fn entry_text(entry: &KdlEntry) -> String {
    match entry.value() {
        KdlValue::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn split_hotkey(hotkey: &str) -> (Vec<String>, String) {
    let (modifiers, trigger) = match hotkey.rsplit_once('+') {
        Some((modifiers, trigger)) => (modifiers, trigger),
        None => ("", hotkey),
    };
    let modifiers = modifiers
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(canonical_modifier)
        .collect();
    (modifiers, trigger.trim().to_ascii_lowercase())
}

fn canonical_modifier(modifier: &str) -> String {
    match modifier.to_ascii_lowercase().as_str() {
        // Niri's Mod alias is the Super (Mod4) modifier on this keymap.
        "mod" => "Super",
        "ctrl" | "control" => "Ctrl",
        "shift" => "Shift",
        "alt" => "Alt",
        "super" | "win" => "Super",
        "iso_level3_shift" | "mod5" => "Mod5",
        "iso_level5_shift" | "mod3" => "Mod3",
        other => other,
    }
    .to_string()
}

/// Resolve a keysym to the key cap that draws it, when there is one.
pub fn layout_id(trigger: &str) -> Option<&'static str> {
    let id = match trigger {
        "return" | "enter" | "kp_enter" => "return",
        "escape" | "esc" => "escape",
        "backspace" => "backspace",
        "tab" => "tab",
        "space" => "space",
        "bracketleft" => "bracketleft",
        "bracketright" => "bracketright",
        "minus" => "minus",
        "equal" => "equal",
        "period" => "period",
        "comma" => "comma",
        "slash" => "slash",
        "backslash" => "backslash",
        "semicolon" => "semicolon",
        "apostrophe" | "quote" => "quote",
        "grave" => "grave",
        "up" => "up",
        "down" => "down",
        "left" => "left",
        "right" => "right",
        "print" | "sysrq" => "print",
        "delete" | "del" => "delete",
        "insert" | "ins" => "insert",
        "shift_l" => "shiftleft",
        "shift_r" => "shiftright",
        "control_l" => "ctrlleft",
        "alt_l" => "altleft",
        "alt_r" => "altright",
        "super_l" => "superleft",
        "caps_lock" => "capslock",
        "f1" => "f1",
        "f2" => "f2",
        "f3" => "f3",
        "f4" => "f4",
        "f5" => "f5",
        "f6" => "f6",
        "f7" => "f7",
        "f8" => "f8",
        "f9" => "f9",
        "f10" => "f10",
        "f11" => "f11",
        "f12" => "f12",
        "0" => "0",
        "1" => "1",
        "2" => "2",
        "3" => "3",
        "4" => "4",
        "5" => "5",
        "6" => "6",
        "7" => "7",
        "8" => "8",
        "9" => "9",
        letter if letter.len() == 1 && letter.chars().all(|c| c.is_ascii_lowercase()) => {
            // Letters are their own layout ids.
            return ANSI_ROWS
                .iter()
                .flat_map(|row| row.iter())
                .find(|(key, _)| *key == letter)
                .map(|(key, _)| *key);
        }
        _ => return None,
    };
    Some(id)
}

/// Compact ANSI layout: rows of `(key id, width in units)`. An empty id is a gap.
pub const ANSI_ROWS: &[&[(&str, u8)]] = &[
    &[
        ("escape", 4),
        ("", 2),
        ("f1", 3),
        ("f2", 3),
        ("f3", 3),
        ("f4", 3),
        ("", 2),
        ("f5", 3),
        ("f6", 3),
        ("f7", 3),
        ("f8", 3),
        ("", 2),
        ("f9", 3),
        ("f10", 3),
        ("f11", 3),
        ("f12", 3),
        ("", 2),
        ("print", 4),
        ("insert", 4),
        ("delete", 4),
    ],
    &[
        ("grave", 4),
        ("1", 4),
        ("2", 4),
        ("3", 4),
        ("4", 4),
        ("5", 4),
        ("6", 4),
        ("7", 4),
        ("8", 4),
        ("9", 4),
        ("0", 4),
        ("minus", 4),
        ("equal", 4),
        ("backspace", 8),
    ],
    &[
        ("tab", 6),
        ("q", 4),
        ("w", 4),
        ("e", 4),
        ("r", 4),
        ("t", 4),
        ("y", 4),
        ("u", 4),
        ("i", 4),
        ("o", 4),
        ("p", 4),
        ("bracketleft", 4),
        ("bracketright", 4),
        ("backslash", 6),
    ],
    &[
        ("capslock", 7),
        ("a", 4),
        ("s", 4),
        ("d", 4),
        ("f", 4),
        ("g", 4),
        ("h", 4),
        ("j", 4),
        ("k", 4),
        ("l", 4),
        ("semicolon", 4),
        ("quote", 4),
        ("return", 9),
    ],
    &[
        ("shiftleft", 7),
        ("z", 4),
        ("x", 4),
        ("c", 4),
        ("v", 4),
        ("b", 4),
        ("n", 4),
        ("m", 4),
        ("comma", 4),
        ("period", 4),
        ("slash", 4),
        ("shiftright", 5),
        ("up", 4),
        ("", 4),
    ],
    &[
        ("ctrlleft", 6),
        ("superleft", 6),
        ("altleft", 6),
        ("space", 24),
        ("altright", 6),
        ("left", 4),
        ("down", 4),
        ("right", 4),
    ],
];

pub fn key_label(id: &str) -> String {
    let label = match id {
        "escape" => "Esc",
        "backspace" => "Bksp",
        "tab" => "Tab",
        "capslock" => "Caps",
        "return" => "Enter",
        "shiftleft" | "shiftright" => "Shift",
        "ctrlleft" | "ctrlright" => "Ctrl",
        "superleft" | "superright" => "Super",
        "altleft" | "altright" => "Alt",
        "space" => "",
        "print" => "PrtSc",
        "insert" => "Ins",
        "delete" => "Del",
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        "grave" => "`",
        "minus" => "-",
        "equal" => "=",
        "bracketleft" => "[",
        "bracketright" => "]",
        "backslash" => "\\",
        "semicolon" => ";",
        "quote" => "'",
        "comma" => ",",
        "period" => ".",
        "slash" => "/",
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            return id.to_ascii_uppercase();
        }
        other => other,
    };
    label.to_ascii_uppercase()
}

/// Chip label for keys that are not on the drawn keyboard.
pub fn trigger_label(trigger: &str) -> String {
    match trigger {
        "xf86audioraisevolume" => "Vol +".into(),
        "xf86audiolowervolume" => "Vol -".into(),
        "xf86audiomute" => "Mute".into(),
        "xf86audiomicmute" => "Mic Mute".into(),
        "xf86audioplay" => "Play".into(),
        "xf86audiostop" => "Stop".into(),
        "xf86audioprev" => "Prev".into(),
        "xf86audionext" => "Next".into(),
        "xf86monbrightnessup" => "Bright +".into(),
        "xf86monbrightnessdown" => "Bright -".into(),
        "mouseleft" => "Mouse Left".into(),
        "mouseright" => "Mouse Right".into(),
        "mousemiddle" => "Mouse Middle".into(),
        "wheelscrollup" => "Wheel Up".into(),
        "wheelscrolldown" => "Wheel Down".into(),
        "touchpadscrollup" => "Touchpad Up".into(),
        "touchpadscrolldown" => "Touchpad Down".into(),
        "page_up" | "pageup" => "Page Up".into(),
        "page_down" | "pagedown" => "Page Down".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        "space" => "Space".into(),
        "return" | "enter" | "kp_enter" => "Enter".into(),
        "escape" | "esc" => "Esc".into(),
        "tab" => "Tab".into(),
        "backspace" => "Backspace".into(),
        "delete" | "del" => "Del".into(),
        "insert" | "ins" => "Ins".into(),
        "slash" => "/".into(),
        "backslash" => "\\".into(),
        "semicolon" => ";".into(),
        "apostrophe" | "quote" => "'".into(),
        "comma" => ",".into(),
        "period" => ".".into(),
        "minus" => "-".into(),
        "equal" => "=".into(),
        "bracketleft" => "[".into(),
        "bracketright" => "]".into(),
        "grave" => "`".into(),
        "up" => "Up".into(),
        "down" => "Down".into(),
        "left" => "Left".into(),
        "right" => "Right".into(),
        "print" | "sysrq" => "PrtSc".into(),
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            trigger.to_ascii_uppercase()
        }
        "shift_l" => "Shift".into(),
        "shift_r" => "Shift".into(),
        "control_l" => "Ctrl".into(),
        "control_r" => "Ctrl".into(),
        "super_l" => "Super".into(),
        "super_r" => "Super".into(),
        "alt_l" => "Alt".into(),
        "alt_r" => "Alt".into(),
        "caps_lock" => "Caps".into(),
        other if other.len() == 1 => other.to_ascii_uppercase(),
        other => other.into(),
    }
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix('~') {
        let home = env::var_os("HOME").unwrap_or_default();
        PathBuf::from(home).join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir =
                env::temp_dir().join(format!("niri-xmb-keymap-{}-{unique}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.dir.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn parses_binds_and_actions() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "config.kdl",
            r#"
binds {
    Mod+Shift+Q hotkey-overlay-title="Close" { close-window; }
    Mod+T { spawn "kitty" "-e" "htop"; }
}
"#,
        );
        let keymap = NiriKeymap::load(&path);
        assert!(keymap.errors.is_empty(), "{:?}", keymap.errors);
        assert_eq!(keymap.binds.len(), 2);
        let first = &keymap.binds[0];
        assert_eq!(first.hotkey, "Mod+Shift+Q");
        assert_eq!(first.modifiers, vec!["Super", "Shift"]);
        assert_eq!(first.trigger, "q");
        assert_eq!(first.key_id, Some("q"));
        assert_eq!(first.action, "close-window");
        assert_eq!(first.title.as_deref(), Some("Close"));
        assert_eq!(keymap.binds[1].action_text(), "spawn kitty -e htop");
    }

    #[test]
    fn follows_includes_in_order_and_marks_shadowed_binds() {
        let fixture = Fixture::new();
        fixture.write(
            "config.d/10-binds.kdl",
            r#"
binds {
    Mod+T { spawn "kitty"; }
    Mod+Y { spawn "yazi"; }
}
"#,
        );
        let path = fixture.write(
            "config.kdl",
            r#"
include "config.d/10-binds.kdl"
binds {
    Mod+T { spawn "ghostty"; }
}
include "config.d/optional.kdl" optional=true
"#,
        );
        let keymap = NiriKeymap::load(&path);
        assert!(keymap.errors.is_empty(), "{:?}", keymap.errors);
        assert_eq!(keymap.binds.len(), 3);
        assert!(keymap.binds[0].shadowed, "first Mod+T is replaced");
        assert!(!keymap.binds[1].shadowed);
        assert!(!keymap.binds[2].shadowed);
        assert_eq!(keymap.effective_count(), 2);
        let tee = keymap
            .effective()
            .find(|bind| bind.hotkey == "Mod+T")
            .unwrap();
        assert_eq!(tee.action, "spawn");
        assert_eq!(tee.args, vec!["ghostty"]);
        assert_eq!(keymap.files.len(), 2);
    }

    #[test]
    fn flags_duplicate_keys_in_one_file() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "config.kdl",
            r#"
binds {
    Mod+Q { close-window; }
    Mod+Q { quit; }
}
"#,
        );
        let keymap = NiriKeymap::load(&path);
        assert!(keymap.binds.iter().all(|bind| bind.duplicate));
        assert!(keymap.binds[0].shadowed);
    }

    #[test]
    fn separate_binds_blocks_replace_instead_of_conflicting() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "config.kdl",
            r#"
binds {
    Mod+Q { close-window; }
}
binds {
    Mod+Q { quit; }
}
"#,
        );
        let keymap = NiriKeymap::load(&path);
        assert_eq!(keymap.binds.len(), 2);
        assert!(keymap.binds.iter().all(|bind| !bind.duplicate));
        assert!(keymap.binds[0].shadowed);
        assert!(!keymap.binds[1].shadowed);
    }

    #[test]
    fn reports_parse_errors_without_losing_the_file_list() {
        let fixture = Fixture::new();
        let path = fixture.write("config.kdl", "binds { Mod+Q { close-window; } ");
        let keymap = NiriKeymap::load(&path);
        assert!(!keymap.errors.is_empty());
        assert!(keymap.binds.is_empty());
    }

    #[test]
    fn maps_common_keysyms_to_the_ansi_layout() {
        assert_eq!(layout_id("return"), Some("return"));
        assert_eq!(layout_id("enter"), Some("return"));
        assert_eq!(layout_id("shift_l"), Some("shiftleft"));
        assert_eq!(layout_id("super_l"), Some("superleft"));
        assert_eq!(layout_id("a"), Some("a"));
        assert_eq!(layout_id("xf86audiomute"), None);
        assert_eq!(key_label("f5"), "F5");
    }

    #[test]
    fn splits_modifiers_case_insensitively() {
        let (modifiers, trigger) = split_hotkey("mod+SHIFT+Left");
        assert_eq!(modifiers, vec!["Super", "Shift"]);
        assert_eq!(trigger, "left");
    }
}
