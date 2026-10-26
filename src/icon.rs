//! The app icon (`assets/icon.png`, drawn from `assets/icon.svg`), embedded in the binary.

use image::imageops::FilterType;

const PNG: &[u8] = include_bytes!("../assets/icon.png");

/// The icon as RGBA, scaled to `side` x `side` pixels; `None` if it can't be decoded.
pub fn rgba(side: u32) -> Option<(u32, Vec<u8>)> {
    let img = image::load_from_memory(PNG).ok()?;
    let img = if img.width() == side {
        img
    } else {
        img.resize_exact(side, side, FilterType::Lanczos3)
    };
    Some((side, img.to_rgba8().into_raw()))
}

/// For the window title bar / task switcher.
pub fn window(side: u32) -> Option<eframe::egui::IconData> {
    let (side, rgba) = rgba(side)?;
    Some(eframe::egui::IconData {
        rgba,
        width: side,
        height: side,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_at_any_size() {
        for side in [32, 64, 256] {
            let (w, px) = super::rgba(side).expect("icon decodes");
            assert_eq!((w, px.len()), (side, (side * side * 4) as usize));
            assert!(px.chunks(4).any(|p| p[3] > 0), "not blank");
        }
    }
}
