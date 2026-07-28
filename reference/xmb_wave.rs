// XMB wave background — CPU port of the classic ShaderToy wave.
//
// NOT COMPILED. Kept for reuse in an animated-wallpaper engine, where a
// full-screen surface is redrawn continuously anyway. It was integrated into
// the niri-xmb popup panel and then removed: the effect is designed for a full
// screen, and in a panel a few hundred pixels tall the ribbons overlap into
// grey smears rather than reading as the XMB wave.
//
// Measured on a 2560x1440 output, panel roughly 600x210, 24 fps:
//   - naive per-pixel powi(15..23):            46% of one core
//   - with the per-band falloff lookup table:  11%
//   - rasterised at 1/3 size and scaled up:    39%  (a non-identity
//     tiny-skia Pattern transform drops onto a slow filtered sampling
//     path per destination pixel, costing far more than the raster saved)
//
// So: tabulate the falloff, and keep the pattern transform a pure translate.
// The residual cost was not the wave at all but re-uploading an output-sized
// layer-shell buffer every frame.
//
// Depends on: tiny_skia::{Pixmap, PremultipliedColorU8} and a hex colour
// parser. WaveConfig/WaveBand definitions are in the git history of
// crates/niri-xmb/src/config.rs, or reconstruct from the field docs below.
//! Animated XMB-style wave background for the popup panel.
//!
//! A CPU port of the classic ShaderToy wave: a vertical gradient with several
//! sine bands drawn over it, each band a soft glowing line whose falloff is
//! sharper on one side. Everything is driven from [`WaveConfig`] so a theme can
//! restyle or reshape it without touching this code.
//!
//! Two properties of the original make it cheap enough to rasterise on the CPU
//! at panel size:
//!
//! - A band's centre height depends only on `x` and time, so the sine is
//!   evaluated once per column rather than once per pixel.
//! - `smoothstep(width * width_factor, 0.0, d)` is exactly zero once `d`
//!   reaches the band width, which bounds each band to a narrow horizontal
//!   strip and lets most pixels skip most bands outright.

use crate::config::{WaveBand, WaveConfig};
use std::time::Instant;
use tiny_skia::{Pixmap, PremultipliedColorU8};
use woven_common::draw::hex_color;

/// Resolution of the falloff lookup table.
const FALLOFF_STEPS: usize = 256;

/// Per-band values that do not vary across the panel.
///
/// The falloff `smoothstep(...)^exponent` depends only on the normalised
/// distance to the band centre, so it is tabulated once per frame rather than
/// evaluated per pixel. That removes a `powi` with an exponent of 15 to 23 from
/// the innermost loop, which dominated the cost of the wave.
struct BandStatic {
    /// Distance at which the band stops contributing at all.
    reach: f32,
    /// Reciprocal of `reach`, to avoid a division per pixel.
    inverse_reach: f32,
    /// Which side of the centre gets the tighter falloff.
    upward: bool,
    red: f32,
    green: f32,
    blue: f32,
    falloff: [f32; FALLOFF_STEPS + 1],
}

impl BandStatic {
    fn new(band: &WaveBand, width_factor: f32) -> Self {
        let reach = (band.width * width_factor).max(f32::EPSILON);
        let exponent = band.exponent.round().max(1.0) as i32;
        let mut falloff = [0.0f32; FALLOFF_STEPS + 1];
        for (index, slot) in falloff.iter_mut().enumerate() {
            let t = index as f32 / FALLOFF_STEPS as f32;
            *slot = (t * t * (3.0 - 2.0 * t)).powi(exponent);
        }
        let color = hex_color(&band.color);
        Self {
            reach,
            inverse_reach: 1.0 / reach,
            upward: band.upward,
            red: color.red(),
            green: color.green(),
            blue: color.blue(),
            falloff,
        }
    }
}

/// A band's centre for one column, paired with its static values.
struct ColumnBand {
    /// Band centre in normalised height, 0.0 at the bottom of the panel.
    centre: f32,
    band: usize,
}

