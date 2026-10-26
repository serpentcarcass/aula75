//! What goes on the 128×128 screen: decoded images/GIFs, rendered text and pixel art.
//! Everything here produces RGBA frames for `proto::Animation::from_rgba`.

use crate::effects::Rgb;
use crate::proto::{MAX_FRAMES, SCREEN};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use anyhow::Result;
use image::{AnimationDecoder, ImageFormat, codecs::gif::GifDecoder, imageops};
use std::{io::Cursor, path::Path};

/// RGBA 128×128 pixels and the frame delay in ms.
pub type Frame = (Vec<u8>, u32);

/// Decodes an image/GIF into 128×128 RGBA frames. Untrusted input: only regular files up to
/// 32 MiB, decoder limits on canvas size and memory, frames resized one at a time.
pub fn load_frames(path: &Path) -> Result<Vec<Frame>> {
    let meta = std::fs::metadata(path)?;
    anyhow::ensure!(
        meta.is_file() && meta.len() <= 32 << 20,
        "not a regular file under 32 MiB"
    );
    let bytes = std::fs::read(path)?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(256 << 20);
    let n = SCREEN as u32;
    let shrink = |f: image::Frame| {
        let (num, den) = f.delay().numer_denom_ms();
        let ms = num / den.max(1);
        let px = imageops::resize(f.buffer(), n, n, imageops::FilterType::Triangle).into_raw();
        (px, if ms <= 10 { 100 } else { ms }) // browsers treat 0–10 ms as 100 ms
    };
    let frames: Vec<_> = if image::guess_format(&bytes)? == ImageFormat::Gif {
        let mut dec = GifDecoder::new(Cursor::new(&bytes))?;
        image::ImageDecoder::set_limits(&mut dec, limits)?;
        dec.into_frames()
            .take(MAX_FRAMES)
            .map(|f| f.map(shrink))
            .collect::<Result<_, _>>()?
    } else {
        let mut r = image::ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?;
        r.limits(limits);
        vec![shrink(image::Frame::new(r.decode()?.to_rgba8()))]
    };
    anyhow::ensure!(!frames.is_empty(), "image has no frames");
    Ok(frames)
}

const SCROLL_DELAY_MS: u32 = 40;
const MAX_SCROLL_STEP: f32 = 8.0;

/// Renders `text` in Hack (egui's bundled monospace font). Static: each line centred.
/// `scroll`: one line moving right to left, never more than the screen's 255 frames.
pub fn text_frames(text: &str, size: f32, fg: Rgb, bg: Rgb, scroll: bool) -> Vec<Frame> {
    let font = FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).expect("bundled font");
    let sf = font.as_scaled(PxScale::from(size));
    let width = |s: &str| s.chars().map(|c| sf.h_advance(sf.glyph_id(c))).sum::<f32>();
    let n = SCREEN as f32;
    if !scroll {
        let lines: Vec<&str> = text.lines().collect();
        let block = sf.height() * lines.len() as f32;
        let mut img = canvas(bg);
        for (i, line) in lines.iter().enumerate() {
            let baseline = (n - block) / 2.0 + sf.ascent() + sf.height() * i as f32;
            draw(&mut img, &font, size, line, (n - width(line)) / 2.0, baseline, fg);
        }
        return vec![(img, 1000)];
    }
    // At most MAX_SCROLL_STEP px per frame within 255 frames, else it's unreadable: cut the tail.
    let budget = MAX_FRAMES as f32 * MAX_SCROLL_STEP - n;
    let mut used = 0.0;
    let line: String = text
        .lines()
        .collect::<Vec<_>>()
        .join("   ")
        .chars()
        .take_while(|&c| {
            used += sf.h_advance(sf.glyph_id(c));
            used <= budget
        })
        .collect();
    let span = width(&line) + n; // enters on the right, leaves on the left
    let step = (span / MAX_FRAMES as f32).ceil().max(2.0);
    let baseline = (n - sf.height()) / 2.0 + sf.ascent();
    (0..(span / step) as usize)
        .map(|i| {
            let mut img = canvas(bg);
            draw(&mut img, &font, size, &line, n - i as f32 * step, baseline, fg);
            (img, SCROLL_DELAY_MS)
        })
        .collect()
}

