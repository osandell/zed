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
mod arcoscope;
mod arcoscope_skin;

pub use components::*;
pub use prelude::*;
pub use styles::*;
pub use traits::animation_ext::*;
pub use arcoscope::*;
pub use arcoscope_skin::{
    has_arcoscope_skin, paint_arcoscope_skin, arcoscope_skin_padding, arcoscope_skin_surface,
    arcoscope_skin_surface_variant,
};