pub struct WaveRenderer {
    /// Rasterised background, reused between frames.
    cache: Option<Pixmap>,
    started: Instant,
    /// When the cache was last redrawn, for rate limiting.
    last_frame: Option<Instant>,
}

impl WaveRenderer {
    pub fn new() -> Self {
        Self {
            cache: None,
            started: Instant::now(),
            last_frame: None,
        }
    }

    /// Whether enough time has passed to be worth redrawing the wave.
    ///
    /// The wave animates continuously, so without a cap it would pull the whole
    /// panel into a redraw on every loop iteration.
    pub fn needs_frame(&self, config: &WaveConfig) -> bool {
        if !config.enabled {
            return false;
        }
        let interval = 1.0 / config.fps.clamp(1, 240) as f32;
        self.last_frame
            .is_none_or(|last| last.elapsed().as_secs_f32() >= interval)
    }

    /// Rasterise the wave at `width` x `height` and return it.
    ///
    /// Deliberately rasterised at full panel size. Drawing smaller and letting
    /// the pattern scale it up was measured to be roughly three times *more*
    /// expensive: the per-pixel raster is cheap once the falloff is tabulated,
    /// while a non-identity pattern transform drops tiny-skia onto a slow
    /// filtered sampling path for every destination pixel.
    ///
    /// Returns `None` when the wave is disabled or the size is degenerate.
    pub fn frame(&mut self, width: u32, height: u32, config: &WaveConfig) -> Option<&Pixmap> {
        if !config.enabled || width == 0 || height == 0 {
            return None;
        }
        let stale = self.needs_frame(config);
        let resized = self
            .cache
            .as_ref()
            .is_none_or(|pixmap| pixmap.width() != width || pixmap.height() != height);
        if resized {
            self.cache = Pixmap::new(width, height);
        }
        if resized || stale {
            let time = self.started.elapsed().as_secs_f32() * config.speed;
            let pixmap = self.cache.as_mut()?;
            paint(pixmap, config, time);
            self.last_frame = Some(Instant::now());
        }
        self.cache.as_ref()
    }
}