/// A `side`×`side` grid of cells scaled up to the full screen.
pub fn pixel_frame(cells: &[Rgb], side: usize) -> Frame {
    let mut img = Vec::with_capacity(SCREEN * SCREEN * 4);
    for y in 0..SCREEN {
        for x in 0..SCREEN {
            let [r, g, b] = cells[(y * side / SCREEN) * side + x * side / SCREEN];
            img.extend_from_slice(&[r, g, b, 255]);
        }
    }
    (img, 1000)
}

fn canvas([r, g, b]: Rgb) -> Vec<u8> {
    [r, g, b, 255].repeat(SCREEN * SCREEN)
}

/// Draws one line with anti-aliasing (coverage blends `fg` over what's there); clips to the screen.
fn draw(img: &mut [u8], font: &FontRef, size: f32, text: &str, x: f32, baseline: f32, fg: Rgb) {
    let sf = font.as_scaled(PxScale::from(size));
    let mut caret = x;
    for c in text.chars() {
        let id = sf.glyph_id(c);
        let advance = sf.h_advance(id);
        if caret > SCREEN as f32 {
            break; // the rest is off the right edge
        }
        if caret + advance < 0.0 {
            caret += advance; // still off the left edge: skip rasterising
            continue;
        }
        let glyph = id.with_scale_and_position(PxScale::from(size), point(caret, baseline));
        caret += advance;
        let Some(out) = font.outline_glyph(glyph) else { continue };
        let b = out.px_bounds();
        out.draw(|gx, gy, cov| {
            let (px, py) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
            if (0..SCREEN as i32).contains(&px) && (0..SCREEN as i32).contains(&py) {
                let o = (py as usize * SCREEN + px as usize) * 4;
                for ch in 0..3 {
                    let under = img[o + ch] as f32;
                    img[o + ch] = (under + (fg[ch] as f32 - under) * cov.clamp(0.0, 1.0)).round() as u8;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generators() {
        let lit = |f: &Frame| f.0.chunks(4).filter(|p| p[0] > 128).count();
        let still = text_frames("Hi", 40.0, [255, 0, 0], [0, 0, 0], false);
        assert_eq!(still.len(), 1);
        assert_eq!(still[0].0.len(), SCREEN * SCREEN * 4);
        assert!(lit(&still[0]) > 50, "text is drawn");
        let long = "a very long marquee line that would need far too many frames at two px each ".repeat(20);
        let scroll = text_frames(&long, 30.0, [255; 3], [0; 3], true);
        assert!((1..=MAX_FRAMES).contains(&scroll.len()), "{} frames", scroll.len());
        let t = std::time::Instant::now();
        text_frames(&"x".repeat(20_000), 96.0, [255; 3], [0; 3], true);
        assert!(
            t.elapsed().as_secs() < 5,
            "huge text stays fast (off-screen glyphs skipped, tail cut)"
        );
        assert!(
            text_frames("", 30.0, [255; 3], [0; 3], false)[0]
                .0
                .iter()
                .step_by(4)
                .all(|&r| r == 0)
        );

        let mut cells = vec![[0u8; 3]; 32 * 32];
        cells[0] = [9, 8, 7];
        let (px, _) = pixel_frame(&cells, 32);
        assert_eq!(&px[..4], &[9, 8, 7, 255]);
        assert_eq!(&px[3 * 4..4 * 4], &[9, 8, 7, 255], "cell is 4 px wide");
        assert_eq!(&px[4 * 4..5 * 4], &[0, 0, 0, 255]);
    }
}
