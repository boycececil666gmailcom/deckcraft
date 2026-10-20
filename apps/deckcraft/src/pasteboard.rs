//! System pasteboard: HTML rich text, and pictures.
//!
//! On Windows, decodes native clipboard bitmaps (CF_DIB / CF_DIBV5) as well as registered PNG
//! payloads into PNG byte buffers. On macOS, reads Keynote/Pages TIFF pictures decoded by arboard.

/// HTML currently on the pasteboard, when there is some.
pub fn html() -> Option<String> {
    let mut cb = arboard::Clipboard::new().ok()?;
    let html = cb.get().html().ok()?;
    let html = html.trim();
    if html.is_empty() { None } else { Some(html.to_string()) }
}

/// Write `html` and the plain alternative `text`. `false` when the pasteboard refuses it.
pub fn set_html(html: &str, text: &str) -> bool {
    let Ok(mut cb) = arboard::Clipboard::new() else { return false };
    cb.set_html(html, Some(text)).is_ok()
}

/// The pasteboard picture as PNG bytes.
///
/// On Windows, queries the native clipboard for registered "PNG", `CF_DIBV5`, and `CF_DIB`,
/// decoding device-independent bitmaps to valid PNG byte buffers while handling BGR/BGRA channel
/// orders, row padding, bottom-up/top-down orientations, and GDI zero-alpha quirks.
/// Falls back to arboard if native formats are absent.
pub fn image_png() -> Option<Vec<u8>> {
    #[cfg(target_os = "windows")]
    {
        if let Some(bytes) = windows_image_png() {
            return Some(bytes);
        }
    }
    arboard_image_png()
}

fn arboard_image_png() -> Option<Vec<u8>> {
    let mut cb = arboard::Clipboard::new().ok()?;
    let img = cb.get_image().ok()?;
    let width = u32::try_from(img.width).ok()?;
    let height = u32::try_from(img.height).ok()?;
    let rgba = image::RgbaImage::from_raw(width, height, img.bytes.into_owned())?;
    let mut out = Vec::new();
    rgba.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).ok()?;
    Some(out)
}

#[cfg(target_os = "windows")]
fn windows_image_png() -> Option<Vec<u8>> {
    let _clip = clipboard_win::Clipboard::new_attempts(10).ok()?;

    // 1. Registered format "PNG" or "image/png" (written by Chrome, Edge, modern Snipping Tool).
    for name in ["PNG", "image/png"] {
        if let Some(id) = clipboard_win::raw::register_format(name).map(u32::from)
            && clipboard_win::is_format_avail(id)
        {
            let mut data = Vec::new();
            if clipboard_win::raw::get_vec(id, &mut data).is_ok()
                && !data.is_empty()
                && image::load_from_memory_with_format(&data, image::ImageFormat::Png).is_ok()
            {
                return Some(data);
            }
        }
    }

    // 2. CF_DIBV5 (Device Independent Bitmap Version 5, format 17).
    if clipboard_win::is_format_avail(clipboard_win::formats::CF_DIBV5) {
        let mut data = Vec::new();
        if clipboard_win::raw::get_vec(clipboard_win::formats::CF_DIBV5, &mut data).is_ok()
            && !data.is_empty()
            && let Some(png) = decode_dib_to_png(&data)
        {
            return Some(png);
        }
    }

    // 3. CF_DIB (Standard Device Independent Bitmap, format 8).
    if clipboard_win::is_format_avail(clipboard_win::formats::CF_DIB) {
        let mut data = Vec::new();
        if clipboard_win::raw::get_vec(clipboard_win::formats::CF_DIB, &mut data).is_ok()
            && !data.is_empty()
            && let Some(png) = decode_dib_to_png(&data)
        {
            return Some(png);
        }
    }

    None
}

/// Decodes raw CF_DIB or CF_DIBV5 bytes from the Windows clipboard into PNG bytes.
pub fn decode_dib_to_png(data: &[u8]) -> Option<Vec<u8>> {
    let rgba = decode_dib(data)?;
    let mut out = Vec::new();
    rgba.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).ok()?;
    Some(out)
}

