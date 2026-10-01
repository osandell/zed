//! Ghostty's config on Linux. libghostty-vt has no config of its own, so the
//! subset the terminal draws with is read here from the same files Ghostty
//! reads (`~/.config/ghostty/config`, its `config-file` includes and the
//! `theme` files), with the same functions `runtime.rs` offers on macOS.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use gpui::{App, Rgba, TaskExt as _};
use parking_lot::Mutex;

pub struct TerminalColors {
    pub background: Rgba,
    pub foreground: Rgba,
}

/// One of the config's `command-palette-entry` commands.
pub struct PaletteCommand {
    pub title: String,
    pub description: String,
    pub action: String,
    pub action_key: String,
}

#[derive(Clone, Default)]
pub struct Config {
    pub font_family: Option<String>,
    pub font_size: f32,
    pub background: Option<Rgba>,
    pub foreground: Option<Rgba>,
    pub cursor_color: Option<Rgba>,
    pub cursor_text: Option<Rgba>,
    pub selection_background: Option<Rgba>,
    pub selection_foreground: Option<Rgba>,
    pub palette: HashMap<u8, Rgba>,
    pub command: Option<String>,
    pub env: Vec<(String, String)>,
    /// Left/right and top/bottom padding in points.
    pub padding_x: (f32, f32),
    pub padding_y: (f32, f32),
    /// `keybind` lines on top of the macOS defaults, in order: (trigger, action).
    pub keybinds: Vec<(String, String)>,
    pub scrollback_lines: Option<usize>,
    /// The last value of every key, for `config_color` / `config_f64`.
    raw: HashMap<String, String>,
}

static CONFIG: Mutex<Option<Arc<Config>>> = Mutex::new(None);
static DARK: AtomicBool = AtomicBool::new(true);
/// Bumped on every reload, so terminals notice and re-apply.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

pub fn config() -> Arc<Config> {
    let mut config = CONFIG.lock();
    config
        .get_or_insert_with(|| Arc::new(load(DARK.load(Ordering::Relaxed))))
        .clone()
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join(".config"))
        .join("ghostty")
}

/// Ghostty's resources (terminfo, shell integration, themes): the prebuilt
/// kit next to the worktrees, else an installed Ghostty.
pub fn resources_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GHOSTTY_RESOURCES_DIR") {
        return Some(PathBuf::from(dir));
    }
    let exe_relative = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("../share/ghostty")));
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    [
        exe_relative,
        Some(manifest.join("../../../../ghostty-vt/share/ghostty")),
        Some(manifest.join("../../../ghostty-vt/share/ghostty")),
        Some(PathBuf::from("/snap/ghostty/current/share/ghostty")),
        Some(PathBuf::from("/usr/share/ghostty")),
    ]
    .into_iter()
    .flatten()
    .find(|dir| dir.join("shell-integration").is_dir())
    .and_then(|dir| dir.canonicalize().ok())
}

/// The Ghostty config file, created if it does not exist yet.
pub fn config_file_path() -> Option<PathBuf> {
    let dir = config_dir();
    let path = dir.join("config");
    if !path.exists() {
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(&path, "").ok()?;
    }
    Some(path)
}

fn parse_color(value: &str) -> Option<Rgba> {
    let hex = value.trim().trim_start_matches('#');
    let value = u32::from_str_radix(hex, 16).ok()?;
    (hex.len() == 6).then(|| gpui::rgb(value))
}

/// `(a, b)` from Ghostty's `a,b` or `a` padding values.
fn parse_pair(value: &str) -> (f32, f32) {
    let mut parts = value.split(',').map(|part| part.trim().parse::<f32>().unwrap_or(0.));
    let first = parts.next().unwrap_or(0.);
    (first, parts.next().unwrap_or(first))
}

fn apply(config: &mut Config, key: &str, value: &str) {
    config.raw.insert(key.to_string(), value.to_string());
    match key {
        "font-family" if !value.is_empty() => {
            config.font_family.get_or_insert_with(|| value.to_string());
        }
        "font-size" => {
            if let Ok(size) = value.parse() {
                config.font_size = size;
            }
        }
        "background" => config.background = parse_color(value),
        "foreground" => config.foreground = parse_color(value),
        "cursor-color" => config.cursor_color = parse_color(value),
        "cursor-text" => config.cursor_text = parse_color(value),
        "selection-background" => config.selection_background = parse_color(value),
        "selection-foreground" => config.selection_foreground = parse_color(value),
        "palette" => {
            if let Some((index, color)) = value.split_once('=')
                && let (Ok(index), Some(color)) = (index.trim().parse::<u8>(), parse_color(color))
            {
                config.palette.insert(index, color);
            }
        }
        "command" => config.command = Some(value.to_string()),
        "env" => {
            if let Some((name, env_value)) = value.split_once('=') {
                config.env.push((name.trim().to_string(), env_value.trim().to_string()));
            }
        }
        "window-padding-x" => config.padding_x = parse_pair(value),
        "window-padding-y" => config.padding_y = parse_pair(value),
        "keybind" => {
            if let Some((trigger, action)) = value.split_once('=') {
                config.keybinds.push((trigger.trim().to_string(), action.trim().to_string()));
            }
        }
        "scrollback-limit" => config.scrollback_lines = value.parse::<usize>().ok().map(|bytes| bytes / 100),
        _ => {}
    }
}

