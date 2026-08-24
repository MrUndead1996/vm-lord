//! Turning what crosses the wire into what Windows keeps on its clipboard.
//!
//! Four kinds travel; each has a Windows format that means the same thing, and
//! none of them mean it in the same bytes:
//!
//! | on the wire | on the clipboard | what is between them |
//! | --- | --- | --- |
//! | `text/plain;charset=utf-8` | `CF_UNICODETEXT` | UTF-8 against UTF-16LE |
//! | `text/html` | registered `HTML Format` | the CF_HTML envelope |
//! | `image/bmp` | `CF_DIB` | a fourteen-byte file header |
//! | `image/png` | `CF_DIB` | an actual codec |
//!
//! Nothing here opens the clipboard or allocates global memory: these are
//! conversions on plain buffers, which is what makes them testable without a
//! window. The clipboard itself is [`super`].

use std::fmt;

/// What a picture is, between one format and another.
///
/// Rows top down, three bytes per pixel in the order a PNG has them. BMP's
/// bottom-up rows and BGR order are handled where BMPs are read and written.
struct Picture {
    width: u32,
    height: u32,
    /// `width * height * 3` bytes, red first.
    rgb: Vec<u8>,
}

/// A picture that could not be converted.
#[derive(Debug)]
pub struct ImageError(String);

impl fmt::Display for ImageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "the picture could not be converted: {}", self.0)
    }
}

impl std::error::Error for ImageError {}

/// The most pixels a picture may have, whatever its header claims.
///
/// A guest may announce any geometry it likes; this is what stops a header
/// from making the viewer allocate on the strength of two numbers. Thirty-two
/// megabytes of transfer at three bytes a pixel cannot exceed it either.
const MAX_PIXELS: usize = 64 * 1024 * 1024;

/// The `BITMAPFILEHEADER` a BMP begins with and a DIB does not.
const FILE_HEADER: usize = 14;

/// The smallest DIB header, which is `BITMAPINFOHEADER`.
const INFO_HEADER: usize = 40;

/// UTF-8 as `CF_UNICODETEXT` holds it: UTF-16LE, terminated.
#[must_use]
pub fn utf16_of(text: &[u8]) -> Vec<u16> {
    let mut units: Vec<u16> = String::from_utf8_lossy(text).encode_utf16().collect();
    units.push(0);

    units
}

/// `CF_UNICODETEXT` as the wire holds it: UTF-8, unterminated.
///
/// Lossy on purpose. A lone surrogate is something a Windows application can
/// put on the clipboard and something UTF-8 cannot hold; refusing the paste
/// over one character would be worse than replacing it.
#[must_use]
pub fn utf8_of(units: &[u16]) -> Vec<u8> {
    let end = units.iter().position(|unit| *unit == 0).unwrap_or(units.len());

    String::from_utf16_lossy(&units[..end]).into_bytes()
}

/// The header block of a CF_HTML envelope, with room for the offsets.
const HTML_HEADER: &str = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";

/// What precedes the fragment inside the envelope's document.
const HTML_PROLOGUE: &str = "<html><body>\r\n<!--StartFragment-->";

/// What follows it.
const HTML_EPILOGUE: &str = "<!--EndFragment-->\r\n</body></html>";

/// HTML as the `HTML Format` clipboard format holds it.
///
/// The offsets are counted over the finished envelope, which is why the header
/// is written with ten-digit fields first and filled in after: a number that
/// changed width would move the thing it points at.
#[must_use]
pub fn cf_html_of(html: &[u8]) -> Vec<u8> {
    let head = HTML_HEADER.len();
    let start_html = head;
    let start_fragment = head + HTML_PROLOGUE.len();
    let end_fragment = start_fragment + html.len();
    let end_html = end_fragment + HTML_EPILOGUE.len();

    let header = HTML_HEADER
        .replacen("StartHTML:0000000000", &format!("StartHTML:{start_html:010}"), 1)
        .replacen("EndHTML:0000000000", &format!("EndHTML:{end_html:010}"), 1)
        .replacen(
            "StartFragment:0000000000",
            &format!("StartFragment:{start_fragment:010}"),
            1,
        )
        .replacen(
            "EndFragment:0000000000",
            &format!("EndFragment:{end_fragment:010}"),
            1,
        );

    let mut envelope = Vec::with_capacity(end_html);
    envelope.extend_from_slice(header.as_bytes());
    envelope.extend_from_slice(HTML_PROLOGUE.as_bytes());
    envelope.extend_from_slice(html);
    envelope.extend_from_slice(HTML_EPILOGUE.as_bytes());

    envelope
}

