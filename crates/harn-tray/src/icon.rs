//! The tray icon: a coloured dot drawn in code, so there are no image files to ship.

use anyhow::{Context, Result};
use tray_icon::Icon;

use crate::state::Colour;

const SIZE: u32 = 32;
/// Leaves a transparent margin so the dot does not touch the edges of the tray slot.
const RADIUS: f32 = 13.0;
const BYTES_PER_PIXEL: usize = 4;
const OPAQUE: f32 = 255.0;

fn rgb(colour: Colour) -> [u8; 3] {
    match colour {
        Colour::Green => [46, 160, 67],
        Colour::Grey => [139, 148, 158],
        Colour::Amber => [227, 160, 8],
        Colour::Red => [218, 54, 51],
    }
}

/// A filled circle whose alpha is how much of each pixel the disc covers, which anti-aliases the rim.
pub fn rgba(colour: Colour) -> Vec<u8> {
    let [r, g, b] = rgb(colour);
    let centre = SIZE as f32 / 2.0;
    let side = SIZE as usize;
    let mut pixels = Vec::with_capacity(side * side * BYTES_PER_PIXEL);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 + 0.5 - centre;
            let dy = y as f32 + 0.5 - centre;
            let coverage = (RADIUS + 0.5 - dx.hypot(dy)).clamp(0.0, 1.0);
            pixels.extend_from_slice(&[r, g, b, (coverage * OPAQUE).round() as u8]);
        }
    }
    pixels
}

pub fn icon(colour: Colour) -> Result<Icon> {
    Icon::from_rgba(rgba(colour), SIZE, SIZE).context("cannot build the tray icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(pixels: &[u8], x: u32, y: u32) -> &[u8] {
        let at = (y * SIZE + x) as usize * BYTES_PER_PIXEL;
        &pixels[at..at + BYTES_PER_PIXEL]
    }

    #[test]
    fn draws_an_opaque_dot_with_soft_edges_on_a_clear_background() {
        let pixels = rgba(Colour::Green);
        assert_eq!(pixels.len(), (SIZE * SIZE) as usize * BYTES_PER_PIXEL);
        assert_eq!(pixel(&pixels, SIZE / 2, SIZE / 2), [46, 160, 67, 255]);
        assert_eq!(pixel(&pixels, 0, 0)[3], 0);
        let partial = pixels
            .chunks(BYTES_PER_PIXEL)
            .filter(|p| p[3] > 0 && p[3] < 255)
            .count();
        assert!(partial > 0, "rim is not anti-aliased");
    }

    #[test]
    fn every_colour_is_distinct_and_builds_an_icon() {
        let colours = [Colour::Green, Colour::Grey, Colour::Amber, Colour::Red];
        for (i, a) in colours.iter().enumerate() {
            assert!(icon(*a).is_ok());
            for b in &colours[i + 1..] {
                assert_ne!(rgb(*a), rgb(*b));
            }
        }
    }
}
