//! The few libghostty (embedded API) types and constants the shared code
//! names, for Linux, where libghostty's embedded API does not exist. Names
//! and meaning follow `ghostty_embed` so the column and arcoscope code is the
//! same on both platforms; the values only need to be distinct.

#![allow(non_upper_case_globals, non_camel_case_types, dead_code)]

pub type ghostty_input_mods_e = u32;
pub const GHOSTTY_MODS_NONE: ghostty_input_mods_e = 0;
pub const GHOSTTY_MODS_SHIFT: ghostty_input_mods_e = 1 << 0;
pub const GHOSTTY_MODS_CTRL: ghostty_input_mods_e = 1 << 1;
pub const GHOSTTY_MODS_ALT: ghostty_input_mods_e = 1 << 2;
pub const GHOSTTY_MODS_SUPER: ghostty_input_mods_e = 1 << 3;

pub type ghostty_action_split_direction_e = u32;
pub const GHOSTTY_SPLIT_DIRECTION_RIGHT: ghostty_action_split_direction_e = 0;
pub const GHOSTTY_SPLIT_DIRECTION_DOWN: ghostty_action_split_direction_e = 1;
pub const GHOSTTY_SPLIT_DIRECTION_LEFT: ghostty_action_split_direction_e = 2;
pub const GHOSTTY_SPLIT_DIRECTION_UP: ghostty_action_split_direction_e = 3;

pub type ghostty_action_goto_split_e = u32;
pub const GHOSTTY_GOTO_SPLIT_PREVIOUS: ghostty_action_goto_split_e = 0;
pub const GHOSTTY_GOTO_SPLIT_NEXT: ghostty_action_goto_split_e = 1;
pub const GHOSTTY_GOTO_SPLIT_UP: ghostty_action_goto_split_e = 2;
pub const GHOSTTY_GOTO_SPLIT_LEFT: ghostty_action_goto_split_e = 3;
pub const GHOSTTY_GOTO_SPLIT_DOWN: ghostty_action_goto_split_e = 4;
pub const GHOSTTY_GOTO_SPLIT_RIGHT: ghostty_action_goto_split_e = 5;

pub type ghostty_action_goto_tab_e = i32;
pub const GHOSTTY_GOTO_TAB_PREVIOUS: ghostty_action_goto_tab_e = -1;
pub const GHOSTTY_GOTO_TAB_NEXT: ghostty_action_goto_tab_e = -2;
pub const GHOSTTY_GOTO_TAB_LAST: ghostty_action_goto_tab_e = -3;

pub type ghostty_action_close_tab_mode_e = u32;
pub const GHOSTTY_ACTION_CLOSE_TAB_MODE_THIS: ghostty_action_close_tab_mode_e = 0;
pub const GHOSTTY_ACTION_CLOSE_TAB_MODE_OTHER: ghostty_action_close_tab_mode_e = 1;
pub const GHOSTTY_ACTION_CLOSE_TAB_MODE_RIGHT: ghostty_action_close_tab_mode_e = 2;

pub type ghostty_action_resize_split_direction_e = u32;
pub const GHOSTTY_RESIZE_SPLIT_UP: ghostty_action_resize_split_direction_e = 0;
pub const GHOSTTY_RESIZE_SPLIT_DOWN: ghostty_action_resize_split_direction_e = 1;
pub const GHOSTTY_RESIZE_SPLIT_LEFT: ghostty_action_resize_split_direction_e = 2;
pub const GHOSTTY_RESIZE_SPLIT_RIGHT: ghostty_action_resize_split_direction_e = 3;