/// Decodes raw CF_DIB or CF_DIBV5 bytes into an RGBA image.
pub fn decode_dib(data: &[u8]) -> Option<image::RgbaImage> {
    if data.len() < 40 {
        return None;
    }
    let header_size = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
    if header_size < 40 || header_size > data.len() {
        return None;
    }

    let width = i32::from_le_bytes(data[4..8].try_into().ok()?);
    let height = i32::from_le_bytes(data[8..12].try_into().ok()?);
    let planes = u16::from_le_bytes(data[12..14].try_into().ok()?);
    let bit_count = u16::from_le_bytes(data[14..16].try_into().ok()?);
    let compression = u32::from_le_bytes(data[16..20].try_into().ok()?);
    let clr_used = u32::from_le_bytes(data[32..36].try_into().ok()?);

    if width <= 0 || height == 0 || planes != 1 {
        return None;
    }

    // Try standard BMP decoding by synthesizing a 14-byte BITMAPFILEHEADER.
    if let Some(mut img) = decode_via_bmp_file(data, header_size, bit_count, compression, clr_used) {
        fix_alpha_if_all_transparent(&mut img);
        return Some(img);
    }

    // Fallback: direct uncompressed DIB decoding (handles 24-bit and 32-bit BI_RGB / BI_BITFIELDS).
    let mask_size = if header_size == 40 && compression == 3 {
        12
    } else if header_size == 40 && compression == 6 {
        16
    } else {
        0
    };
    let palette_colors = if bit_count <= 8 { if clr_used != 0 { clr_used as usize } else { 1 << bit_count } } else { clr_used as usize };
    let offset_to_bits = header_size + mask_size + palette_colors * 4;

    if (bit_count == 24 || bit_count == 32)
        && (compression == 0 || compression == 3)
        && let Some(mut img) = decode_uncompressed_dib(data, header_size, width, height, bit_count, compression, offset_to_bits)
    {
        fix_alpha_if_all_transparent(&mut img);
        return Some(img);
    }

    None
}

fn decode_via_bmp_file(data: &[u8], header_size: usize, bit_count: u16, compression: u32, clr_used: u32) -> Option<image::RgbaImage> {
    let mask_size = if header_size == 40 && compression == 3 {
        12
    } else if header_size == 40 && compression == 6 {
        16
    } else {
        0
    };
    let palette_colors = if bit_count <= 8 { if clr_used != 0 { clr_used as usize } else { 1 << bit_count } } else { clr_used as usize };
    let palette_size = palette_colors * 4;
    let offset_to_bits = header_size + mask_size + palette_size;
    let bf_off_bits = u32::try_from(14 + offset_to_bits).ok()?;
    let bf_size = u32::try_from(14 + data.len()).ok()?;

    let mut bmp = Vec::with_capacity(14 + data.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&bf_size.to_le_bytes());
    bmp.extend_from_slice(&[0u8; 4]);
    bmp.extend_from_slice(&bf_off_bits.to_le_bytes());
    bmp.extend_from_slice(data);

    // If DIBV5 has 32-bit BI_RGB but carries an alpha mask (Chrome/Electron),
    // tweak compression to BI_BITFIELDS so the BMP decoder reads the alpha channel.
    if header_size >= 124 && bit_count == 32 && compression == 0 && data.len() >= 56 {
        let alpha_mask = u32::from_le_bytes(data[52..56].try_into().ok()?);
        if alpha_mask == 0xff000000 {
            // offset 14 + 16 is biCompression in the BMP file
            bmp[14 + 16..14 + 20].copy_from_slice(&3u32.to_le_bytes());
            let r_mask = u32::from_le_bytes(data[40..44].try_into().ok()?);
            let g_mask = u32::from_le_bytes(data[44..48].try_into().ok()?);
            let b_mask = u32::from_le_bytes(data[48..52].try_into().ok()?);
            if r_mask == 0 && g_mask == 0 && b_mask == 0 {
                bmp[14 + 40..14 + 44].copy_from_slice(&0x00ff0000u32.to_le_bytes());
                bmp[14 + 44..14 + 48].copy_from_slice(&0x0000ff00u32.to_le_bytes());
                bmp[14 + 48..14 + 52].copy_from_slice(&0x000000ffu32.to_le_bytes());
            }
        }
    }

    let dyn_img = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp).ok()?;
    Some(dyn_img.to_rgba8())
}

