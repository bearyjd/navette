//! A 3x5 bitmap font, just large enough to draw the performance HUD onto a
//! frame buffer.
//!
//! Text on a decoded picture needs no font stack for this: the HUD is one
//! short line of digits and capitals, so a hand-cut glyph table keeps the
//! viewer free of any font dependency. Drawing lives here rather than in
//! [`crate::hud`] so the figures stay computable — and testable — without any
//! notion of pixels.

/// Pixel columns and rows of one glyph, before scaling.
const GLYPH_WIDTH: usize = 3;
const GLYPH_HEIGHT: usize = 5;
/// Blank columns between glyphs, before scaling.
const GLYPH_SPACING: usize = 1;

/// Each row is three bits, most significant bit leftmost.
const fn glyph(character: char) -> Option<[u8; GLYPH_HEIGHT]> {
    Some(match character {
        ' ' => [0b000, 0b000, 0b000, 0b000, 0b000],
        '.' => [0b000, 0b000, 0b000, 0b000, 0b010],
        ':' => [0b000, 0b010, 0b000, 0b010, 0b000],
        '/' => [0b001, 0b001, 0b010, 0b100, 0b100],
        '-' => [0b000, 0b000, 0b111, 0b000, 0b000],
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b001, 0b001],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        'A' => [0b111, 0b101, 0b111, 0b101, 0b101],
        'B' => [0b110, 0b101, 0b110, 0b101, 0b110],
        'C' => [0b111, 0b100, 0b100, 0b100, 0b111],
        'D' => [0b110, 0b101, 0b101, 0b101, 0b110],
        'E' => [0b111, 0b100, 0b111, 0b100, 0b111],
        'F' => [0b111, 0b100, 0b111, 0b100, 0b100],
        'G' => [0b111, 0b100, 0b101, 0b101, 0b111],
        'H' => [0b101, 0b101, 0b111, 0b101, 0b101],
        'I' => [0b111, 0b010, 0b010, 0b010, 0b111],
        'J' => [0b001, 0b001, 0b001, 0b101, 0b111],
        'K' => [0b101, 0b101, 0b110, 0b101, 0b101],
        'L' => [0b100, 0b100, 0b100, 0b100, 0b111],
        'M' => [0b101, 0b111, 0b111, 0b101, 0b101],
        'N' => [0b110, 0b101, 0b101, 0b101, 0b101],
        'O' => [0b111, 0b101, 0b101, 0b101, 0b111],
        'P' => [0b111, 0b101, 0b111, 0b100, 0b100],
        'Q' => [0b111, 0b101, 0b101, 0b111, 0b001],
        'R' => [0b111, 0b101, 0b111, 0b110, 0b101],
        'S' => [0b111, 0b100, 0b111, 0b001, 0b111],
        'T' => [0b111, 0b010, 0b010, 0b010, 0b010],
        'U' => [0b101, 0b101, 0b101, 0b101, 0b111],
        'V' => [0b101, 0b101, 0b101, 0b101, 0b010],
        'W' => [0b101, 0b101, 0b111, 0b111, 0b101],
        'X' => [0b101, 0b101, 0b010, 0b101, 0b101],
        'Y' => [0b101, 0b101, 0b010, 0b010, 0b010],
        'Z' => [0b111, 0b001, 0b010, 0b100, 0b111],
        _ => return None,
    })
}

/// Width in pixels of `text` drawn at `scale`.
pub fn text_width(text: &str, scale: usize) -> usize {
    let glyphs = text.chars().count();
    if glyphs == 0 {
        return 0;
    }
    (glyphs * (GLYPH_WIDTH + GLYPH_SPACING) - GLYPH_SPACING) * scale
}

/// Height in pixels of one line drawn at `scale`.
pub fn text_height(scale: usize) -> usize {
    GLYPH_HEIGHT * scale
}

/// How the overlay is drawn: glyph magnification and its two `0RGB` colours.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextStyle {
    pub scale: usize,
    pub foreground: u32,
    pub background: u32,
}

/// Draws `text` into a `size`-shaped buffer of `0RGB` pixels, on a filled
/// plate so it stays readable over any picture.
///
/// Characters the font does not carry are skipped rather than substituted, and
/// anything that would fall outside the buffer is clipped. Text is uppercased
/// first, since the font has no lowercase.
pub fn draw_text(
    buffer: &mut [u32],
    size: (usize, usize),
    origin: (usize, usize),
    text: &str,
    style: TextStyle,
) {
    let (width, height) = size;
    if style.scale == 0 || width == 0 || buffer.len() < width * height {
        return;
    }
    let text = text.to_ascii_uppercase();
    let padding = style.scale;
    fill(
        buffer,
        size,
        origin,
        (
            text_width(&text, style.scale) + 2 * padding,
            text_height(style.scale) + 2 * padding,
        ),
        style.background,
    );
    let mut left = origin.0 + padding;
    for character in text.chars() {
        if let Some(rows) = glyph(character) {
            draw_glyph(
                buffer,
                size,
                (left, origin.1 + padding),
                rows,
                style.scale,
                style.foreground,
            );
        }
        left += (GLYPH_WIDTH + GLYPH_SPACING) * style.scale;
    }
}

