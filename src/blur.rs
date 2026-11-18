//! Turning a raw screen capture into a blurred backdrop.
//!
//! The work is done at a fraction of the screen resolution: downscaling is
//! itself a box filter, the upscale back to full size is done by the renderer
//! when the texture is drawn, and three box passes in between approximate a
//! Gaussian. Turning a rotated or flipped output's buffer upright costs
//! nothing extra: the mirrors are folded into the downscale's reads and the
//! transposition into the blur's.

use crate::capture::Frame;

/// Longest edge of the working image. Small enough to be cheap, large enough
/// that the upscale does not show blocking.
const WORKING_EDGE: u32 = 480;

/// Box passes per axis. Three approximate a Gaussian of the same radius.
const PASSES: usize = 3;

/// The mirrors and the transposition that turn a captured buffer into the
/// picture on its output — any of the eight rotations and flips is some
/// combination of the three. The mirrors apply to the buffer as captured, the
/// transposition after them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Orientation {
    pub flip_x: bool,
    pub flip_y: bool,
    pub transpose: bool,
}

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
    let transpose = frame.orientation.transpose;
    if radius > 0 {
        box_blur(&mut image, (radius / factor).max(1), transpose);
    } else if transpose {
        let mut scratch = vec![0; image.rgb.len()];
        transpose_into(&mut image, &mut scratch);
    }
    Some(image)
}

fn scale_factor(width: u32, height: u32) -> u32 {
    let longest = width.max(height);
    longest.div_ceil(WORKING_EDGE).max(1)
}

/// Division by a divisor fixed for a whole image, as a multiply and a shift.
/// The blur divides once per channel per pixel per pass, and a hardware
/// divide there is most of what the blur costs.
#[derive(Clone, Copy)]
struct Divisor {
    reciprocal: u64,
}

impl Divisor {
    fn new(divisor: u32) -> Self {
        Self { reciprocal: (1u64 << 32).div_ceil(divisor as u64) }
    }

    /// `n / divisor`, exact for every `n` this module produces: the
    /// reciprocal overshoots by less than one part in 2^32, which cannot carry
    /// a quotient over the next integer while `n` stays below 2^32 / divisor
    /// — the largest window sum is under 2^17, with a divisor under 2^9.
    fn divide(self, n: u32) -> u32 {
        ((n as u64 * self.reciprocal) >> 32) as u32
    }
}

/// Average each `factor` x `factor` block down to one RGB pixel, sampling
/// every `max(factor / 2, 1)`-th pixel on both axes — a 2x2 grid inside the
/// block for an even `factor`, 3x3 for an odd one. Reading every source pixel
/// is the single most expensive thing the launcher does on a slow CPU, and the
/// difference never survives three box passes.
///
/// The frame's mirrors are applied here, by reading the blocks in reverse,
/// which is free; its transposition is left to the blur.
fn downscale(frame: &Frame, factor: u32) -> Option<Image> {
    let width = (frame.width / factor) as usize;
    let height = (frame.height / factor) as usize;
    if width == 0 || height == 0 {
        return None;
    }
    // Checked once here rather than per sample: every block below lies inside
    // `frame.width` x `frame.height`, so a buffer that holds that many rows of
    // `stride` bytes, each wide enough for `frame.width` pixels, holds every
    // sample. A frame that does not is not worth a partial backdrop.
    let stride = frame.stride as usize;
    let row_bytes = frame.width as usize * 4;
    let data: &[u8] = &frame.data;
    if stride < row_bytes || data.len() < stride * frame.height as usize {
        return None;
    }

    let [r_shift, g_shift, b_shift] = frame.channels;
    let Orientation { flip_x, flip_y, .. } = frame.orientation;
    let factor = factor as usize;
    let step = (factor / 2).max(1);
    let offsets: Vec<usize> = (0..factor).step_by(step).collect();
    let count = (offsets.len() * offsets.len()) as u32;
    let samples = Divisor::new(count);
    // Rounded, like the blur passes, rather than losing up to a level per
    // pixel before they even start.
    let half = count / 2;
    let mut rgb = Vec::with_capacity(width * height * 3);
    // The sampled rows of one block row, as pixels: reused from one block row
    // to the next rather than allocated for each.
    let mut rows: Vec<&[[u8; 4]]> = Vec::with_capacity(offsets.len());

    for y in 0..height {
        let block_y = if flip_y { height - 1 - y } else { y };
        rows.clear();
        rows.extend(offsets.iter().map(|dy| {
            let start = (block_y * factor + dy) * stride;
            data[start..start + row_bytes].as_chunks::<4>().0
        }));
        for x in 0..width {
            let block_x = if flip_x { width - 1 - x } else { x };
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            for row in &rows {
                for dx in &offsets {
                    let pixel = u32::from_le_bytes(row[block_x * factor + dx]);
                    r += (pixel >> r_shift) & 0xff;
                    g += (pixel >> g_shift) & 0xff;
                    b += (pixel >> b_shift) & 0xff;
                }
            }
            rgb.extend([r, g, b].map(|sum| samples.divide(sum + half) as u8));
        }
    }
    Some(Image { width: width as u32, height: height as u32, rgb })
}

