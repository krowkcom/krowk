//! An image's pixel size, read out of the front of its bytes the way the Go
//! stand-in read it — `image.DecodeConfig` for PNG, GIF and JPEG, and headers
//! by hand for WebP and SVG. The registry measures all five, and a stand-in
//! that measured differently would let a client pass here and find no
//! dimensions in production.
//!
//! Only headers are read; nothing here decodes pixels.

/// How much of an image is read to measure it: the registry's number, set by
/// how much EXIF a JPEG carries in front of its size marker.
const DIMENSION_HEADER_BYTES: usize = 64 << 10;

/// The dimensions, or (0, 0) for a non-image and for any header that does not
/// parse — a measurement must never fail a push.
pub fn image_size(content_type: &str, body: &[u8]) -> (i64, i64) {
    let mime = content_type.split(';').next().unwrap_or("").trim().to_lowercase();
    if !mime.starts_with("image/") {
        return (0, 0);
    }
    let body = &body[..body.len().min(DIMENSION_HEADER_BYTES)];
    let found = webp(body).or_else(|| if mime == "image/svg+xml" { svg(body) } else { None }).or_else(|| {
        // DecodeConfig dispatches on the magic, whatever the declared type.
        if body.starts_with(b"GIF8") && body.len() > 5 && body[5] == b'a' {
            gif(body)
        } else if body.starts_with(b"\xff\xd8") {
            jpeg(body)
        } else if body.starts_with(b"\x89PNG\r\n\x1a\n") {
            png(body)
        } else {
            None
        }
    });
    found.unwrap_or((0, 0))
}

