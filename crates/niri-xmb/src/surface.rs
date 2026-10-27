//! Geometry shared by the Wayland surface manager and the renderer.
//!
//! Kept out of `wayland` so the popup renderer can be used without a Wayland
//! connection (the config studio preview does exactly that).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    Bar(usize),
    Popup,
}

#[derive(Debug, Clone)]
pub struct SurfaceInfo {
    pub id: usize,
    pub kind: SurfaceKind,
    pub output: String,
    pub global_x: f64,
    pub global_y: f64,
    pub width: u32,
    pub height: u32,
    pub configured: bool,
    pub frame_ready: bool,
    pub needs_full_redraw: bool,
    pub scale_factor: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl DamageRect {
    pub(crate) fn right(self) -> u32 {
        self.x + self.width
    }

    pub(crate) fn bottom(self) -> u32 {
        self.y + self.height
    }

    pub fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }
}
