//! Shared tiny-skia drawing primitives for woven-shell.

use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Paint, PathBuilder, Pixmap,
    Point, Rect, SpreadMode, Stroke, Transform,
};

/// Parse a hex color string (#RRGGBB or #AARRGGBB) into a tiny-skia Color.
pub fn hex_color(s: &str) -> Color {
    let s = s.trim_start_matches('#');
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).unwrap_or(0);
            let g = u8::from_str_radix(&s[2..4], 16).unwrap_or(0);
            let b = u8::from_str_radix(&s[4..6], 16).unwrap_or(0);
            Color::from_rgba8(r, g, b, 255)
        }
        8 => {
            let a = u8::from_str_radix(&s[0..2], 16).unwrap_or(255);
            let r = u8::from_str_radix(&s[2..4], 16).unwrap_or(0);
            let g = u8::from_str_radix(&s[4..6], 16).unwrap_or(0);
            let b = u8::from_str_radix(&s[6..8], 16).unwrap_or(0);
            Color::from_rgba8(r, g, b, a)
        }
        _ => Color::BLACK,
    }
}

/// Fill the entire pixmap with a solid color.
pub fn clear(pixmap: &mut Pixmap, color: Color) {
    pixmap.fill(color);
}

/// Fill a solid rectangle.
pub fn fill_rect(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, color: Color) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let Some(rect) = Rect::from_xywh(x, y, w, h) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = false;
    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
}

/// Fill a rounded rectangle.
pub fn fill_rounded_rect(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    color: Color,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    if r <= 0.0 || w < r * 2.0 || h < r * 2.0 {
        fill_rect(pixmap, x, y, w, h, color);
        return;
    }
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    if let Some(path) = rounded_rect_path(x, y, w, h, r) {
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Fill a rounded rectangle with a vertical `top` to `bottom` linear gradient.
///
/// Falls back to a flat `top` fill when the two stops are identical or the
/// gradient cannot be built.
#[allow(clippy::too_many_arguments)]
pub fn fill_gradient_rounded_rect(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    top: Color,
    bottom: Color,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    if top == bottom {
        fill_rounded_rect(pixmap, x, y, w, h, r, top);
        return;
    }
    let shader = LinearGradient::new(
        Point::from_xy(x, y),
        Point::from_xy(x, y + h),
        vec![GradientStop::new(0.0, top), GradientStop::new(1.0, bottom)],
        SpreadMode::Pad,
        Transform::identity(),
    );
    let Some(shader) = shader else {
        fill_rounded_rect(pixmap, x, y, w, h, r, top);
        return;
    };
    let Some(path) = rounded_rect_path(x, y, w, h, r.max(0.0).min(w * 0.5).min(h * 0.5)) else {
        return;
    };
    let paint = Paint {
        shader,
        anti_alias: true,
        ..Paint::default()
    };
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

/// Stroke a rounded rectangle outline.
#[allow(clippy::too_many_arguments)]
pub fn stroke_rounded_rect(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    color: Color,
    width: f32,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let stroke = Stroke {
        width,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    if let Some(path) = rounded_rect_path(x, y, w, h, r) {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

/// Draw a filled circle.
pub fn fill_circle(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, color: Color) {
    if r <= 0.0 {
        return;
    }
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Blit pre-decoded RGBA icon pixels into the pixmap at (x, y), scaled to `size×size`.
pub fn blit_icon(
    pixmap: &mut Pixmap,
    x: i32,
    y: i32,
    size: u32,
    rgba: &[u8],
    src_w: u32,
    src_h: u32,
) {
    blit_icon_alpha(pixmap, x, y, size, rgba, src_w, src_h, 1.0);
}

/// Blit an icon with its alpha scaled by `alpha` (0.0..=1.0).
///
/// Dimming unfocused icons is what lets the focused one read as the focal
/// point without drawing a container around it.
#[allow(clippy::too_many_arguments)]
pub fn blit_icon_alpha(
    pixmap: &mut Pixmap,
    x: i32,
    y: i32,
    size: u32,
    rgba: &[u8],
    src_w: u32,
    src_h: u32,
    alpha: f32,
) {
    let alpha = alpha.clamp(0.0, 1.0);
    if alpha <= 0.0 || size == 0 || src_w == 0 || src_h == 0 {
        return;
    }
    let alpha_scale = (alpha * 255.0).round() as u16;
    let pw = pixmap.width() as i32;
    let ph = pixmap.height() as i32;
    let pixels = pixmap.pixels_mut();

    for dy in 0..size as i32 {
        for dx in 0..size as i32 {
            let px = x + dx;
            let py = y + dy;
            if px < 0 || py < 0 || px >= pw || py >= ph {
                continue;
            }

            let sx = (dx as f32 / size as f32 * src_w as f32) as usize;
            let sy = (dy as f32 / size as f32 * src_h as f32) as usize;
            let si = (sy * src_w as usize + sx) * 4;
            if si + 3 >= rgba.len() {
                continue;
            }

            let sr = rgba[si];
            let sg = rgba[si + 1];
            let sb = rgba[si + 2];
            let sa = rgba[si + 3];
            if sa == 0 {
                continue;
            }

            let idx = (py * pw + px) as usize;
            let dst = &mut pixels[idx];
            let src_a = sa as u16 * alpha_scale / 255;
            if src_a == 0 {
                continue;
            }
            let inv_a = 255u16 - src_a;
            let dr = ((sr as u16 * src_a + dst.red() as u16 * inv_a) / 255) as u8;
            let dg = ((sg as u16 * src_a + dst.green() as u16 * inv_a) / 255) as u8;
            let db = ((sb as u16 * src_a + dst.blue() as u16 * inv_a) / 255) as u8;
            let da = src_a.saturating_add(dst.alpha() as u16 * inv_a / 255) as u8;
            *dst = tiny_skia::PremultipliedColorU8::from_rgba(dr, dg, db, da).unwrap_or(*dst);
        }
    }
}

/// Blit RGBA pixels at (x,y) without scaling, clipped to `clip_h` rows.
pub fn blit_thumb(pixmap: &mut Pixmap, x: i32, y: i32, w: u32, h: u32, rgba: &[u8], clip_h: u32) {
    let pw = pixmap.width() as i32;
    let ph = pixmap.height() as i32;
    let pixels = pixmap.pixels_mut();
    let rows = clip_h.min(h) as i32;

    for dy in 0..rows {
        for dx in 0..w as i32 {
            let px = x + dx;
            let py = y + dy;
            if px < 0 || py < 0 || px >= pw || py >= ph {
                continue;
            }
            let si = (dy as usize * w as usize + dx as usize) * 4;
            if si + 3 >= rgba.len() {
                continue;
            }
            let (sr, sg, sb, sa) = (rgba[si], rgba[si + 1], rgba[si + 2], rgba[si + 3]);
            if sa == 0 {
                continue;
            }
            let idx = (py * pw + px) as usize;
            let dst = &mut pixels[idx];
            let src_a = sa as u16;
            let inv_a = 255u16 - src_a;
            let dr = ((sr as u16 * src_a + dst.red() as u16 * inv_a) / 255) as u8;
            let dg = ((sg as u16 * src_a + dst.green() as u16 * inv_a) / 255) as u8;
            let db = ((sb as u16 * src_a + dst.blue() as u16 * inv_a) / 255) as u8;
            let da = src_a.saturating_add(dst.alpha() as u16 * inv_a / 255) as u8;
            *dst = tiny_skia::PremultipliedColorU8::from_rgba(dr, dg, db, da).unwrap_or(*dst);
        }
    }
}

fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}
