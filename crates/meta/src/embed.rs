//! Replacing the XMP packet of a JPEG or PNG file in place. Every other segment, chunk and the
//! compressed pixel data are copied byte for byte, so nothing is re-encoded.

const JPEG_XMP_SIG: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
const PNG_XMP_KEY: &[u8] = b"XML:com.adobe.xmp\0";

/// `bytes` with its XMP packet replaced by `packet`. The packet is written as the main XMP
/// segment of a JPEG (right after SOI and a JFIF APP0 segment, and any extended XMP is kept), or as
/// the `XML:com.adobe.xmp` iTXt chunk of a PNG (right after IHDR). Returns why the file can't carry
/// the packet, and leaves the caller's bytes alone in that case.
pub fn set_xmp(bytes: &[u8], packet: &str) -> Result<Vec<u8>, &'static str> {
    if bytes.starts_with(&[0xff, 0xd8]) {
        set_jpeg_xmp(bytes, packet)
    } else if bytes.starts_with(PNG_SIG) {
        set_png_xmp(bytes, packet)
    } else {
        Err("only JPEG and PNG files can hold XMP in the file")
    }
}

fn set_jpeg_xmp(b: &[u8], packet: &str) -> Result<Vec<u8>, &'static str> {
    let seg_len = JPEG_XMP_SIG
        .len()
        .checked_add(packet.len())
        .and_then(|n| n.checked_add(2))
        .filter(|&n| n <= usize::from(u16::MAX))
        .ok_or("the XMP packet is too large for a JPEG segment")?;

    // marker segments up to the scan data: (start, end, marker)
    let mut segs: Vec<(usize, usize, u8)> = Vec::new();
    let mut i = 2usize;
    let scan = loop {
        let marker = match b.get(i..i + 2) {
            Some(&[0xff, m]) => m,
            _ => return Err("malformed JPEG marker"),
        };
        match marker {
            0xff => {
                i += 1;
                continue;
            }
            0xd8 | 0x01 | 0xd0..=0xd7 => {
                i += 2;
                continue;
            }
            0xd9 | 0xda => break i,
            _ => {}
        }
        let len = match b.get(i + 2..i + 4) {
            Some(&[hi, lo]) => usize::from(u16::from_be_bytes([hi, lo])),
            _ => return Err("truncated JPEG"),
        };
        if len < 2 {
            return Err("malformed JPEG segment");
        }
        let end = i.checked_add(2 + len).filter(|&e| e <= b.len()).ok_or("truncated JPEG segment")?;
        segs.push((i, end, marker));
        i = end;
    };

    // a JFIF APP0 must stay first
    let insert_at = match segs.first() {
        Some(&(2, end, 0xe0)) => end,
        _ => 2,
    };
    let head = b.get(..insert_at).ok_or("truncated JPEG")?;
    let mut out = Vec::with_capacity(b.len() + seg_len + 2);
    out.extend_from_slice(head);
    out.extend_from_slice(&[0xff, 0xe1]);
    out.extend_from_slice(&u16::try_from(seg_len).map_err(|_| "the XMP packet is too large for a JPEG segment")?.to_be_bytes());
    out.extend_from_slice(JPEG_XMP_SIG);
    out.extend_from_slice(packet.as_bytes());
    for &(start, end, marker) in &segs {
        if start < insert_at {
            continue;
        }
        let payload = b.get(start + 4..end).unwrap_or_default();
        if marker == 0xe1 && payload.starts_with(JPEG_XMP_SIG) {
            continue;
        }
        out.extend_from_slice(b.get(start..end).ok_or("truncated JPEG segment")?);
    }
    out.extend_from_slice(b.get(scan..).ok_or("truncated JPEG")?);
    Ok(out)
}

struct PngChunk<'a> {
    start: usize,
    end: usize,
    typ: [u8; 4],
    data: &'a [u8],
}