/// The fragment inside a CF_HTML envelope, if it has one.
///
/// `None` for an envelope whose headers are missing or whose offsets do not
/// lie inside it -- which is a document this build should not slice rather than
/// one it should slice wrongly.
#[must_use]
pub fn html_of_cf_html(envelope: &[u8]) -> Option<Vec<u8>> {
    let text = String::from_utf8_lossy(envelope);
    let start = header_value(&text, "StartFragment")?;
    let end = header_value(&text, "EndFragment")?;

    if start > end || end > envelope.len() {
        return None;
    }

    Some(envelope[start..end].to_vec())
}

/// One `Name:0000000123` header of an envelope.
fn header_value(text: &str, name: &str) -> Option<usize> {
    let at = text.find(&format!("{name}:"))? + name.len() + 1;
    let digits: String = text[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();

    digits.trim().parse().ok()
}

/// A BMP as `CF_DIB` holds it: the same bytes without the file header.
///
/// `None` for anything that is not a BMP this build can vouch for, since what
/// follows would otherwise be a slice of something else.
#[must_use]
pub fn dib_of_bmp(bmp: &[u8]) -> Option<Vec<u8>> {
    if bmp.len() < FILE_HEADER + INFO_HEADER || &bmp[..2] != b"BM" {
        return None;
    }

    Some(bmp[FILE_HEADER..].to_vec())
}

/// A `CF_DIB` as a BMP: the file header, then the same bytes.
///
/// The pixel offset is computed from the DIB's own header rather than assumed:
/// a `BITMAPV5HEADER` is 124 bytes, a palette is four bytes a colour, and
/// `BI_BITFIELDS` puts three masks between the header and the pixels.
#[must_use]
pub fn bmp_of_dib(dib: &[u8]) -> Vec<u8> {
    let header_len = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]) as usize;
    let offset = FILE_HEADER + header_len + extras(dib, header_len);
    let size = FILE_HEADER + dib.len();

    let mut bmp = Vec::with_capacity(size);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&(size as u32).to_le_bytes());
    bmp.extend_from_slice(&0u16.to_le_bytes());
    bmp.extend_from_slice(&0u16.to_le_bytes());
    bmp.extend_from_slice(&(offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);

    bmp
}

/// What sits between a DIB's header and its pixels: a palette, some masks.
fn extras(dib: &[u8], header_len: usize) -> usize {
    if dib.len() < INFO_HEADER || header_len < INFO_HEADER {
        return 0;
    }

    let bits = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = u32::from_le_bytes([dib[16], dib[17], dib[18], dib[19]]);
    let used = u32::from_le_bytes([dib[32], dib[33], dib[34], dib[35]]) as usize;

    let palette = if bits <= 8 {
        let colours = if used == 0 { 1usize << bits } else { used };
        colours * 4
    } else {
        used * 4
    };
    // `BI_BITFIELDS` is 3, and only a v1 header keeps its masks outside itself.
    let masks = usize::from(compression == 3 && header_len == INFO_HEADER) * 12;

    palette + masks
}

/// A BMP as a PNG.
///
/// # Errors
///
/// [`ImageError`] for a BMP this build does not read -- anything but 24- or
/// 32-bit uncompressed pixels -- or for one whose header describes more pixels
/// than the viewer will hold.
pub fn png_of_bmp(bmp: &[u8]) -> Result<Vec<u8>, ImageError> {
    let picture = decode_bmp(bmp)?;
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, picture.width, picture.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(&picture.rgb))
        .map_err(|error| ImageError(error.to_string()))?;

    Ok(bytes)
}

/// A PNG as a BMP, which is a DIB once its file header comes off.
///
/// # Errors
///
/// [`ImageError`] for a PNG that does not decode, or one larger than the
/// viewer will hold.
pub fn bmp_of_png(png_bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    // A cursor rather than the slice itself: this codec seeks.
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|error| ImageError(error.to_string()))?;
    let info = reader.info();
    let (width, height) = (info.width, info.height);
    ensure_size(width, height)?;

    let mut buffer = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|error| ImageError(error.to_string()))?;
    let rgb = to_rgb(&buffer[..frame.buffer_size()], frame.color_type)?;

    Ok(encode_bmp(&Picture { width, height, rgb }))
}

/// Whatever the codec produced, as three bytes a pixel.
fn to_rgb(bytes: &[u8], colour: png::ColorType) -> Result<Vec<u8>, ImageError> {
    match colour {
        png::ColorType::Rgb => Ok(bytes.to_vec()),
        png::ColorType::Rgba => Ok(bytes
            .chunks_exact(4)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect()),
        png::ColorType::Grayscale => Ok(bytes
            .iter()
            .flat_map(|level| [*level, *level, *level])
            .collect()),
        png::ColorType::GrayscaleAlpha => Ok(bytes
            .chunks_exact(2)
            .flat_map(|pixel| [pixel[0], pixel[0], pixel[0]])
            .collect()),
        other => Err(ImageError(format!("{other:?} is not a colour this reads"))),
    }
}

