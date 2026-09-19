//! Reduces a composited [`Frame`] to a small JPEG for the session drawer.
//!
//! Pure pixel work, no policy: *when* to snapshot and *where* the result goes
//! are decided by the daemon. This module only knows how to turn one BGRA
//! frame into one bounded-size JPEG, cheaply enough to run on the encode
//! thread without anyone noticing.

use jpeg_encoder::{ColorType, Encoder};
use thiserror::Error;

use crate::scene::Frame;

/// Thumbnails are never wider than this. 320 px is a drawer tile at 2-3x
/// density; at JPEG quality 75 a 320x180 thumbnail is ~10-30 KB.
pub const THUMBNAIL_MAX_WIDTH: u32 = 320;
/// Visibly fine for a tile, and about a third the size of quality 90.
const JPEG_QUALITY: u8 = 75;
const BGRA_BYTES_PER_PIXEL: usize = 4;
const RGB_BYTES_PER_PIXEL: usize = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Thumbnail {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Thumbnail {
    /// Encodes an already-downscaled image as a baseline JPEG. Split from
    /// [`thumbnail_from_frame`] so a caller can keep the small RGB image
    /// around and encode it later without keeping the frame it came from.
    pub fn from_rgb(image: &RgbImage) -> Result<Self, ThumbnailError> {
        let width = u16::try_from(image.width).map_err(|_| ThumbnailError::TooLarge)?;
        let height = u16::try_from(image.height).map_err(|_| ThumbnailError::TooLarge)?;
        let mut jpeg = Vec::new();
        Encoder::new(&mut jpeg, JPEG_QUALITY)
            .encode(&image.pixels, width, height, ColorType::Rgb)
            .map_err(|error| ThumbnailError::Encode(error.to_string()))?;
        Ok(Self {
            jpeg,
            width: image.width,
            height: image.height,
        })
    }
}

/// A downscaled frame, alpha dropped, ready for a JPEG encoder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RgbImage {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGB.
    pub pixels: Vec<u8>,
}

#[derive(Debug, Error, PartialEq)]
pub enum ThumbnailError {
    #[error("frame has a zero dimension")]
    EmptyFrame,
    #[error("frame pixel buffer does not match its dimensions")]
    MalformedFrame,
    #[error("downscaled dimensions exceed what JPEG can describe")]
    TooLarge,
    #[error("jpeg encoding failed: {0}")]
    Encode(String),
}

/// Scales `frame` down so it is at most `max_width` wide, keeping its aspect
/// ratio, and converts BGRA to RGB on the way.
///
/// Area averaging: every destination pixel is the mean of the block of
/// source pixels it covers, so fine detail (a text-heavy window, a
/// checkerboard) blends to its average tone instead of aliasing into noise
/// the way nearest-neighbour sampling would. All integer -- one pass over the
/// source, a `u64` accumulator per channel -- so it runs in a couple of
/// milliseconds for a 1080p frame.
///
/// Frames already narrower than `max_width` are not upscaled. A frame with a
/// zero dimension is an error rather than an empty image: nothing downstream
/// can do anything useful with a 0x0 thumbnail, and returning it would only
/// move the check.
pub fn downscale_bgra_to_rgb(frame: &Frame, max_width: u32) -> Result<RgbImage, ThumbnailError> {
    if frame.width == 0 || frame.height == 0 || max_width == 0 {
        return Err(ThumbnailError::EmptyFrame);
    }
    let source_width = frame.width as usize;
    let source_height = frame.height as usize;
    if frame.pixels.len() != source_width * source_height * BGRA_BYTES_PER_PIXEL {
        return Err(ThumbnailError::MalformedFrame);
    }
    let width = source_width.min(max_width as usize);
    // Round to nearest, never below one row. `width <= source_width` keeps
    // this at or below `source_height`, so every destination row still maps
    // to at least one source row below.
    let height = ((source_height * width + source_width / 2) / source_width).max(1);

    let mut pixels = Vec::with_capacity(width * height * RGB_BYTES_PER_PIXEL);
    for target_y in 0..height {
        let y0 = target_y * source_height / height;
        let y1 = (target_y + 1) * source_height / height;
        for target_x in 0..width {
            let x0 = target_x * source_width / width;
            let x1 = (target_x + 1) * source_width / width;
            pixels.extend(box_average(frame, (x0, x1), (y0, y1)));
        }
    }
    Ok(RgbImage {
        width: width as u32,
        height: height as u32,
        pixels,
    })
}