fn set_png_xmp(b: &[u8], packet: &str) -> Result<Vec<u8>, &'static str> {
    let mut chunks: Vec<PngChunk> = Vec::new();
    let mut i = PNG_SIG.len();
    loop {
        let len = b.get(i..i + 4).and_then(|s| <[u8; 4]>::try_from(s).ok()).map(u32::from_be_bytes).ok_or("truncated PNG")?;
        let typ = b.get(i + 4..i + 8).and_then(|s| <[u8; 4]>::try_from(s).ok()).ok_or("truncated PNG")?;
        let len = usize::try_from(len).map_err(|_| "truncated PNG")?;
        let end = i.checked_add(12 + len).filter(|&e| e <= b.len()).ok_or("truncated PNG chunk")?;
        let data = b.get(i + 8..i + 8 + len).ok_or("truncated PNG chunk")?;
        chunks.push(PngChunk { start: i, end, typ, data });
        if &typ == b"IEND" {
            break;
        }
        i = end;
    }
    let first = chunks.first().ok_or("truncated PNG")?;
    if &first.typ != b"IHDR" {
        return Err("PNG without an IHDR chunk");
    }
    let insert_at = first.end;

    let mut data = Vec::with_capacity(PNG_XMP_KEY.len() + 4 + packet.len());
    data.extend_from_slice(PNG_XMP_KEY);
    // uncompressed, compression method 0, empty language tag, empty translated keyword
    data.extend_from_slice(&[0, 0, 0, 0]);
    data.extend_from_slice(packet.as_bytes());
    let data_len = u32::try_from(data.len()).map_err(|_| "the XMP packet is too large for a PNG chunk")?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(b"iTXt");
    crc.update(&data);

    let mut out = Vec::with_capacity(b.len() + data.len() + 12);
    out.extend_from_slice(b.get(..insert_at).ok_or("truncated PNG")?);
    out.extend_from_slice(&data_len.to_be_bytes());
    out.extend_from_slice(b"iTXt");
    out.extend_from_slice(&data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
    for c in chunks.iter().skip(1) {
        if &c.typ == b"iTXt" && c.data.starts_with(PNG_XMP_KEY) {
            continue;
        }
        out.extend_from_slice(b.get(c.start..c.end).ok_or("truncated PNG chunk")?);
    }
    let last_end = chunks.last().map_or(insert_at, |c| c.end);
    out.extend_from_slice(b.get(last_end..).unwrap_or_default());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedded;

    const PACKET: &str = "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?><x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/><?xpacket end=\"w\"?>";

    fn jpeg() -> Vec<u8> {
        let mut v = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
        v.extend_from_slice(b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00");
        v.extend_from_slice(&[0xff, 0xdb, 0x00, 0x03, 0x00]);
        v.extend_from_slice(&[0xff, 0xda, 0x00, 0x02, 0x11, 0x22, 0x33]);
        v.extend_from_slice(&[0xff, 0xd9]);
        v
    }

    fn png() -> Vec<u8> {
        let mut v = PNG_SIG.to_vec();
        let mut chunk = |typ: &[u8; 4], data: &[u8]| {
            v.extend_from_slice(&u32::try_from(data.len()).unwrap_or_default().to_be_bytes());
            v.extend_from_slice(typ);
            v.extend_from_slice(data);
            let mut c = crc32fast::Hasher::new();
            c.update(typ);
            c.update(data);
            v.extend_from_slice(&c.finalize().to_be_bytes());
        };
        chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
        chunk(b"IEND", &[]);
        v
    }

    #[test]
    fn jpeg_xmp_is_written_once_after_app0_and_replaces_an_old_packet() {
        let first = set_xmp(&jpeg(), "<old/>").expect("jpeg");
        let second = set_xmp(&first, PACKET).expect("jpeg again");
        assert_eq!(embedded(&second).xmp.as_deref(), Some(PACKET));
        assert_eq!(second.windows(JPEG_XMP_SIG.len()).filter(|w| *w == JPEG_XMP_SIG).count(), 1);
        assert_eq!(&second[2..4], &[0xff, 0xe0], "the JFIF APP0 stays first");
        assert!(second.ends_with(&[0xff, 0xd9]));
    }

    #[test]
    fn jpeg_pixels_are_copied_unchanged() {
        let src = jpeg();
        let out = set_xmp(&src, PACKET).expect("jpeg");
        let scan = src.windows(2).position(|w| w == [0xff, 0xda]).unwrap_or_default();
        assert!(out.ends_with(&src[scan..]));
    }

    #[test]
    fn png_xmp_is_written_after_ihdr_with_a_valid_crc() {
        let out = set_xmp(&png(), PACKET).expect("png");
        assert_eq!(embedded(&out).xmp.as_deref(), Some(PACKET));
        let again = set_xmp(&out, PACKET).expect("png again");
        assert_eq!(again.windows(4).filter(|w| *w == b"iTXt").count(), 1);
        let typ_at = again.windows(4).position(|w| w == b"iTXt").expect("iTXt chunk");
        let len_at = typ_at - 4;
        let len = usize::try_from(u32::from_be_bytes(again[len_at..typ_at].try_into().expect("4 bytes"))).expect("usize");
        let typed = &again[typ_at..typ_at + 4 + len];
        let stored = u32::from_be_bytes(again[typ_at + 4 + len..typ_at + 8 + len].try_into().expect("4 bytes"));
        assert_eq!(crc32fast::hash(typed), stored);
    }

    #[test]
    fn unsupported_or_truncated_input_is_an_error() {
        assert!(set_xmp(b"GIF89a", PACKET).is_err());
        assert!(set_xmp(&[0xff, 0xd8, 0xff, 0xe1, 0x00], PACKET).is_err());
        assert!(set_xmp(PNG_SIG, PACKET).is_err());
    }
}
