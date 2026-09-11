//! Turning a raw screen capture into a blurred backdrop.
//!
//! The work is done at a fraction of the screen resolution: downscaling is
//! itself a box filter, the upscale back to full size is done by the GPU when
//! the texture is drawn, and three box passes in between approximate a
//! Gaussian. A 2560x1440 capture costs a couple of milliseconds this way.

use crate::capture::Frame;

/// Longest edge of the working image. Small enough to be cheap, large enough
/// that the upscale does not show blocking.
const WORKING_EDGE: u32 = 480;

pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGB, three bytes per pixel.
    pub rgb: Vec<u8>,
}

/// Downscale `frame`, blur it, and hand back something ready to upload as a
/// texture. `radius` is expressed in full resolution pixels.
pub fn blurred(frame: &Frame, radius: u32) -> Option<Image> {
    let factor = scale_factor(frame.width, frame.height);
    let mut image = downscale(frame, factor)?;
    if radius > 0 {
        // Three box passes ~= one Gaussian of the same radius.
        let scaled = (radius / factor).max(1);
        for _ in 0..3 {
            box_blur(&mut image, scaled);
        }
    }
    Some(image)
}

fn scale_factor(width: u32, height: u32) -> u32 {
    let longest = width.max(height);
    longest.div_ceil(WORKING_EDGE).max(1)
}

/// Average each `factor` x `factor` block down to one RGB pixel, sampling at
/// most a 2x2 grid inside the block. Reading every source pixel is the single
/// most expensive thing the launcher does on a slow CPU, and the difference
/// never survives three box passes.
fn downscale(frame: &Frame, factor: u32) -> Option<Image> {
    let width = frame.width / factor;
    let height = frame.height / factor;
    if width == 0 || height == 0 {
        return None;
    }
    let (r_offset, g_offset, b_offset) = frame.channels;
    let step = (factor / 2).max(1);
    let offsets: Vec<u32> = (0..factor).step_by(step as usize).collect();
    let samples = (offsets.len() * offsets.len()) as u32;
    let mut rgb = vec![0u8; (width * height * 3) as usize];

    for y in 0..height {
        for x in 0..width {
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            for dy in &offsets {
                let row = ((y * factor + dy) * frame.stride) as usize;
                for dx in &offsets {
                    let pixel = row + ((x * factor + dx) * 4) as usize;
                    // A short final row cannot happen with a whole-block loop,
                    // but a lying stride would, so stay defensive.
                    let Some(chunk) = frame.data.get(pixel..pixel + 4) else { continue };
                    r += chunk[r_offset] as u32;
                    g += chunk[g_offset] as u32;
                    b += chunk[b_offset] as u32;
                }
            }
            let out = ((y * width + x) * 3) as usize;
            rgb[out] = (r / samples) as u8;
            rgb[out + 1] = (g / samples) as u8;
            rgb[out + 2] = (b / samples) as u8;
        }
    }
    Some(Image { width, height, rgb })
}

/// Separable box blur: one horizontal pass, one vertical pass.
fn box_blur(image: &mut Image, radius: u32) {
    blur_horizontal(image, radius);
    transpose(image);
    blur_horizontal(image, radius);
    transpose(image);
}

fn blur_horizontal(image: &mut Image, radius: u32) {
    let width = image.width as i64;
    let radius = radius as i64;
    if width <= 1 {
        return;
    }
    let mut out = vec![0u8; image.rgb.len()];

    for y in 0..image.height as i64 {
        let row = (y * width * 3) as usize;
        // Running sum over the window, edges clamped to the outermost pixel.
        let mut sums = [0i64; 3];
        for x in -radius..=radius {
            let clamped = x.clamp(0, width - 1) as usize;
            for (c, sum) in sums.iter_mut().enumerate() {
                *sum += image.rgb[row + clamped * 3 + c] as i64;
            }
        }
        let window = radius * 2 + 1;
        for x in 0..width {
            let leaving = (x - radius).clamp(0, width - 1) as usize;
            let entering = (x + radius + 1).clamp(0, width - 1) as usize;
            for (c, sum) in sums.iter_mut().enumerate() {
                out[row + (x * 3) as usize + c] = (*sum / window) as u8;
                *sum += image.rgb[row + entering * 3 + c] as i64;
                *sum -= image.rgb[row + leaving * 3 + c] as i64;
            }
        }
    }
    image.rgb = out;
}

fn transpose(image: &mut Image) {
    let (width, height) = (image.width as usize, image.height as usize);
    let mut out = vec![0u8; image.rgb.len()];
    for y in 0..height {
        for x in 0..width {
            let from = (y * width + x) * 3;
            let to = (x * height + y) * 3;
            out[to..to + 3].copy_from_slice(&image.rgb[from..from + 3]);
        }
    }
    image.rgb = out;
    image.width = height as u32;
    image.height = width as u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, color: [u8; 3]) -> Image {
        Image { width, height, rgb: color.repeat((width * height) as usize) }
    }

    #[test]
    fn blurring_a_flat_image_changes_nothing() {
        let mut image = solid(16, 9, [10, 200, 30]);
        box_blur(&mut image, 3);
        assert!(image.rgb.chunks(3).all(|p| p == [10, 200, 30]));
        assert_eq!((image.width, image.height), (16, 9));
    }

    #[test]
    fn blurring_spreads_a_single_bright_pixel() {
        let mut image = solid(9, 9, [0, 0, 0]);
        let centre = ((4 * 9 + 4) * 3) as usize;
        image.rgb[centre..centre + 3].copy_from_slice(&[255, 255, 255]);
        box_blur(&mut image, 2);

        let at = |x: usize, y: usize| image.rgb[(y * 9 + x) * 3];
        assert!(at(4, 4) < 255, "centre should have been averaged down");
        assert!(at(3, 4) > 0, "neighbour should have picked up light");
        assert_eq!(at(0, 0), 0, "a corner outside the radius stays dark");
    }

    #[test]
    fn transpose_round_trips() {
        let mut image = Image { width: 2, height: 3, rgb: (0..18).collect() };
        let original = image.rgb.clone();
        transpose(&mut image);
        assert_eq!((image.width, image.height), (3, 2));
        transpose(&mut image);
        assert_eq!((image.width, image.height), (2, 3));
        assert_eq!(image.rgb, original);
    }

    #[test]
    fn downscale_averages_blocks() {
        // 4x4 XRGB frame: left half black, right half white.
        let mut data = Vec::new();
        for _ in 0..4 {
            for x in 0..4 {
                let value = if x < 2 { 0 } else { 255 };
                data.extend_from_slice(&[value, value, value, 255]);
            }
        }
        let frame = Frame {
            connector: None,
            width: 4,
            height: 4,
            stride: 16,
            channels: (2, 1, 0),
            data: data.into(),
        };
        let image = downscale(&frame, 2).unwrap();
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(image.rgb[0], 0);
        assert_eq!(image.rgb[3], 255);
    }
}
