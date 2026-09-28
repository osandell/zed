//! # UI – Zed UI Primitives & Components
//!
//! This crate provides a set of UI primitives and components that are used to build all of the elements in Zed's UI.
//!
//! ## Related Crates:
//!
//! - [`ui_macros`] - proc_macros support for this crate
//! - `ui_input` - the single line input component

pub mod component_prelude;
mod components;
pub mod prelude;
mod styles;
mod traits;
pub mod utils;
mod winman;
mod winman_skin;

pub use components::*;
pub use prelude::*;
pub use styles::*;
pub use traits::animation_ext::*;
pub use winman::*;
pub use winman_skin::{
    has_winman_skin, paint_winman_skin, winman_skin_padding, winman_skin_surface,
    winman_skin_surface_variant,
};
