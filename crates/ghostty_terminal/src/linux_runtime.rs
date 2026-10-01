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
    /// `command-palette-entry` values, in order.
    pub palette_entries: Vec<String>,
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
        "command-palette-entry" => config.palette_entries.push(value.to_string()),
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

/// Ghostty's default `command-palette-entry` list (`src/input/command.zig`
/// at the pinned commit): title, description, action, action key.
const DEFAULT_PALETTE: &[(&str, &str, &str, &str)] = &[
    ("Change Tab Title…", "Prompt for a new title for the current tab.", "prompt_tab_title", "prompt_tab_title"),
    ("Change Terminal Title…", "Prompt for a new title for the current terminal.", "prompt_surface_title", "prompt_surface_title"),
    ("Change Window Title…", "Prompt for a new title for the current window.", "prompt_window_title", "prompt_window_title"),
    ("Check for Updates", "Check for updates to the application.", "check_for_updates", "check_for_updates"),
    ("Clear Screen", "Clear the screen and scrollback.", "clear_screen", "clear_screen"),
    ("Close All Windows", "Close all windows.", "close_all_windows", "close_all_windows"),
    ("Close Other Tabs", "Close all tabs in this window except the current one.", "close_tab:other", "close_tab"),
    ("Close Tab", "Close the current tab.", "close_tab:this", "close_tab"),
    ("Close Tabs to the Right", "Close all tabs to the right of the current one.", "close_tab:right", "close_tab"),
    ("Close Terminal", "Close the current terminal.", "close_surface", "close_surface"),
    ("Close Window", "Close the current window.", "close_window", "close_window"),
    ("Copy Screen as ANSI Sequences to Temporary File and Copy Path", "Copy the screen contents as ANSI escape sequences to a temporary file and copy the path to the clipboard.", "write_screen_file:copy,vt", "write_screen_file"),
    ("Copy Screen as ANSI Sequences to Temporary File and Open", "Copy the screen contents as ANSI escape sequences to a temporary file and open it.", "write_screen_file:open,vt", "write_screen_file"),
    ("Copy Screen as ANSI Sequences to Temporary File and Paste Path", "Copy the screen contents as ANSI escape sequences to a temporary file and paste the path to the file.", "write_screen_file:paste,vt", "write_screen_file"),
    ("Copy Screen as HTML to Temporary File and Copy Path", "Copy the screen contents as HTML to a temporary file and copy the path to the clipboard.", "write_screen_file:copy,html", "write_screen_file"),
    ("Copy Screen as HTML to Temporary File and Open", "Copy the screen contents as HTML to a temporary file and open it.", "write_screen_file:open,html", "write_screen_file"),
    ("Copy Screen as HTML to Temporary File and Paste Path", "Copy the screen contents as HTML to a temporary file and paste the path to the file.", "write_screen_file:paste,html", "write_screen_file"),
    ("Copy Screen to Temporary File and Copy Path", "Copy the screen contents to a temporary file and copy the path to the clipboard.", "write_screen_file:copy", "write_screen_file"),
    ("Copy Screen to Temporary File and Open", "Copy the screen contents to a temporary file and open it.", "write_screen_file:open", "write_screen_file"),
    ("Copy Screen to Temporary File and Paste Path", "Copy the screen contents to a temporary file and paste the path to the file.", "write_screen_file:paste", "write_screen_file"),
    ("Copy Selection as ANSI Sequences to Clipboard", "Copy the selected text as ANSI escape sequences to the clipboard.", "copy_to_clipboard:vt", "copy_to_clipboard"),
    ("Copy Selection as ANSI Sequences to Temporary File and Copy Path", "Copy the selection contents as ANSI escape sequences to a temporary file and copy the path to the clipboard.", "write_selection_file:copy,vt", "write_selection_file"),
    ("Copy Selection as ANSI Sequences to Temporary File and Open", "Copy the selection contents as ANSI escape sequences to a temporary file and open it.", "write_selection_file:open,vt", "write_selection_file"),
    ("Copy Selection as ANSI Sequences to Temporary File and Paste Path", "Copy the selection contents as ANSI escape sequences to a temporary file and paste the path to the file.", "write_selection_file:paste,vt", "write_selection_file"),
    ("Copy Selection as HTML to Clipboard", "Copy the selected text as HTML to the clipboard.", "copy_to_clipboard:html", "copy_to_clipboard"),
    ("Copy Selection as HTML to Temporary File and Copy Path", "Copy the selection contents as HTML to a temporary file and copy the path to the clipboard.", "write_selection_file:copy,html", "write_selection_file"),
    ("Copy Selection as HTML to Temporary File and Open", "Copy the selection contents as HTML to a temporary file and open it.", "write_selection_file:open,html", "write_selection_file"),
    ("Copy Selection as HTML to Temporary File and Paste Path", "Copy the selection contents as HTML to a temporary file and paste the path to the file.", "write_selection_file:paste,html", "write_selection_file"),
    ("Copy Selection as Plain Text to Clipboard", "Copy the selected text as plain text to the clipboard.", "copy_to_clipboard:plain", "copy_to_clipboard"),
    ("Copy Selection to Temporary File and Copy Path", "Copy the selection contents to a temporary file and copy the path to the clipboard.", "write_selection_file:copy", "write_selection_file"),
    ("Copy Selection to Temporary File and Open", "Copy the selection contents to a temporary file and open it.", "write_selection_file:open", "write_selection_file"),
    ("Copy Selection to Temporary File and Paste Path", "Copy the selection contents to a temporary file and paste the path to the file.", "write_selection_file:paste", "write_selection_file"),
    ("Copy Terminal Title to Clipboard", "Copy the terminal title to the clipboard. If the terminal title is not set this has no effect.", "copy_title_to_clipboard", "copy_title_to_clipboard"),
    ("Copy to Clipboard", "Copy the selected text to the clipboard in both plain and styled formats.", "copy_to_clipboard:mixed", "copy_to_clipboard"),
    ("Copy URL to Clipboard", "Copy the URL under the cursor to the clipboard.", "copy_url_to_clipboard", "copy_url_to_clipboard"),
    ("Decrease Font Size", "Decrease the font size by 1 point.", "decrease_font_size:1", "decrease_font_size"),
    ("End Search", "End the current search if any and hide any GUI elements.", "end_search", "end_search"),
    ("Equalize Splits", "Equalize the size of all splits.", "equalize_splits", "equalize_splits"),
    ("Focus Split: Down", "Focus the split below, if it exists.", "goto_split:down", "goto_split"),
    ("Focus Split: Left", "Focus the split to the left, if it exists.", "goto_split:left", "goto_split"),
    ("Focus Split: Next", "Focus the next split, if any.", "goto_split:next", "goto_split"),
    ("Focus Split: Previous", "Focus the previous split, if any.", "goto_split:previous", "goto_split"),
    ("Focus Split: Right", "Focus the split to the right, if it exists.", "goto_split:right", "goto_split"),
    ("Focus Split: Up", "Focus the split above, if it exists.", "goto_split:up", "goto_split"),
    ("Focus Window: Next", "Focus the next window, if any.", "goto_window:next", "goto_window"),
    ("Focus Window: Previous", "Focus the previous window, if any.", "goto_window:previous", "goto_window"),
    ("Ghostty", "Put a little Ghostty in your terminal.", "text:👻", "text"),
    ("Increase Font Size", "Increase the font size by 1 point.", "increase_font_size:1", "increase_font_size"),
    ("Move Tab Left", "Move the current tab to the left.", "move_tab:-1", "move_tab"),
    ("Move Tab Right", "Move the current tab to the right.", "move_tab:1", "move_tab"),
    ("Move Tab to New Window", "Move the current tab to a new window.", "move_tab_to_new_window", "move_tab_to_new_window"),
    ("New Tab", "Open a new tab.", "new_tab", "new_tab"),
    ("New Window", "Open a new window.", "new_window", "new_window"),
    ("Next Search Result", "Navigate to the next search result, if any.", "navigate_search:next", "navigate_search"),
    ("Open Config in New Terminal Window", "Open the config file in a new window using $EDITOR or $VISUAL.", "open_config:new_window", "open_config"),
    ("Open Config Using OS editor", "Open the config file with the OS's default editor.", "open_config:os_open", "open_config"),
    ("Paste from Clipboard", "Paste the contents of the main clipboard.", "paste_from_clipboard", "paste_from_clipboard"),
    ("Paste from Selection", "Paste the contents of the selection clipboard.", "paste_from_selection", "paste_from_selection"),
    ("Previous Search Result", "Navigate to the previous search result, if any.", "navigate_search:previous", "navigate_search"),
    ("Quit", "Quit the application.", "quit", "quit"),
    ("Redo", "Redo the last undone action.", "redo", "redo"),
    ("Reload Config", "Reload the config file.", "reload_config", "reload_config"),
    ("Reset Font Size", "Reset the font size to the default.", "reset_font_size", "reset_font_size"),
    ("Reset Terminal", "Reset the terminal to a clean state.", "reset", "reset"),
    ("Reset Window Size", "Reset the window size to the default.", "reset_window_size", "reset_window_size"),
    ("Scroll Page Down", "Scroll the screen down by a page.", "scroll_page_down", "scroll_page_down"),
    ("Scroll Page Up", "Scroll the screen up by a page.", "scroll_page_up", "scroll_page_up"),
    ("Scroll to Bottom", "Scroll to the bottom of the screen.", "scroll_to_bottom", "scroll_to_bottom"),
    ("Scroll to Selection", "Scroll to the selected text.", "scroll_to_selection", "scroll_to_selection"),
    ("Scroll to Top", "Scroll to the top of the screen.", "scroll_to_top", "scroll_to_top"),
    ("Search Selection", "Start a search for the current text selection.", "search_selection", "search_selection"),
    ("Select All", "Select all text on the screen.", "select_all", "select_all"),
    ("Show On-Screen Keyboard", "Show the on-screen keyboard if present.", "show_on_screen_keyboard", "show_on_screen_keyboard"),
    ("Show the GTK Inspector", "Show the GTK inspector.", "show_gtk_inspector", "show_gtk_inspector"),
    ("Split Down", "Split the terminal down.", "new_split:down", "new_split"),
    ("Split Left", "Split the terminal to the left.", "new_split:left", "new_split"),
    ("Split Right", "Split the terminal to the right.", "new_split:right", "new_split"),
    ("Split Up", "Split the terminal up.", "new_split:up", "new_split"),
    ("Start Search", "Start a search if one isn't already active.", "start_search", "start_search"),
    ("Toggle Background Opacity", "Toggle the background opacity of a window that started transparent.", "toggle_background_opacity", "toggle_background_opacity"),
    ("Toggle Float on Top", "Toggle the float on top state of the current window.", "toggle_window_float_on_top", "toggle_window_float_on_top"),
    ("Toggle Fullscreen", "Toggle the fullscreen state of the current window.", "toggle_fullscreen", "toggle_fullscreen"),
    ("Toggle Inspector", "Toggle the inspector.", "inspector:toggle", "inspector"),
    ("Toggle Maximize", "Toggle the maximized state of the current window.", "toggle_maximize", "toggle_maximize"),
    ("Toggle Mouse Reporting", "Toggle whether mouse events are reported to terminal applications.", "toggle_mouse_reporting", "toggle_mouse_reporting"),
    ("Toggle Read-Only Mode", "Toggle read-only mode for the current surface.", "toggle_readonly", "toggle_readonly"),
    ("Toggle Secure Input", "Toggle secure input mode.", "toggle_secure_input", "toggle_secure_input"),
    ("Toggle Split Zoom", "Toggle the zoom state of the current split.", "toggle_split_zoom", "toggle_split_zoom"),
    ("Toggle Tab Overview", "Toggle the tab overview.", "toggle_tab_overview", "toggle_tab_overview"),
    ("Toggle Window Decorations", "Toggle the window decorations.", "toggle_window_decorations", "toggle_window_decorations"),
    ("Undo", "Undo the last action.", "undo", "undo"),
];