fn fix_alpha_if_all_transparent(rgba: &mut image::RgbaImage) {
    if rgba.pixels().all(|p| p[3] == 0) {
        for p in rgba.pixels_mut() {
            p[3] = 255;
        }
    }
}

fn decode_uncompressed_dib(
    data: &[u8],
    header_size: usize,
    width: i32,
    height: i32,
    bit_count: u16,
    compression: u32,
    offset_to_bits: usize,
) -> Option<image::RgbaImage> {
    let w = u32::try_from(width).ok()?;
    let h = height.unsigned_abs();
    let top_down = height < 0;

    let bytes_per_pixel = usize::from(bit_count / 8);
    let row_stride = (w as usize * bytes_per_pixel).div_ceil(4) * 4;
    let total_pixel_bytes = row_stride.checked_mul(h as usize)?;

    if offset_to_bits > data.len() {
        return None;
    }
    let bits = &data[offset_to_bits..];
    if bits.len() < total_pixel_bytes {
        return None;
    }

    let mut img = image::RgbaImage::new(w, h);
    let mut has_non_zero_alpha = false;

    let (r_shift, g_shift, b_shift, a_shift) = if bit_count == 32 && compression == 3 {
        let (rm, gm, bm, am) = if header_size >= 56 {
            let rm = u32::from_le_bytes(data[40..44].try_into().ok()?);
            let gm = u32::from_le_bytes(data[44..48].try_into().ok()?);
            let bm = u32::from_le_bytes(data[48..52].try_into().ok()?);
            let am = u32::from_le_bytes(data[52..56].try_into().ok()?);
            (rm, gm, bm, am)
        } else if data.len() >= 52 {
            let rm = u32::from_le_bytes(data[40..44].try_into().ok()?);
            let gm = u32::from_le_bytes(data[44..48].try_into().ok()?);
            let bm = u32::from_le_bytes(data[48..52].try_into().ok()?);
            let am = if data.len() >= 56 { u32::from_le_bytes(data[52..56].try_into().unwrap_or([0; 4])) } else { 0 };
            (rm, gm, bm, am)
        } else {
            (0x00ff0000, 0x0000ff00, 0x000000ff, 0)
        };
        (mask_shift(rm), mask_shift(gm), mask_shift(bm), if am != 0 { Some(mask_shift(am)) } else { None })
    } else {
        (16, 8, 0, if bit_count == 32 { Some(24) } else { None })
    };

    for y in 0..h {
        let src_y = if top_down { y as usize } else { (h - 1 - y) as usize };
        let row_start = src_y * row_stride;
        let row_bytes = &bits[row_start..row_start + row_stride];

        for x in 0..w {
            let px_offset = (x as usize) * bytes_per_pixel;
            let (r, g, b, a) = if bit_count == 32 {
                let val = u32::from_le_bytes(row_bytes[px_offset..px_offset + 4].try_into().ok()?);
                let r = ((val >> r_shift) & 0xff) as u8;
                let g = ((val >> g_shift) & 0xff) as u8;
                let b = ((val >> b_shift) & 0xff) as u8;
                let a = if let Some(asft) = a_shift {
                    let a_val = ((val >> asft) & 0xff) as u8;
                    if a_val != 0 {
                        has_non_zero_alpha = true;
                    }
                    a_val
                } else {
                    255
                };
                (r, g, b, a)
            } else {
                let b = row_bytes[px_offset];
                let g = row_bytes[px_offset + 1];
                let r = row_bytes[px_offset + 2];
                (r, g, b, 255)
            };
            img.put_pixel(x, y, image::Rgba([r, g, b, a]));
        }
    }

    if bit_count == 32 && a_shift.is_some() && !has_non_zero_alpha {
        for p in img.pixels_mut() {
            p[3] = 255;
        }
    }

    Some(img)
}