fn draw_glyph(
    buffer: &mut [u32],
    buffer_size: (usize, usize),
    origin: (usize, usize),
    rows: [u8; GLYPH_HEIGHT],
    scale: usize,
    colour: u32,
) {
    for (row, bits) in rows.into_iter().enumerate() {
        for column in 0..GLYPH_WIDTH {
            if bits & (1 << (GLYPH_WIDTH - 1 - column)) == 0 {
                continue;
            }
            fill(
                buffer,
                buffer_size,
                (origin.0 + column * scale, origin.1 + row * scale),
                (scale, scale),
                colour,
            );
        }
    }
}

/// Paints an axis-aligned rectangle, clipped to the buffer.
fn fill(
    buffer: &mut [u32],
    buffer_size: (usize, usize),
    origin: (usize, usize),
    size: (usize, usize),
    colour: u32,
) {
    let (width, height) = buffer_size;
    for y in origin.1..(origin.1 + size.1).min(height) {
        let row = y * width;
        for x in origin.0..(origin.0 + size.0).min(width) {
            if let Some(pixel) = buffer.get_mut(row + x) {
                *pixel = colour;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: usize = 64;
    const HEIGHT: usize = 16;
    const FOREGROUND: u32 = 0x00ff_ffff;
    const BACKGROUND: u32 = 0x0000_0000;
    const UNTOUCHED: u32 = 0x00ff_0000;

    fn canvas() -> Vec<u32> {
        vec![UNTOUCHED; WIDTH * HEIGHT]
    }

    fn style(scale: usize) -> TextStyle {
        TextStyle {
            scale,
            foreground: FOREGROUND,
            background: BACKGROUND,
        }
    }

    #[test]
    fn drawing_marks_the_plate_and_the_glyph_but_nothing_beyond_them() {
        let mut buffer = canvas();
        draw_text(&mut buffer, (WIDTH, HEIGHT), (2, 2), "1", style(1));
        // '1' at scale 1 has a lit pixel at its top-middle column and a blank
        // one to its left, both inside the plate.
        assert_eq!(buffer[3 * WIDTH + 4], FOREGROUND);
        assert_eq!(buffer[3 * WIDTH + 3], BACKGROUND);
        // Outside the plate the picture is untouched.
        assert_eq!(buffer[0], UNTOUCHED);
        assert_eq!(buffer[(HEIGHT - 1) * WIDTH + WIDTH - 1], UNTOUCHED);
    }

    #[test]
    fn text_that_runs_past_the_edge_is_clipped_rather_than_wrapping_or_panicking() {
        let mut buffer = canvas();
        // Far wider than the buffer, and starting close to its right edge.
        draw_text(
            &mut buffer,
            (WIDTH, HEIGHT),
            (WIDTH - 4, HEIGHT - 3),
            "FPS 999.9 KBPS 999999",
            style(3),
        );
        // Nothing wrapped onto the row above the origin: clipping by column
        // must not spill into the next row of the buffer.
        for x in 0..WIDTH {
            assert_eq!(
                buffer[(HEIGHT - 4) * WIDTH + x],
                UNTOUCHED,
                "row above the text was disturbed at column {x}"
            );
        }
    }

    #[test]
    fn unknown_characters_take_up_space_without_drawing_anything() {
        let mut buffer = canvas();
        draw_text(&mut buffer, (WIDTH, HEIGHT), (0, 0), "~", style(1));
        // The plate is drawn, but the glyph cell inside it stays blank.
        assert!(
            buffer[..GLYPH_HEIGHT * WIDTH]
                .iter()
                .all(|pixel| *pixel != FOREGROUND)
        );
        assert_eq!(text_width("~", 1), text_width("A", 1));
    }

    #[test]
    fn measurements_account_for_spacing_between_glyphs_but_not_after_the_last() {
        assert_eq!(text_width("", 2), 0);
        assert_eq!(text_width("A", 2), 6);
        assert_eq!(text_width("AB", 2), 14);
        assert_eq!(text_height(2), 10);
    }
}