/// As Ghostty: the defaults, plus every `command-palette-entry =
/// title:...,description:...,action:...` line; an empty value clears the
/// list so far.
pub fn command_palette_entries() -> Vec<PaletteCommand> {
    let mut commands: Vec<PaletteCommand> = DEFAULT_PALETTE
        .iter()
        .map(|(title, description, action, action_key)| PaletteCommand {
            title: title.to_string(),
            description: description.to_string(),
            action: action.to_string(),
            action_key: action_key.to_string(),
        })
        .collect();
    for value in &config().palette_entries {
        if value.is_empty() {
            commands.clear();
            continue;
        }
        let mut title = String::new();
        let mut description = String::new();
        let mut action = String::new();
        for field in value.split(',') {
            if let Some((name, field_value)) = field.split_once(':') {
                let field_value = field_value.trim().trim_matches('"').to_string();
                match name.trim() {
                    "title" => title = field_value,
                    "description" => description = field_value,
                    "action" => action = field_value,
                    _ => {}
                }
            } else if !action.is_empty() {
                // A comma inside the action's parameter.
                action.push(',');
                action.push_str(field);
            }
        }
        if !title.is_empty() && !action.is_empty() {
            let action_key = action.split(':').next().unwrap_or_default().to_string();
            commands.push(PaletteCommand { title, description, action, action_key });
        }
    }
    commands
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