/// Reads the BMPs a desktop actually puts on a clipboard.
fn decode_bmp(bmp: &[u8]) -> Result<Picture, ImageError> {
    if bmp.len() < FILE_HEADER + INFO_HEADER || &bmp[..2] != b"BM" {
        return Err(ImageError("not a BMP".to_owned()));
    }

    let offset = u32::from_le_bytes([bmp[10], bmp[11], bmp[12], bmp[13]]) as usize;
    let dib = &bmp[FILE_HEADER..];
    let width = i32::from_le_bytes([dib[4], dib[5], dib[6], dib[7]]);
    let raw_height = i32::from_le_bytes([dib[8], dib[9], dib[10], dib[11]]);
    let bits = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = u32::from_le_bytes([dib[16], dib[17], dib[18], dib[19]]);

    if !(bits == 24 || bits == 32) || !(compression == 0 || compression == 3) {
        return Err(ImageError(format!(
            "{bits}-bit pixels under compression {compression} are not read by this build"
        )));
    }
    if width <= 0 || raw_height == 0 {
        return Err(ImageError("a picture with no pixels".to_owned()));
    }

    let width_pixels = width as u32;
    let height_pixels = raw_height.unsigned_abs();
    ensure_size(width_pixels, height_pixels)?;

    let bytes_per_pixel = usize::from(bits / 8);
    // Every row of a BMP is padded to four bytes.
    let stride = (width_pixels as usize * bytes_per_pixel).div_ceil(4) * 4;
    let needed = offset
        .checked_add(stride * height_pixels as usize)
        .ok_or_else(|| ImageError("a picture larger than this address space".to_owned()))?;
    if bmp.len() < needed {
        return Err(ImageError("the picture is shorter than its header".to_owned()));
    }

    // A positive height is bottom-up, which is the ordinary BMP; a negative one
    // is top-down, which is what a screen capture usually is.
    let top_down = raw_height < 0;
    let mut rgb = Vec::with_capacity(width_pixels as usize * height_pixels as usize * 3);
    for row in 0..height_pixels as usize {
        let source = if top_down {
            row
        } else {
            height_pixels as usize - 1 - row
        };
        let start = offset + source * stride;
        for column in 0..width_pixels as usize {
            let pixel = start + column * bytes_per_pixel;
            // BMP keeps its channels blue first.
            rgb.extend_from_slice(&[bmp[pixel + 2], bmp[pixel + 1], bmp[pixel]]);
        }
    }

    Ok(Picture {
        width: width_pixels,
        height: height_pixels,
        rgb,
    })
}

/// Writes a 24-bit bottom-up BMP, which every Windows application reads.
fn encode_bmp(picture: &Picture) -> Vec<u8> {
    let width = picture.width as usize;
    let height = picture.height as usize;
    let stride = (width * 3).div_ceil(4) * 4;
    let pixels = stride * height;
    let offset = FILE_HEADER + INFO_HEADER;

    let mut bmp = Vec::with_capacity(offset + pixels);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((offset + pixels) as u32).to_le_bytes());
    bmp.extend_from_slice(&0u16.to_le_bytes());
    bmp.extend_from_slice(&0u16.to_le_bytes());
    bmp.extend_from_slice(&(offset as u32).to_le_bytes());

    bmp.extend_from_slice(&(INFO_HEADER as u32).to_le_bytes());
    bmp.extend_from_slice(&(picture.width as i32).to_le_bytes());
    bmp.extend_from_slice(&(picture.height as i32).to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&24u16.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&(pixels as u32).to_le_bytes());
    // Ninety-six dots per inch in pixels per metre, which is what everything
    // writes and nothing reads.
    bmp.extend_from_slice(&3780u32.to_le_bytes());
    bmp.extend_from_slice(&3780u32.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());

    for row in (0..height).rev() {
        let start = row * width * 3;
        for column in 0..width {
            let pixel = start + column * 3;
            bmp.extend_from_slice(&[
                picture.rgb[pixel + 2],
                picture.rgb[pixel + 1],
                picture.rgb[pixel],
            ]);
        }
        bmp.resize(bmp.len() + stride - width * 3, 0);
    }

    bmp
}