/// Mean RGB of the source pixels in `[x0, x1) x [y0, y1)`, rounded to
/// nearest. The block is never empty: the caller's ranges tile the source
/// and `width <= source_width`, `height <= source_height` guarantee each
/// covers at least one pixel.
fn box_average(frame: &Frame, (x0, x1): (usize, usize), (y0, y1): (usize, usize)) -> [u8; 3] {
    let source_width = frame.width as usize;
    let mut sum = [0u64; 3];
    for y in y0..y1 {
        let row = y * source_width * BGRA_BYTES_PER_PIXEL;
        for x in x0..x1 {
            let offset = row + x * BGRA_BYTES_PER_PIXEL;
            // Source is BGRA; output is RGB.
            sum[0] += u64::from(frame.pixels[offset + 2]);
            sum[1] += u64::from(frame.pixels[offset + 1]);
            sum[2] += u64::from(frame.pixels[offset]);
        }
    }
    let count = ((x1 - x0) * (y1 - y0)) as u64;
    sum.map(|channel| ((channel + count / 2) / count) as u8)
}

/// Downscales `frame` to at most [`THUMBNAIL_MAX_WIDTH`] wide and encodes it
/// as a baseline JPEG.
pub fn thumbnail_from_frame(frame: &Frame) -> Result<Thumbnail, ThumbnailError> {
    Thumbnail::from_rgb(&downscale_bgra_to_rgb(frame, THUMBNAIL_MAX_WIDTH)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uniform_frame(width: u32, height: u32, bgra: [u8; 4]) -> Frame {
        Frame {
            width,
            height,
            pixels: bgra.repeat(width as usize * height as usize),
        }
    }

    /// Black and white tiles, `tile` pixels square.
    fn checkerboard(width: u32, height: u32, tile: u32) -> Frame {
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                let white = ((x / tile) + (y / tile)).is_multiple_of(2);
                let value = if white { 0xff } else { 0x00 };
                pixels.extend_from_slice(&[value, value, value, 0xff]);
            }
        }
        Frame {
            width,
            height,
            pixels,
        }
    }

    /// Parses the SOF0 segment of a baseline JPEG and returns
    /// `(height, width)` as the file declares them.
    fn sof0_dimensions(jpeg: &[u8]) -> (u16, u16) {
        let mut offset = 2; // past SOI
        while offset + 4 <= jpeg.len() {
            assert_eq!(jpeg[offset], 0xff, "expected a marker at {offset}");
            let marker = jpeg[offset + 1];
            let length = usize::from(u16::from_be_bytes([jpeg[offset + 2], jpeg[offset + 3]]));
            if marker == 0xc0 {
                let payload = &jpeg[offset + 4..offset + 2 + length];
                // precision u8, height u16 BE, width u16 BE
                let height = u16::from_be_bytes([payload[1], payload[2]]);
                let width = u16::from_be_bytes([payload[3], payload[4]]);
                return (height, width);
            }
            offset += 2 + length;
        }
        panic!("no SOF0 marker found");
    }

    #[test]
    fn a_640x360_frame_becomes_a_320x180_baseline_jpeg() {
        let frame = uniform_frame(640, 360, [0xff, 0x00, 0x00, 0xff]); // blue

        let thumbnail = thumbnail_from_frame(&frame).unwrap();

        assert_eq!((thumbnail.width, thumbnail.height), (320, 180));
        assert_eq!(&thumbnail.jpeg[..2], &[0xff, 0xd8], "SOI");
        assert_eq!(
            &thumbnail.jpeg[thumbnail.jpeg.len() - 2..],
            &[0xff, 0xd9],
            "EOI"
        );
        assert_eq!(sof0_dimensions(&thumbnail.jpeg), (180, 320));
    }

    #[test]
    fn encoding_a_kept_rgb_image_matches_encoding_the_frame_it_came_from() {
        let frame = uniform_frame(640, 360, [0x20, 0x40, 0x80, 0xff]);

        let image = downscale_bgra_to_rgb(&frame, THUMBNAIL_MAX_WIDTH).unwrap();
        let from_image = Thumbnail::from_rgb(&image).unwrap();
        let from_frame = thumbnail_from_frame(&frame).unwrap();

        assert_eq!(from_image, from_frame);
        assert_eq!((from_image.width, from_image.height), (320, 180));
    }

    #[test]
    fn downscaling_a_fine_checkerboard_averages_to_mid_gray() {
        // 1 px tiles into 2x2 boxes: every destination pixel covers two
        // black and two white source pixels, so the mean is 127.5. A
        // nearest-neighbour sampler would return pure black or pure white.
        let frame = checkerboard(640, 360, 1);

        let image = downscale_bgra_to_rgb(&frame, 320).unwrap();

        assert_eq!((image.width, image.height), (320, 180));
        assert_eq!(image.pixels.len(), 320 * 180 * 3);
        for (index, channel) in image.pixels.iter().enumerate() {
            assert!(
                (126..=129).contains(channel),
                "channel {index} averaged to {channel}, not mid-gray"
            );
        }
    }

    #[test]
    fn a_frame_narrower_than_the_limit_is_not_upscaled() {
        let frame = uniform_frame(200, 100, [0x10, 0x20, 0x30, 0xff]);

        let image = downscale_bgra_to_rgb(&frame, 320).unwrap();

        assert_eq!((image.width, image.height), (200, 100));
        let thumbnail = thumbnail_from_frame(&frame).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (200, 100));
    }

    #[test]
    fn bgra_channel_order_becomes_rgb() {
        // Pure red in BGRA is [B=0, G=0, R=255, A=255]. A swapped or
        // alpha-included conversion cannot produce [255, 0, 0].
        let frame = uniform_frame(4, 2, [0x00, 0x00, 0xff, 0xff]);

        let image = downscale_bgra_to_rgb(&frame, 2).unwrap();

        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(image.pixels, [0xff, 0x00, 0x00, 0xff, 0x00, 0x00]);
    }

    #[test]
    fn a_frame_with_a_zero_dimension_is_rejected() {
        let empty = Frame {
            width: 0,
            height: 0,
            pixels: Vec::new(),
        };
        assert_eq!(
            downscale_bgra_to_rgb(&empty, 320),
            Err(ThumbnailError::EmptyFrame)
        );
        assert_eq!(
            thumbnail_from_frame(&empty),
            Err(ThumbnailError::EmptyFrame)
        );

        let no_rows = Frame {
            width: 8,
            height: 0,
            pixels: Vec::new(),
        };
        assert_eq!(
            downscale_bgra_to_rgb(&no_rows, 320),
            Err(ThumbnailError::EmptyFrame)
        );
    }

    #[test]
    fn a_pixel_buffer_that_disagrees_with_its_dimensions_is_rejected() {
        let short = Frame {
            width: 4,
            height: 4,
            pixels: vec![0; 4 * 4 * 4 - 1],
        };
        assert_eq!(
            downscale_bgra_to_rgb(&short, 320),
            Err(ThumbnailError::MalformedFrame)
        );
    }

    #[test]
    fn odd_ratios_keep_the_aspect_and_cover_every_source_pixel() {
        // 641x361 -> 320 wide; 361 * 320 / 641 = 180.2 -> 180. Source pixels
        // are not evenly divisible, so the box edges must still tile the
        // source without an empty block (which would divide by zero).
        let frame = uniform_frame(641, 361, [0x40, 0x80, 0xc0, 0xff]);

        let image = downscale_bgra_to_rgb(&frame, 320).unwrap();

        assert_eq!((image.width, image.height), (320, 180));
        assert!(
            image.pixels.chunks(3).all(|rgb| rgb == [0xc0, 0x80, 0x40]),
            "a uniform source must downscale to the same uniform colour"
        );
    }

    #[test]
    fn a_single_column_frame_stays_one_pixel_wide() {
        let frame = uniform_frame(1, 5, [0x00, 0xff, 0x00, 0xff]); // green

        let image = downscale_bgra_to_rgb(&frame, 320).unwrap();

        assert_eq!((image.width, image.height), (1, 5));
        assert_eq!(image.pixels, [0x00, 0xff, 0x00].repeat(5));
    }
}
