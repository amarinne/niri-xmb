//! Reusable popup rendering for the daemon and the config studio.
//!
//! The daemon binary keeps its own module tree; this library exposes only the
//! parts the config studio needs to draw a real popup frame without a Wayland
//! connection.

pub mod anim;
pub mod config;
pub mod draw;
pub mod icons;
pub mod model;
pub mod preview;
pub mod render;
pub mod surface;
pub mod text;