fn mask_shift(mask: u32) -> u32 {
    if mask == 0 {
        return 0;
    }
    mask.trailing_zeros()
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    fn make_cf_dib_24(width: i32, height: i32, bgr: &[u8; 3]) -> Vec<u8> {
        let w = width as usize;
        let h = height.unsigned_abs() as usize;
        let row_stride = (w * 3).div_ceil(4) * 4;
        let mut dib = vec![0u8; 40 + row_stride * h];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&width.to_le_bytes());
        dib[8..12].copy_from_slice(&height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        dib[16..20].copy_from_slice(&0u32.to_le_bytes());
        dib[20..24].copy_from_slice(&(row_stride as u32 * h as u32).to_le_bytes());

        for y in 0..h {
            let row_start = 40 + y * row_stride;
            for x in 0..w {
                let px = row_start + x * 3;
                dib[px..px + 3].copy_from_slice(bgr);
            }
        }
        dib
    }

    fn make_cf_dib_32(width: i32, height: i32, bgra: &[u8; 4]) -> Vec<u8> {
        let w = width as usize;
        let h = height.unsigned_abs() as usize;
        let row_stride = w * 4;
        let mut dib = vec![0u8; 40 + row_stride * h];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&width.to_le_bytes());
        dib[8..12].copy_from_slice(&height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[16..20].copy_from_slice(&0u32.to_le_bytes());
        dib[20..24].copy_from_slice(&(row_stride as u32 * h as u32).to_le_bytes());

        for y in 0..h {
            let row_start = 40 + y * row_stride;
            for x in 0..w {
                let px = row_start + x * 4;
                dib[px..px + 4].copy_from_slice(bgra);
            }
        }
        dib
    }

    fn make_cf_dibv5_32(width: i32, height: i32, bgra: &[u8; 4], alpha_mask: u32) -> Vec<u8> {
        let w = width as usize;
        let h = height.unsigned_abs() as usize;
        let row_stride = w * 4;
        let mut dib = vec![0u8; 124 + row_stride * h];
        dib[0..4].copy_from_slice(&124u32.to_le_bytes());
        dib[4..8].copy_from_slice(&width.to_le_bytes());
        dib[8..12].copy_from_slice(&height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        let comp = if alpha_mask != 0 { 3u32 } else { 0u32 };
        dib[16..20].copy_from_slice(&comp.to_le_bytes());
        dib[20..24].copy_from_slice(&(row_stride as u32 * h as u32).to_le_bytes());
        dib[40..44].copy_from_slice(&0x00ff0000u32.to_le_bytes());
        dib[44..48].copy_from_slice(&0x0000ff00u32.to_le_bytes());
        dib[48..52].copy_from_slice(&0x000000ffu32.to_le_bytes());
        dib[52..56].copy_from_slice(&alpha_mask.to_le_bytes());

        for y in 0..h {
            let row_start = 124 + y * row_stride;
            for x in 0..w {
                let px = row_start + x * 4;
                dib[px..px + 4].copy_from_slice(bgra);
            }
        }
        dib
    }

    #[test]
    fn cf_dib_24bit_decodes_to_valid_png() {
        // Red pixel in BGR is [0, 0, 255]
        let dib = make_cf_dib_24(3, 2, &[0, 0, 255]);
        let png = super::decode_dib_to_png(&dib).expect("png");
        let decoded = image::load_from_memory(&png).expect("decode").to_rgba8();
        assert_eq!(decoded.width(), 3);
        assert_eq!(decoded.height(), 2);
        let p = decoded.get_pixel(1, 1);
        assert_eq!(p[0], 255);
        assert_eq!(p[1], 0);
        assert_eq!(p[2], 0);
        assert_eq!(p[3], 255);
    }

    #[test]
    fn cf_dib_32bit_with_zero_alpha_becomes_opaque() {
        // Snipping Tool / GDI quirk: 32-bit BGRx with alpha = 0 for every pixel.
        let dib = make_cf_dib_32(2, 2, &[255, 128, 64, 0]);
        let png = super::decode_dib_to_png(&dib).expect("png");
        let decoded = image::load_from_memory(&png).expect("decode").to_rgba8();
        assert_eq!(decoded.width(), 2);
        assert_eq!(decoded.height(), 2);
        let p = decoded.get_pixel(0, 0);
        assert_eq!(p[0], 64);
        assert_eq!(p[1], 128);
        assert_eq!(p[2], 255);
        assert_eq!(p[3], 255, "zero alpha should be restored to fully opaque");
    }

    #[test]
    fn cf_dibv5_32bit_with_alpha_mask_preserves_transparency() {
        // Chrome / modern browser quirk: BITMAPV5HEADER with alpha mask and semi-transparent alpha.
        let dib = make_cf_dibv5_32(2, 2, &[10, 20, 30, 180], 0xff000000);
        let png = super::decode_dib_to_png(&dib).expect("png");
        let decoded = image::load_from_memory(&png).expect("decode").to_rgba8();
        assert_eq!(decoded.width(), 2);
        assert_eq!(decoded.height(), 2);
        let p = decoded.get_pixel(0, 0);
        assert_eq!(p[0], 30);
        assert_eq!(p[1], 20);
        assert_eq!(p[2], 10);
        assert_eq!(p[3], 180, "alpha channel should be preserved");
    }

    #[test]
    fn top_down_dib_decodes_with_correct_orientation() {
        // Top-down: negative height
        let width: i32 = 2;
        let height: i32 = -2;
        let w = width as usize;
        let h = 2usize;
        let row_stride = w * 4;
        let mut dib = vec![0u8; 40 + row_stride * h];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&width.to_le_bytes());
        dib[8..12].copy_from_slice(&height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());

        // Top row: red [0, 0, 255, 255]
        for x in 0..w {
            let px = 40 + x * 4;
            dib[px..px + 4].copy_from_slice(&[0, 0, 255, 255]);
        }
        // Bottom row: blue [255, 0, 0, 255]
        for x in 0..w {
            let px = 40 + row_stride + x * 4;
            dib[px..px + 4].copy_from_slice(&[255, 0, 0, 255]);
        }

        let png = super::decode_dib_to_png(&dib).expect("png");
        let decoded = image::load_from_memory(&png).expect("decode").to_rgba8();
        let top = decoded.get_pixel(0, 0);
        let bottom = decoded.get_pixel(0, 1);
        assert_eq!(top[0], 255, "top row is red");
        assert_eq!(bottom[2], 255, "bottom row is blue");
    }

    #[test]
    fn invalid_or_truncated_dib_returns_none() {
        assert!(super::decode_dib_to_png(&[]).is_none());
        assert!(super::decode_dib_to_png(&[0u8; 10]).is_none());
        assert!(super::decode_dib_to_png(&[40, 0, 0, 0]).is_none());
    }

    static CLIPBOARD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_clipboard_dib_round_trip() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let previous = arboard::Clipboard::new().ok().and_then(|mut cb| cb.get_text().ok());
        let _restore = Restore(previous);

        let dib = make_cf_dib_24(2, 2, &[0, 0, 255]); // red
        {
            let _clip = clipboard_win::Clipboard::new_attempts(10).expect("open clipboard");
            let _ = clipboard_win::raw::empty();
            clipboard_win::raw::set_without_clear(clipboard_win::formats::CF_DIB, &dib).expect("set CF_DIB");
        }

        let png = super::image_png().expect("read image from windows clipboard");
        let decoded = image::load_from_memory(&png).expect("decode png").to_rgba8();
        assert_eq!(decoded.width(), 2);
        assert_eq!(decoded.height(), 2);
        let p = decoded.get_pixel(0, 0);
        assert!(p[0] > 200 && p[1] < 40 && p[2] < 40 && p[3] > 200, "pixel should be red: {p:?}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_clipboard_dibv5_round_trip() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let previous = arboard::Clipboard::new().ok().and_then(|mut cb| cb.get_text().ok());
        let _restore = Restore(previous);

        let dib = make_cf_dibv5_32(2, 2, &[0, 255, 0, 180], 0xff000000); // green with alpha 180
        {
            let _clip = clipboard_win::Clipboard::new_attempts(10).expect("open clipboard");
            let _ = clipboard_win::raw::empty();
            clipboard_win::raw::set_without_clear(clipboard_win::formats::CF_DIBV5, &dib).expect("set CF_DIBV5");
        }

        let png = super::image_png().expect("read dibv5 from windows clipboard");
        let decoded = image::load_from_memory(&png).expect("decode png").to_rgba8();
        assert_eq!(decoded.width(), 2);
        assert_eq!(decoded.height(), 2);
        let p = decoded.get_pixel(0, 0);
        assert!(p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] == 180, "pixel should be green with alpha 180: {p:?}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_clipboard_zero_alpha_snipping_tool_round_trip() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let previous = arboard::Clipboard::new().ok().and_then(|mut cb| cb.get_text().ok());
        let _restore = Restore(previous);

        let dib = make_cf_dib_32(2, 2, &[255, 0, 0, 0]); // blue with 0 alpha
        {
            let _clip = clipboard_win::Clipboard::new_attempts(10).expect("open clipboard");
            let _ = clipboard_win::raw::empty();
            clipboard_win::raw::set_without_clear(clipboard_win::formats::CF_DIB, &dib).expect("set CF_DIB");
        }

        let png = super::image_png().expect("read image from windows clipboard");
        let decoded = image::load_from_memory(&png).expect("decode png").to_rgba8();
        let p = decoded.get_pixel(0, 0);
        assert!(p[0] < 40 && p[1] < 40 && p[2] > 200 && p[3] == 255, "zero alpha restored to opaque: {p:?}");
    }

    /// Puts HTML and a 1×1 picture on the macOS pasteboard and reads them back.
    /// Restores whatever plain text was there. Other platforms return immediately.
    #[test]
    fn html_and_a_picture_round_trip_on_macos() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let previous = arboard::Clipboard::new().ok().and_then(|mut cb| cb.get_text().ok());
        let _restore = Restore(previous);
        assert!(super::set_html("<b>DeckCraft</b>", "DeckCraft"), "set html");
        let html = super::html().unwrap_or_default();
        assert!(html.contains("<b>") || html.contains("<B>"), "{html}");
        let text = arboard::Clipboard::new().ok().and_then(|mut cb| cb.get_text().ok()).unwrap_or_default();
        assert_eq!(text, "DeckCraft");
        let mut cb = arboard::Clipboard::new().expect("pasteboard");
        let img = arboard::ImageData { width: 1, height: 1, bytes: Cow::Owned(vec![255, 0, 0, 255]) };
        cb.set_image(img).expect("set image");
        drop(cb);
        let png = super::image_png().expect("png");
        let decoded = image::load_from_memory(&png).expect("decode").to_rgba8();
        let p = decoded.get_pixel(0, 0);
        assert!(p[0] > 200 && p[1] < 40 && p[3] > 200, "{p:?}");
    }

    /// Puts the previous plain text back, including when the test fails halfway.
    struct Restore(Option<String>);

    impl Drop for Restore {
        fn drop(&mut self) {
            let Ok(mut cb) = arboard::Clipboard::new() else { return };
            match self.0.take() {
                Some(t) => {
                    let _ = cb.set_text(t);
                }
                None => {
                    let _ = cb.clear();
                }
            }
        }
    }
}