fn le16(b: &[u8]) -> i64 {
    u16::from_le_bytes([b[0], b[1]]) as i64
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// A WebP is a RIFF container whose first chunk names one of three encodings,
/// each stating the canvas in its first bytes.
pub fn webp(b: &[u8]) -> Option<(i64, i64)> {
    if b.len() < 30 || &b[0..4] != b"RIFF" || &b[8..12] != b"WEBP" {
        return None;
    }
    match &b[12..16] {
        // Lossy: a frame tag, a keyframe sync code, then two 14-bit sizes.
        b"VP8 " => {
            if b[23..26] != [0x9d, 0x01, 0x2a] {
                return None;
            }
            let (w, h) = (le16(&b[26..]) & 0x3fff, le16(&b[28..]) & 0x3fff);
            (w > 0 && h > 0).then_some((w, h))
        }
        // Lossless: a signature byte, then width-1 and height-1 in 14 bits each.
        b"VP8L" => {
            if b[20] != 0x2f {
                return None;
            }
            let bits = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
            Some(((bits & 0x3fff) as i64 + 1, ((bits >> 14) & 0x3fff) as i64 + 1))
        }
        // Extended: the canvas outright, as two 24-bit values holding size-1.
        b"VP8X" => {
            let three = |i: usize| (b[i] as i64 | (b[i + 1] as i64) << 8 | (b[i + 2] as i64) << 16) + 1;
            Some((three(24), three(27)))
        }
        _ => None,
    }
}

fn gif(b: &[u8]) -> Option<(i64, i64)> {
    if b.len() < 13 || (&b[..6] != b"GIF87a" && &b[..6] != b"GIF89a") {
        return None;
    }
    // A global colour table has to be all there, as the decoder reads it.
    if b[10] & 0x80 != 0 && b.len() < 13 + 3 * (1 << (1 + (b[10] & 7))) {
        return None;
    }
    Some((le16(&b[6..]), le16(&b[8..])))
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in data {
        c ^= x as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// Go's chunk walk: every chunk's CRC is checked, IHDR is validated, and a
/// paletted image reads on to its PLTE and then a tRNS or IDAT.
fn png(b: &[u8]) -> Option<(i64, i64)> {
    let mut pos = 8;
    let (mut size, mut depth, mut palette) = (None, 0u32, false);
    loop {
        let head = b.get(pos..pos + 8)?;
        let length = be32(head) as usize;
        let kind = &head[4..8];
        let data = b.get(pos + 8..(pos + 8).checked_add(length)?)?;
        let crc = b.get(pos + 8 + length..pos + 12 + length)?;
        if crc32(&b[pos + 4..pos + 8 + length]) != be32(crc) {
            return None;
        }
        pos += 12 + length;
        match kind {
            b"IHDR" => {
                if size.is_some() || length != 13 || data[10] != 0 || data[11] != 0 || data[12] > 1 {
                    return None;
                }
                let (w, h) = (be32(data) as i32, be32(&data[4..]) as i32);
                depth = data[8] as u32;
                let valid = matches!((depth, data[9]), (1 | 2 | 4, 0 | 3) | (8, 0 | 2 | 3 | 4 | 6) | (16, 0 | 2 | 4 | 6));
                // Up to 8 bytes a pixel, so a count that overflows at 8x is refused.
                if w <= 0 || h <= 0 || !valid || (w as i64 * h as i64).checked_mul(8).is_none() {
                    return None;
                }
                size = Some((w as i64, h as i64));
                if data[9] != 3 {
                    return size;
                }
            }
            b"PLTE" => {
                let np = length / 3;
                if size.is_none() || palette || !length.is_multiple_of(3) || np == 0 || np > 256 || np > 1 << depth {
                    return None;
                }
                palette = true;
            }
            b"tRNS" | b"IDAT" => {
                if !palette || (kind == b"tRNS" && length > 256) {
                    return None;
                }
                return size;
            }
            b"IEND" => return None,
            _ if length > 0x7fff_ffff => return None,
            _ => {}
        }
    }
}

/// Go's marker walk in config mode: done at the SOF when an APP0 said JFIF,
/// otherwise at the SOS; and an SOF it would not decode is no measurement.
fn jpeg(b: &[u8]) -> Option<(i64, i64)> {
    let (mut pos, mut jfif, mut size) = (2, false, None);
    loop {
        let mut tmp = [*b.get(pos)?, *b.get(pos + 1)?];
        pos += 2;
        while tmp[0] != 0xff {
            tmp = [tmp[1], *b.get(pos)?];
            pos += 1;
        }
        let mut marker = tmp[1];
        if marker == 0 {
            continue;
        }
        while marker == 0xff {
            marker = *b.get(pos)?;
            pos += 1;
        }
        if marker == 0xd9 {
            return None;
        }
        if (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let n = (*b.get(pos)? as usize) << 8 | *b.get(pos + 1)? as usize;
        let n = n.checked_sub(2)?;
        pos += 2;
        let seg = b.get(pos..pos + n)?;
        pos += n;
        match marker {
            0xc0..=0xc2 => {
                if size.is_some() {
                    return None;
                }
                size = Some(sof(seg)?);
                if jfif {
                    return size;
                }
            }
            0xda => return size,
            0xe0 if n >= 5 => jfif = &seg[..5] == b"JFIF\0",
            0xc4 | 0xdb | 0xdd | 0xe0..=0xef | 0xfe => {}
            _ => return None,
        }
    }
}

/// A start-of-frame segment, refused where Go's `processSOF` refuses it.
fn sof(seg: &[u8]) -> Option<(i64, i64)> {
    let ncomp = match seg.len() {
        9 => 1,
        15 => 3,
        18 => 4,
        _ => return None,
    };
    if seg[0] != 8 || seg[5] as usize != ncomp {
        return None;
    }
    let (mut hs, mut vs) = (vec![], vec![]);
    for i in 0..ncomp {
        let (c, hv, tq) = (seg[6 + 3 * i], seg[7 + 3 * i], seg[8 + 3 * i]);
        if (0..i).any(|j| seg[6 + 3 * j] == c) || tq > 3 {
            return None;
        }
        let (mut h, mut v) = (hv >> 4, hv & 0x0f);
        if !(1..=4).contains(&h) || !(1..=4).contains(&v) || h == 3 || v == 3 {
            return None;
        }
        match (ncomp, i) {
            (1, _) => (h, v) = (1, 1),
            (4, 0) if hv != 0x11 && hv != 0x22 => return None,
            (4, 1 | 2) if hv != 0x11 => return None,
            (4, 3) if (h, v) != (hs[0], vs[0]) => return None,
            _ => {}
        }
        hs.push(h);
        vs.push(v);
    }
    let (mh, mv) = (*hs.iter().max()?, *vs.iter().max()?);
    if ncomp == 3 && (0..3).any(|i| mh % hs[i] != 0 || mv % vs[i] != 0) {
        return None;
    }
    let height = (seg[1] as i64) << 8 | seg[2] as i64;
    let width = (seg[3] as i64) << 8 | seg[4] as i64;
    Some((width, height))
}

/// The width and height an SVG states on its root element. A percentage is
/// not a pixel size, and neither is a viewBox alone.
fn svg(b: &[u8]) -> Option<(i64, i64)> {
    let text = String::from_utf8_lossy(b);
    let (name, attrs) = crate::xml::root_element(&text)?;
    if name != "svg" {
        return None;
    }
    let (mut w, mut h) = (0, 0);
    for (k, v) in attrs {
        match k.as_str() {
            "width" => w = svg_length(&v),
            "height" => h = svg_length(&v),
            _ => {}
        }
    }
    (w > 0 && h > 0).then_some((w, h))
}

/// The leading number of an SVG length — a unit is dropped, since 120pt is 120
/// to a browser laying it out — or 0 for one with no fixed size.
fn svg_length(v: &str) -> i64 {
    let v = v.trim();
    if v.ends_with('%') {
        return 0;
    }
    let digits = v.bytes().take_while(u8::is_ascii_digit).count();
    v[..digits].parse::<i64>().map_or(0, |n| n.max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_png_header_is_read_and_a_corrupt_one_is_not() {
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&[0, 0, 1, 64, 0, 0, 0, 200, 8, 6, 0, 0, 0]);
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0d".to_vec();
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        assert_eq!(image_size("image/png", &png), (320, 200));
        let last = png.len() - 1;
        png[last] ^= 1;
        assert_eq!(image_size("image/png", &png), (0, 0));
    }

    /// A RIFF that is not a WebP, and a WebP truncated or broken, fall through
    /// rather than read whatever is at those offsets.
    #[test]
    fn webp_refuses_what_it_cannot_read() {
        let raw = include_bytes!("../tests/testdata/tiny-vp8.webp");
        assert_eq!(webp(raw), Some((40, 24)));
        let mut not_webp = b"RIFF____AVI ".to_vec();
        not_webp.extend_from_slice(&raw[16..]);
        let mut no_sync = raw.to_vec();
        no_sync[23] = 0;
        for body in [&raw[..20], &not_webp[..], &no_sync[..]] {
            assert_eq!(webp(body), None);
        }
    }

    #[test]
    fn svg_lengths_keep_only_a_fixed_size() {
        assert_eq!(svg_length("120px"), 120);
        assert_eq!(svg_length(" 80 "), 80);
        assert_eq!(svg_length("100%"), 0);
        assert_eq!(svg_length("1.5em"), 1);
        assert_eq!(svg_length("em"), 0);
    }
}