/// Key/value lines of one file, `config-file` includes resolved in place.
fn read_file(path: &Path, depth: usize, out: &mut Vec<(String, String)>) {
    if depth > 8 {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        if key == "config-file" {
            let include = value.trim_start_matches('?');
            let include = Path::new(include);
            let include = if include.is_absolute() {
                include.to_path_buf()
            } else {
                path.parent().unwrap_or(Path::new("/")).join(include)
            };
            read_file(&include, depth + 1, out);
        } else {
            out.push((key.to_string(), value.to_string()));
        }
    }
}

fn theme_file(name: &str) -> Option<PathBuf> {
    let user = config_dir().join("themes").join(name);
    if user.exists() {
        return Some(user);
    }
    let path = Path::new(name);
    if path.is_absolute() && path.exists() {
        return Some(path.to_path_buf());
    }
    resources_dir().map(|dir| dir.join("themes").join(name)).filter(|path| path.exists())
}

fn load(dark: bool) -> Config {
    let mut lines = Vec::new();
    if let Some(path) = config_file_path() {
        read_file(&path, 0, &mut lines);
    }
    let mut config = Config {
        font_size: 13.,
        padding_x: (2., 2.),
        padding_y: (2., 2.),
        ..Config::default()
    };
    // As Ghostty: the theme first, the config's own values on top of it.
    let theme = lines.iter().rev().find(|(key, _)| key == "theme").map(|(_, value)| value.clone());
    if let Some(theme) = theme {
        let name = if theme.contains(':') {
            theme.split(',').find_map(|part| {
                let (variant, name) = part.split_once(':')?;
                (variant.trim() == if dark { "dark" } else { "light" }).then(|| name.trim().to_string())
            })
        } else {
            Some(theme)
        };
        if let Some(path) = name.as_deref().and_then(theme_file) {
            let mut theme_lines = Vec::new();
            read_file(&path, 0, &mut theme_lines);
            for (key, value) in theme_lines {
                apply(&mut config, &key, &value);
            }
        }
    }
    for (key, value) in lines {
        apply(&mut config, &key, &value);
    }
    config
}

fn reload_app_config() {
    let config = load(DARK.load(Ordering::Relaxed));
    *CONFIG.lock() = Some(Arc::new(config));
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Reloads the config files, like Ghostty's `reload_config` binding.
pub fn reload_config() {
    reload_app_config();
}

pub fn terminal_font_family() -> Option<gpui::SharedString> {
    config().font_family.clone().map(Into::into)
}

pub fn terminal_font_size() -> f32 {
    config().font_size
}

pub fn open_config(cx: &mut App) {
    let Some(path) = config_file_path() else {
        log::warn!("Ghostty has no config file to open");
        return;
    };
    let Some(app_state) = workspace::AppState::try_global(cx) else {
        return;
    };
    workspace::open_paths(&[path], app_state, workspace::OpenOptions::default(), cx)
        .detach_and_log_err(cx);
}

pub fn terminal_colors() -> TerminalColors {
    let config = config();
    TerminalColors {
        background: config.background.unwrap_or_else(|| gpui::rgb(0x282c34)),
        foreground: config.foreground.unwrap_or_else(|| gpui::rgb(0xffffff)),
    }
}

pub fn config_color(key: &str) -> Option<Rgba> {
    config().raw.get(key).and_then(|value| parse_color(value))
}

pub fn config_f64(key: &str) -> Option<f64> {
    config().raw.get(key).and_then(|value| value.parse().ok())
}

pub fn command_palette_entries() -> Vec<PaletteCommand> {
    Vec::new()
}

/// Follows the system light/dark appearance, which picks the `theme =
/// light:...,dark:...` variant.
pub fn set_color_scheme(dark: bool) {
    if DARK.swap(dark, Ordering::Relaxed) != dark {
        reload_app_config();
    }
}

pub fn is_dark() -> bool {
    DARK.load(Ordering::Relaxed)
}