/// Draw the gradient and every band into `pixmap`.
fn paint(pixmap: &mut Pixmap, config: &WaveConfig, time: f32) {
    let width = pixmap.width() as usize;
    let height = pixmap.height() as usize;
    let top = hex_color(&config.top);
    let bottom = hex_color(&config.bottom);
    let width_factor = config.width_factor.max(0.0);
    let statics: Vec<BandStatic> = config
        .bands
        .iter()
        .map(|band| BandStatic::new(band, width_factor))
        .collect();

    let inverse_width = 1.0 / width.max(1) as f32;
    let inverse_height = 1.0 / height.max(1) as f32;
    let alpha = config.opacity.clamp(0.0, 1.0);
    let scale_to_byte = alpha * 255.0;
    let alpha_byte = (alpha * 255.0).round() as u8;
    let mut column: Vec<ColumnBand> = Vec::with_capacity(statics.len());
    let pixels = pixmap.pixels_mut();

    for x in 0..width {
        let u = x as f32 * inverse_width;

        // The sine only varies along x, so evaluate each band once per column.
        column.clear();
        for (index, band) in config.bands.iter().enumerate() {
            let angle = -time * band.speed * band.frequency + (band.shift + u) * 2.0;
            column.push(ColumnBand {
                centre: angle.sin() * band.amplitude + band.offset,
                band: index,
            });
        }

        for y in 0..height {
            // The shader's origin is bottom-left; a pixmap's is top-left.
            let v = 1.0 - y as f32 * inverse_height;

            let blend = (u * v * 1.1).clamp(0.0, 1.0);
            let mut red = top.red() + (bottom.red() - top.red()) * blend;
            let mut green = top.green() + (bottom.green() - top.green()) * blend;
            let mut blue = top.blue() + (bottom.blue() - top.blue()) * blend;

            for entry in &column {
                let band = &statics[entry.band];
                let difference = entry.centre - v;
                let mut distance = difference.abs();
                if distance >= band.reach {
                    // Outside the band entirely: the falloff would be zero.
                    continue;
                }
                // One side of the band falls off four times faster, which is
                // what gives the ribbon a lit edge rather than a plain line.
                if (band.upward && difference > 0.0) || (!band.upward && difference < 0.0) {
                    distance *= 4.0;
                    if distance >= band.reach {
                        continue;
                    }
                }
                let t = 1.0 - distance * band.inverse_reach;
                let index = (t * FALLOFF_STEPS as f32) as usize;
                let scale = band.falloff[index.min(FALLOFF_STEPS)];
                red += band.red * scale;
                green += band.green * scale;
                blue += band.blue * scale;
            }

            // The pattern is premultiplied, so scale the channels by alpha.
            let encode = |value: f32| (value.clamp(0.0, 1.0) * scale_to_byte).round() as u8;
            pixels[y * width + x] = PremultipliedColorU8::from_rgba(
                encode(red),
                encode(green),
                encode(blue),
                alpha_byte,
            )
            .unwrap_or(PremultipliedColorU8::TRANSPARENT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> WaveConfig {
        WaveConfig::default()
    }

    #[test]
    fn a_band_only_reaches_as_far_as_its_width() {
        // A band contributes nothing beyond width * width_factor, which is what
        // makes the per-pixel loop cheap. Verified by painting a single wide
        // band and checking a row far from its centre stays at the gradient.
        let mut plain = config();
        plain.bands.clear();
        let mut single = config();
        single.bands.truncate(1);
        single.bands[0].amplitude = 0.0;
        single.bands[0].offset = 0.5;
        single.bands[0].width = 0.02;

        let mut a = Pixmap::new(16, 64).unwrap();
        let mut b = Pixmap::new(16, 64).unwrap();
        paint(&mut a, &plain, 0.0);
        paint(&mut b, &single, 0.0);

        // Row 0 is the top of the panel, far from a band centred at v = 0.5.
        assert_eq!(
            a.pixels()[0],
            b.pixels()[0],
            "band leaked outside its width"
        );
        // The centre row must differ, or the test proves nothing.
        let centre = 32 * 16;
        assert_ne!(
            a.pixels()[centre],
            b.pixels()[centre],
            "band did not draw at its own centre"
        );
    }

    #[test]
    fn opacity_scales_the_output_alpha() {
        let mut opaque = config();
        opaque.opacity = 1.0;
        let mut faint = config();
        faint.opacity = 0.25;

        let mut a = Pixmap::new(8, 8).unwrap();
        let mut b = Pixmap::new(8, 8).unwrap();
        paint(&mut a, &opaque, 0.0);
        paint(&mut b, &faint, 0.0);
        assert_eq!(a.pixels()[0].alpha(), 255);
        assert_eq!(b.pixels()[0].alpha(), 64);
    }

    #[test]
    fn the_wave_moves_over_time() {
        let config = config();
        let mut a = Pixmap::new(24, 24).unwrap();
        let mut b = Pixmap::new(24, 24).unwrap();
        paint(&mut a, &config, 0.0);
        paint(&mut b, &config, 4.0);
        assert_ne!(a.data(), b.data(), "wave should animate");
    }

    #[test]
    fn a_disabled_wave_produces_no_frame() {
        let mut config = config();
        config.enabled = false;
        let mut renderer = WaveRenderer::new();
        assert!(renderer.frame(32, 32, &config).is_none());
        assert!(!renderer.needs_frame(&config));
    }

    #[test]
    fn frames_are_rate_limited() {
        let mut config = config();
        config.fps = 1;
        let mut renderer = WaveRenderer::new();
        assert!(renderer.needs_frame(&config), "first frame is always due");
        renderer.frame(16, 16, &config).expect("a frame");
        assert!(
            !renderer.needs_frame(&config),
            "a second frame must wait for the interval"
        );
    }

    /// Every pixel must be written; a gap would leave the panel showing through.
    #[test]
    fn the_whole_pixmap_is_written() {
        let mut config = config();
        config.opacity = 1.0;
        let mut pixmap = Pixmap::new(20, 20).unwrap();
        paint(&mut pixmap, &config, 1.0);
        assert!(pixmap.pixels().iter().all(|pixel| pixel.alpha() == 255));
    }
}