/// `PASSES` box blurs of `radius` on each axis, leaving the result
/// transposed if `transposed` asks for it.
///
/// Box passes along the two axes commute, so all the horizontal passes run
/// back to back and one transpose turns the columns into rows for the vertical
/// ones: two transposes in all rather than two per pass, and two buffers
/// traded back and forth rather than a fresh one for every step. The blur is
/// the same along both axes, so an image that has to end up transposed simply
/// skips the second one.
fn box_blur(image: &mut Image, radius: u32, transposed: bool) {
    let mut scratch = vec![0u8; image.rgb.len()];
    blur_rows(image, &mut scratch, radius);
    transpose_into(image, &mut scratch);
    blur_rows(image, &mut scratch, radius);
    if !transposed {
        transpose_into(image, &mut scratch);
    }
}

/// `PASSES` box passes along every row, edges clamped to the outermost pixel.
fn blur_rows(image: &mut Image, scratch: &mut Vec<u8>, radius: u32) {
    let row_len = image.width as usize * 3;
    if image.width <= 1 {
        return;
    }
    let window = Divisor::new(radius * 2 + 1);
    for _ in 0..PASSES {
        for (src, dst) in image.rgb.chunks_exact(row_len).zip(scratch.chunks_exact_mut(row_len)) {
            blur_row(src, dst, radius as usize, window);
        }
        std::mem::swap(&mut image.rgb, scratch);
    }
}

/// One box pass over one row of RGB pixels: a running sum over the window,
/// which costs the same whatever the radius. The three channels advance
/// together, so their sums are independent chains the CPU can overlap.
fn blur_row(src: &[u8], dst: &mut [u8], radius: usize, window: Divisor) {
    let last = src.len() / 3 - 1;
    let at = |x: usize| {
        let i = x.min(last) * 3;
        [src[i] as u32, src[i + 1] as u32, src[i + 2] as u32]
    };
    // Rounded rather than truncated: truncating loses half a level on average
    // on every pass, and with six of them the whole backdrop comes out
    // visibly darker than the desktop it was taken from.
    let half = radius as u32;

    // The window centred on the first pixel reaches `radius` pixels past the
    // left edge, each of them a copy of the first.
    let mut sum = at(0).map(|c| c * (radius as u32 + 1));
    for x in 1..=radius {
        let pixel = at(x);
        for c in 0..3 {
            sum[c] += pixel[c];
        }
    }
    for (x, out) in dst.as_chunks_mut::<3>().0.iter_mut().enumerate() {
        let entering = at(x + radius + 1);
        let leaving = at(x.saturating_sub(radius));
        for c in 0..3 {
            out[c] = window.divide(sum[c] + half) as u8;
            sum[c] = sum[c] + entering[c] - leaving[c];
        }
    }
}