/// Refuses a geometry before anything is allocated for it.
fn ensure_size(width: u32, height: u32) -> Result<(), ImageError> {
    let pixels = (width as usize).saturating_mul(height as usize);
    if pixels == 0 || pixels > MAX_PIXELS {
        return Err(ImageError(format!(
            "{width}x{height} is not a picture this build will hold"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_crosses_the_encodings_unchanged() {
        let text = "ёлка — ok\r\n".as_bytes();

        assert_eq!(utf8_of(&utf16_of(text)), text);
    }

    #[test]
    fn what_windows_holds_is_terminated_and_what_crosses_is_not() {
        let units = utf16_of(b"hi");

        assert_eq!(units.last(), Some(&0));
        assert_eq!(utf8_of(&units), b"hi");
    }

    #[test]
    fn a_lone_surrogate_does_not_panic_and_does_not_vanish_silently() {
        let broken = [0xD800u16, u16::from(b'a')];

        let text = utf8_of(&broken);

        assert!(text.ends_with(b"a"));
    }

    #[test]
    fn an_html_envelope_round_trips() {
        let html = b"<b>hi</b>";

        let envelope = cf_html_of(html);

        assert!(envelope.starts_with(b"Version:0.9"));
        assert_eq!(html_of_cf_html(&envelope).as_deref(), Some(&html[..]));
    }

    #[test]
    fn an_envelopes_offsets_point_at_the_fragment() {
        let envelope = cf_html_of(b"<p>x</p>");
        let text = String::from_utf8(envelope.clone()).expect("ascii headers");

        let start = header_value(&text, "StartFragment").expect("a start offset");
        let end = header_value(&text, "EndFragment").expect("an end offset");

        assert_eq!(&envelope[start..end], b"<p>x</p>");
    }

    #[test]
    fn an_envelope_whose_offsets_lie_outside_it_is_refused() {
        let broken = b"Version:0.9\r\nStartFragment:0000000010\r\nEndFragment:0000009999\r\n";

        assert_eq!(html_of_cf_html(broken), None);
    }

    #[test]
    fn a_dib_is_a_bmp_without_its_file_header() {
        let bmp = smallest_bmp();

        let dib = dib_of_bmp(&bmp).expect("a bmp this build wrote");

        assert_eq!(dib.len(), bmp.len() - FILE_HEADER);
        assert_eq!(bmp_of_dib(&dib), bmp);
    }

    #[test]
    fn a_truncated_bmp_is_refused_rather_than_sliced() {
        assert_eq!(dib_of_bmp(b"BM"), None);
        assert_eq!(dib_of_bmp(&[0; 200]), None);
    }

    #[test]
    fn a_picture_survives_png_and_back() {
        let bmp = smallest_bmp();

        let png = png_of_bmp(&bmp).expect("an encodable picture");
        let back = bmp_of_png(&png).expect("a decodable picture");

        assert_eq!(pixels_of(&back), pixels_of(&bmp));
    }

    #[test]
    fn a_top_down_capture_is_read_the_right_way_up() {
        let mut bmp = smallest_bmp();
        // A negative height, which is how a screen capture arrives.
        bmp[FILE_HEADER + 8..FILE_HEADER + 12].copy_from_slice(&(-2i32).to_le_bytes());

        let flipped = decode_bmp(&bmp).expect("a top-down picture");
        let upright = decode_bmp(&smallest_bmp()).expect("a bottom-up picture");

        assert_eq!(flipped.rgb.len(), upright.rgb.len());
        assert_ne!(flipped.rgb, upright.rgb, "the rows are in the other order");
    }

    #[test]
    fn a_bmp_this_build_cannot_read_is_refused_rather_than_guessed_at() {
        let mut bmp = smallest_bmp();
        // Eight-bit palette pixels, which the desktop does not produce and this
        // build does not read.
        bmp[FILE_HEADER + 14..FILE_HEADER + 16].copy_from_slice(&8u16.to_le_bytes());

        assert!(png_of_bmp(&bmp).is_err());
    }

    #[test]
    fn a_geometry_no_memory_would_hold_is_refused_before_it_is_allocated() {
        let mut bmp = smallest_bmp();
        bmp[FILE_HEADER + 4..FILE_HEADER + 8].copy_from_slice(&100_000i32.to_le_bytes());
        bmp[FILE_HEADER + 8..FILE_HEADER + 12].copy_from_slice(&100_000i32.to_le_bytes());

        assert!(png_of_bmp(&bmp).is_err());
    }

    /// A two-by-two 24-bit BMP with four distinguishable pixels.
    fn smallest_bmp() -> Vec<u8> {
        let rgb = vec![
            255, 0, 0, // red
            0, 255, 0, // green
            0, 0, 255, // blue
            255, 255, 0, // yellow
        ];

        encode_bmp(&Picture {
            width: 2,
            height: 2,
            rgb,
        })
    }

    /// A picture's pixels, so that two encodings can be compared by what they
    /// show rather than by their bytes.
    fn pixels_of(bmp: &[u8]) -> Vec<u8> {
        decode_bmp(bmp).expect("a bmp this build wrote").rgb
    }
}
