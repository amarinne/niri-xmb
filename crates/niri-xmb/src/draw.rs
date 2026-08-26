//! Tiny-skia drawing primitives for niri-xmb.

use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Paint, PathBuilder, Pixmap,
    Point, RadialGradient, Rect, SpreadMode, Stroke, Transform,
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

/// Fill a solid rectangle.
pub fn fill_rect(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, color: Color, scale: f32) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let Some(rect) = Rect::from_xywh(x, y, w, h) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = false;
    pixmap.fill_rect(rect, &paint, Transform::from_scale(scale, scale), None);
}

/// Fill a rounded rectangle.
#[allow(clippy::too_many_arguments)]
pub fn fill_rounded_rect(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    color: Color,
    scale: f32,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    if r <= 0.0 || w < r * 2.0 || h < r * 2.0 {
        fill_rect(pixmap, x, y, w, h, color, scale);
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
            Transform::from_scale(scale, scale),
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
    scale: f32,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    if top == bottom {
        fill_rounded_rect(pixmap, x, y, w, h, r, top, scale);
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
        fill_rounded_rect(pixmap, x, y, w, h, r, top, scale);
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
        Transform::from_scale(scale, scale),
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
    scale: f32,
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
        pixmap.stroke_path(
            &path,
            &paint,
            &stroke,
            Transform::from_scale(scale, scale),
            None,
        );
    }
}

/// Draw a filled circle.
pub fn fill_circle(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, color: Color, scale: f32) {
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
            Transform::from_scale(scale, scale),
            None,
        );
    }
}

/// Draw a soft circular glow that fades to transparent at `radius`.
pub fn fill_radial_glow(
    pixmap: &mut Pixmap,
    cx: f32,
    cy: f32,
    radius: f32,
    color: Color,
    scale: f32,
) {
    if radius <= 0.0 || color.alpha() <= 0.0 {
        return;
    }
    let mut middle = color;
    middle.set_alpha(color.alpha() * 0.65);
    let mut edge = color;
    edge.set_alpha(0.0);
    let center = Point::from_xy(cx, cy);
    let Some(shader) = RadialGradient::new(
        center,
        center,
        radius,
        vec![
            GradientStop::new(0.0, color),
            GradientStop::new(0.55, middle),
            GradientStop::new(1.0, edge),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) else {
        return;
    };
    let mut path = PathBuilder::new();
    path.push_circle(cx, cy, radius);
    let Some(path) = path.finish() else {
        return;
    };
    pixmap.fill_path(
        &path,
        &Paint {
            shader,
            anti_alias: true,
            ..Paint::default()
        },
        FillRule::Winding,
        Transform::from_scale(scale, scale),
        None,
    );
}

fn sample_bilinear_rgba(rgba: &[u8], src_w: u32, src_h: u32, x: f32, y: f32) -> [u8; 4] {
    let x = x.clamp(0.0, src_w.saturating_sub(1) as f32);
    let y = y.clamp(0.0, src_h.saturating_sub(1) as f32);
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(src_w - 1);
    let y1 = (y0 + 1).min(src_h - 1);
    let tx = x - x0 as f32;
    let ty = y - y0 as f32;
    let samples = [
        (x0, y0, (1.0 - tx) * (1.0 - ty)),
        (x1, y0, tx * (1.0 - ty)),
        (x0, y1, (1.0 - tx) * ty),
        (x1, y1, tx * ty),
    ];

    let mut alpha = 0.0;
    let mut premultiplied = [0.0; 3];
    for (sx, sy, weight) in samples {
        let index = ((sy * src_w + sx) * 4) as usize;
        if index + 3 >= rgba.len() {
            continue;
        }
        let sample_alpha = rgba[index + 3] as f32 / 255.0;
        alpha += sample_alpha * weight;
        for channel in 0..3 {
            premultiplied[channel] += rgba[index + channel] as f32 / 255.0 * sample_alpha * weight;
        }
    }

    if alpha <= f32::EPSILON {
        return [0, 0, 0, 0];
    }
    [
        (premultiplied[0] / alpha * 255.0).round() as u8,
        (premultiplied[1] / alpha * 255.0).round() as u8,
        (premultiplied[2] / alpha * 255.0).round() as u8,
        (alpha * 255.0).round() as u8,
    ]
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

            let source_x = (dx as f32 + 0.5) * src_w as f32 / size as f32 - 0.5;
            let source_y = (dy as f32 + 0.5) * src_h as f32 / size as f32 - 0.5;
            let [sr, sg, sb, sa] = sample_bilinear_rgba(rgba, src_w, src_h, source_x, source_y);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bilinear_sampling_blends_high_resolution_pixels() {
        let rgba = [255, 0, 0, 255, 0, 0, 255, 255];

        let [red, green, blue, alpha] = sample_bilinear_rgba(&rgba, 2, 1, 0.5, 0.0);

        assert!((127..=128).contains(&red));
        assert_eq!(green, 0);
        assert!((127..=128).contains(&blue));
        assert_eq!(alpha, 255);
    }

    #[test]
    fn radial_glow_fades_toward_its_edge() {
        let mut pixmap = Pixmap::new(25, 25).unwrap();
        fill_radial_glow(
            &mut pixmap,
            12.0,
            12.0,
            10.0,
            Color::from_rgba8(255, 0, 0, 128),
            1.0,
        );

        let center = pixmap.pixel(12, 12).unwrap().alpha();
        let middle = pixmap.pixel(17, 12).unwrap().alpha();
        let edge = pixmap.pixel(21, 12).unwrap().alpha();
        assert!(center > middle);
        assert!(middle > edge);
    }
}