/// Transpose `image`, through `scratch`, which must be as large as it is and
/// is left holding the old pixels.
fn transpose_into(image: &mut Image, scratch: &mut Vec<u8>) {
    let (width, height) = (image.width as usize, image.height as usize);
    for y in 0..height {
        for x in 0..width {
            let from = (y * width + x) * 3;
            let to = (x * height + y) * 3;
            scratch[to..to + 3].copy_from_slice(&image.rgb[from..from + 3]);
        }
    }
    std::mem::swap(&mut image.rgb, scratch);
    image.width = height as u32;
    image.height = width as u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, color: [u8; 3]) -> Image {
        Image { width, height, rgb: color.repeat((width * height) as usize) }
    }

    /// An XRGB8888 frame, the format nearly every compositor hands over.
    fn xrgb_frame(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Frame {
        let mut data = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let [r, g, b] = pixel(x, y);
                data.extend_from_slice(&(u32::from_be_bytes([0, r, g, b])).to_le_bytes());
            }
        }
        Frame {
            connector: None,
            width,
            height,
            stride: width * 4,
            channels: [16, 8, 0],
            orientation: Orientation::default(),
            data: data.into(),
        }
    }

    fn grey(value: u8) -> [u8; 3] {
        [value; 3]
    }

    const RED: [u8; 3] = [255, 0, 0];
    const GREEN: [u8; 3] = [0, 255, 0];
    const BLUE: [u8; 3] = [0, 0, 255];
    const WHITE: [u8; 3] = [255, 255, 255];

    fn corners(image: &Image) -> [[u8; 3]; 4] {
        let (w, h) = (image.width as usize, image.height as usize);
        let at = |x: usize, y: usize| {
            let i = (y * w + x) * 3;
            [image.rgb[i], image.rgb[i + 1], image.rgb[i + 2]]
        };
        [at(0, 0), at(w - 1, 0), at(0, h - 1), at(w - 1, h - 1)]
    }

    #[test]
    fn division_by_reciprocal_is_exact() {
        for divisor in [1, 3, 4, 9, 11, 65, 401] {
            let by = Divisor::new(divisor);
            for n in (0..=255 * divisor + divisor).step_by(7).chain([255 * divisor]) {
                assert_eq!(by.divide(n), n / divisor, "{n} / {divisor}");
            }
        }
    }

    #[test]
    fn blurring_a_flat_image_changes_nothing() {
        let mut image = solid(16, 9, [10, 200, 30]);
        box_blur(&mut image, 3, false);
        assert!(image.rgb.chunks(3).all(|p| p == [10, 200, 30]));
        assert_eq!((image.width, image.height), (16, 9));
    }

    #[test]
    fn blurring_spreads_a_single_bright_pixel() {
        let mut image = solid(9, 9, [0, 0, 0]);
        let centre = ((4 * 9 + 4) * 3) as usize;
        image.rgb[centre..centre + 3].copy_from_slice(&[255, 255, 255]);
        box_blur(&mut image, 1, false);

        let at = |x: usize, y: usize| image.rgb[(y * 9 + x) * 3];
        assert!(at(4, 4) < 255, "centre should have been averaged down");
        assert!(at(3, 4) > 0, "neighbour should have picked up light");
        assert_eq!(at(0, 0), 0, "a corner outside the radius stays dark");
    }

    #[test]
    fn blurring_does_not_darken() {
        // Columns alternating 100 and 101: every window sum lands between two
        // levels, so truncating each pass would flatten the whole image to 100
        // and lose half a level of brightness. Rounding keeps the mean.
        let mut image = Image {
            width: 32,
            height: 8,
            rgb: (0..32 * 8).flat_map(|i| [100 + (i % 2) as u8; 3]).collect(),
        };
        let mean = |image: &Image| {
            image.rgb.iter().map(|&c| c as f64).sum::<f64>() / image.rgb.len() as f64
        };
        let before = mean(&image);
        box_blur(&mut image, 1, false);
        assert!((mean(&image) - before).abs() < 0.1, "{before} -> {}", mean(&image));
    }

    #[test]
    fn transpose_round_trips() {
        let mut image = Image { width: 2, height: 3, rgb: (0..18).collect() };
        let mut scratch = vec![0; 18];
        let original = image.rgb.clone();
        transpose_into(&mut image, &mut scratch);
        assert_eq!((image.width, image.height), (3, 2));
        transpose_into(&mut image, &mut scratch);
        assert_eq!((image.width, image.height), (2, 3));
        assert_eq!(image.rgb, original);
    }

    #[test]
    fn downscale_averages_blocks() {
        // 4x4 XRGB frame: left half black, right half white.
        let frame = xrgb_frame(4, 4, |x, _| grey(if x < 2 { 0 } else { 255 }));
        let image = downscale(&frame, 2).unwrap();
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(image.rgb[0], 0);
        assert_eq!(image.rgb[3], 255);
    }

    #[test]
    fn downscale_rejects_a_buffer_shorter_than_its_stride_claims() {
        let mut frame = xrgb_frame(4, 4, |_, _| grey(128));
        frame.stride = 32;
        assert!(downscale(&frame, 2).is_none());
    }

    #[test]
    fn channels_are_read_by_bit_offset() {
        // XBGR8888 stores red in the low byte: the same pixels, read with the
        // other format's offsets, must come out with red and blue swapped.
        let mut frame = xrgb_frame(2, 2, |_, _| [200, 100, 50]);
        assert_eq!(downscale(&frame, 2).unwrap().rgb, [200, 100, 50]);
        frame.channels = [0, 8, 16];
        assert_eq!(downscale(&frame, 2).unwrap().rgb, [50, 100, 200]);

        // XRGB2101010: the top eight of each ten bits.
        let pixel = (0x3ffu32 << 20) | (0x200 << 10) | 0x004;
        frame.data = pixel.to_le_bytes().repeat(4).into();
        frame.channels = [22, 12, 2];
        assert_eq!(downscale(&frame, 2).unwrap().rgb, [255, 128, 1]);
    }

    /// The quadrants of an output that shows red top left, green top right,
    /// blue bottom left and white bottom right, as screencopy hands over its
    /// buffer in each transform — observed on a headless wlroots output, where
    /// grim saw the picture the right way up every time. `(top left, top
    /// right, bottom left)`; bottom right is whatever colour is left over.
    #[test]
    fn every_output_transform_comes_out_upright() {
        use wayland_client::protocol::wl_output::Transform;
        let observed = [
            (Transform::Normal, [RED, GREEN, BLUE]),
            (Transform::_90, [GREEN, WHITE, RED]),
            (Transform::_180, [WHITE, BLUE, GREEN]),
            (Transform::_270, [BLUE, RED, WHITE]),
            (Transform::Flipped, [GREEN, RED, WHITE]),
            (Transform::Flipped90, [RED, BLUE, GREEN]),
            (Transform::Flipped180, [BLUE, WHITE, RED]),
            (Transform::Flipped270, [WHITE, GREEN, BLUE]),
        ];
        for (transform, [top_left, top_right, bottom_left]) in observed {
            let bottom_right = [RED, GREEN, BLUE, WHITE]
                .into_iter()
                .find(|c| ![top_left, top_right, bottom_left].contains(c))
                .unwrap();
            // A landscape buffer, small enough to need no downscale.
            let mut frame = xrgb_frame(96, 54, |x, y| match (x < 48, y < 27) {
                (true, true) => top_left,
                (false, true) => top_right,
                (true, false) => bottom_left,
                (false, false) => bottom_right,
            });
            frame.orientation = crate::capture::orientation(transform, false);
            let image = blurred(&frame, 2).unwrap();
            let portrait = matches!(
                transform,
                Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
            );
            let size = if portrait { (54, 96) } else { (96, 54) };
            assert_eq!((image.width, image.height), size, "{transform:?}");
            assert_eq!(corners(&image), [RED, GREEN, BLUE, WHITE], "{transform:?}");
        }
    }

    #[test]
    fn a_y_inverted_frame_is_turned_back_over() {
        use wayland_client::protocol::wl_output::Transform;
        let mut frame = xrgb_frame(4, 4, |_, y| grey(if y < 2 { 0 } else { 255 }));
        frame.orientation = crate::capture::orientation(Transform::Normal, true);
        let image = downscale(&frame, 2).unwrap();
        assert_eq!((image.rgb[0], image.rgb[6]), (255, 0));
    }

    #[test]
    fn a_transposed_frame_is_transposed_without_a_blur_too() {
        let mut frame = xrgb_frame(8, 4, |_, _| grey(9));
        frame.orientation.transpose = true;
        let image = blurred(&frame, 0).unwrap();
        assert_eq!((image.width, image.height), (4, 8));
    }
}
