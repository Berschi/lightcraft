//! Replacing the XMP packet and the IPTC keywords of a JPEG or PNG file in place. Every other segment,
//! chunk and the compressed pixel data are copied byte for byte, so nothing is re-encoded.

use crate::iptc_write::{decode_raw_profile, encode_raw_profile, rewrite_resources};

const JPEG_XMP_SIG: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const JPEG_EXIF_SIG: &[u8] = b"Exif\0\0";
const JPEG_PHOTOSHOP_SIG: &[u8] = b"Photoshop 3.0\0";
const JPEG_APP13: u8 = 0xed;
const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
const PNG_XMP_KEY: &[u8] = b"XML:com.adobe.xmp\0";
const PNG_IPTC_KEY: &[u8] = b"Raw profile type iptc\0";
/// Largest IPTC profile read back from a PNG; keeps a hostile zlib stream from exhausting memory.
const MAX_PROFILE_BYTES: usize = 16 << 20;
const JPEG_TOO_LARGE: &str = "the data is too large for a JPEG segment";
const PNG_TOO_LARGE: &str = "the data is too large for a PNG chunk";

/// `bytes` with its XMP packet replaced by `packet`. The packet is written as the main XMP
/// segment of a JPEG (right after SOI and a JFIF APP0 segment, and any extended XMP is kept), or as
/// the `XML:com.adobe.xmp` iTXt chunk of a PNG (right after IHDR). Returns why the file can't carry
/// the packet, and leaves the caller's bytes alone in that case.
pub fn set_xmp(bytes: &[u8], packet: &str) -> Result<Vec<u8>, &'static str> {
    if bytes.starts_with(&[0xff, 0xd8]) {
        let mut payload = JPEG_XMP_SIG.to_vec();
        payload.extend_from_slice(packet.as_bytes());
        put_jpeg_segment(bytes, 0xe1, JPEG_XMP_SIG, Some(&payload))
    } else if bytes.starts_with(PNG_SIG) {
        let mut data = PNG_XMP_KEY.to_vec();
        // uncompressed, compression method 0, empty language tag, empty translated keyword
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.extend_from_slice(packet.as_bytes());
        put_png_chunk(bytes, b"iTXt", PNG_XMP_KEY, Some(&data))
    } else {
        Err("only JPEG and PNG files can hold XMP in the file")
    }
}

/// `bytes` with its EXIF block replaced by `exif_block` (a TIFF-structured Exif block, without
/// the `Exif\0\0` prefix).
pub fn set_exif(bytes: &[u8], exif_block: &[u8]) -> Result<Vec<u8>, &'static str> {
    if bytes.starts_with(&[0xff, 0xd8]) {
        let mut payload = JPEG_EXIF_SIG.to_vec();
        payload.extend_from_slice(exif_block);
        put_jpeg_segment(bytes, 0xe1, JPEG_EXIF_SIG, Some(&payload))
    } else if bytes.starts_with(PNG_SIG) {
        put_png_chunk(bytes, b"eXIf", b"", Some(exif_block))
    } else {
        Err("only JPEG and PNG files can hold EXIF in the file")
    }
}

/// `bytes` with the IPTC keywords (the file's own flat tag list, as Photoshop-era tools read it)
/// replaced by `keywords`; an empty list removes them. Other IPTC records and Photoshop resources
/// are kept. A JPEG gets them in its APP13 Photoshop segment, a PNG in its `Raw profile type iptc`
/// text chunk, the form ExifTool writes. Leaves the caller's bytes alone when the file can't carry them.
pub fn set_iptc_keywords(bytes: &[u8], keywords: &[String]) -> Result<Vec<u8>, &'static str> {
    if bytes.starts_with(&[0xff, 0xd8]) {
        set_jpeg_iptc(bytes, keywords)
    } else if bytes.starts_with(PNG_SIG) {
        set_png_iptc(bytes, keywords)
    } else {
        Err("only JPEG and PNG files can hold IPTC keywords in the file")
    }
}

/// The Photoshop image resources in a PNG's `Raw profile type iptc` chunk, if it has one.
pub(crate) fn png_iptc_resources(b: &[u8]) -> Result<Option<Vec<u8>>, &'static str> {
    const BAD: &str = "unreadable IPTC profile in the PNG";
    let chunks = png_layout(b)?;
    let mut hits = chunks.iter().filter(|c| c.typ == *b"zTXt" && c.data.starts_with(PNG_IPTC_KEY));
    let Some(chunk) = hits.next() else {
        return Ok(None);
    };
    if hits.next().is_some() {
        return Err("several IPTC profiles in the PNG");
    }
    let (&method, z) = chunk.data.get(PNG_IPTC_KEY.len()..).and_then(|r| r.split_first()).ok_or(BAD)?;
    if method != 0 {
        return Err(BAD);
    }
    let text = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(z, MAX_PROFILE_BYTES).map_err(|_| BAD)?;
    decode_raw_profile(&text).map(Some).ok_or(BAD)
}

fn set_jpeg_iptc(b: &[u8], keywords: &[String]) -> Result<Vec<u8>, &'static str> {
    let (segs, _) = jpeg_layout(b)?;
    let mut hits = segs.iter().filter(|s| s.marker == JPEG_APP13 && seg_payload(b, s).starts_with(JPEG_PHOTOSHOP_SIG));
    let existing = hits.next().copied();
    if hits.next().is_some() {
        return Err("several Photoshop segments in the file");
    }
    let resources: &[u8] = existing.map_or(&[], |s| seg_payload(b, &s).get(JPEG_PHOTOSHOP_SIG.len()..).unwrap_or_default());
    let iptc = rewrite_resources(resources, keywords)?;
    if iptc.is_empty() {
        return put_jpeg_segment(b, JPEG_APP13, JPEG_PHOTOSHOP_SIG, None);
    }
    let mut payload = JPEG_PHOTOSHOP_SIG.to_vec();
    payload.extend_from_slice(&iptc);
    put_jpeg_segment(b, JPEG_APP13, JPEG_PHOTOSHOP_SIG, Some(&payload))
}

fn set_png_iptc(b: &[u8], keywords: &[String]) -> Result<Vec<u8>, &'static str> {
    let resources = png_iptc_resources(b)?.unwrap_or_default();
    let iptc = rewrite_resources(&resources, keywords)?;
    if iptc.is_empty() {
        return put_png_chunk(b, b"zTXt", PNG_IPTC_KEY, None);
    }
    let mut data = PNG_IPTC_KEY.to_vec();
    // compression method 0 (deflate), then the zlib stream of the hex-encoded profile
    data.push(0);
    data.extend_from_slice(&miniz_oxide::deflate::compress_to_vec_zlib(&encode_raw_profile(&iptc), 6));
    put_png_chunk(b, b"zTXt", PNG_IPTC_KEY, Some(&data))
}

/// A JPEG marker segment: its offsets (marker bytes included) and marker code.
#[derive(Clone, Copy)]
struct JpegSeg {
    start: usize,
    end: usize,
    marker: u8,
}

/// The marker segments before the scan data, and the offset where the scan data starts.
fn jpeg_layout(b: &[u8]) -> Result<(Vec<JpegSeg>, usize), &'static str> {
    let mut segs = Vec::new();
    let mut i = 2usize;
    loop {
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
            0xd9 | 0xda => return Ok((segs, i)),
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
        segs.push(JpegSeg { start: i, end, marker });
        i = end;
    }
}

/// A segment's payload: the bytes after its marker and length field.
fn seg_payload<'a>(b: &'a [u8], s: &JpegSeg) -> &'a [u8] {
    b.get(s.start + 4..s.end).unwrap_or_default()
}

/// `b` with every segment of `marker` whose payload starts with `sig` removed, and `payload` (which
/// starts with `sig` too) added as a new one after SOI and any JFIF APP0. `None` only removes.
fn put_jpeg_segment(b: &[u8], marker: u8, sig: &[u8], payload: Option<&[u8]>) -> Result<Vec<u8>, &'static str> {
    let (segs, scan) = jpeg_layout(b)?;
    let insert_at = match segs.first() {
        Some(s) if s.start == 2 && s.marker == 0xe0 => s.end,
        _ => 2,
    };
    let mut out = Vec::with_capacity(b.len() + payload.map_or(0, <[u8]>::len) + 4);
    out.extend_from_slice(b.get(..insert_at).ok_or("truncated JPEG")?);
    if let Some(payload) = payload {
        let len = payload.len().checked_add(2).and_then(|n| u16::try_from(n).ok()).ok_or(JPEG_TOO_LARGE)?;
        out.extend_from_slice(&[0xff, marker]);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(payload);
    }
    for s in &segs {
        if s.start < insert_at || (s.marker == marker && seg_payload(b, s).starts_with(sig)) {
            continue;
        }
        out.extend_from_slice(b.get(s.start..s.end).ok_or("truncated JPEG segment")?);
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

/// All chunks of a PNG, from IHDR to IEND.
fn png_layout(b: &[u8]) -> Result<Vec<PngChunk<'_>>, &'static str> {
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
    match chunks.first() {
        Some(c) if &c.typ == b"IHDR" => Ok(chunks),
        Some(_) => Err("PNG without an IHDR chunk"),
        None => Err("truncated PNG"),
    }
}

/// `b` with every chunk of type `typ` whose data starts with `key` removed, and a new chunk with
/// `data` (which starts with `key` too) added right after IHDR. `None` only removes.
fn put_png_chunk(b: &[u8], typ: &[u8; 4], key: &[u8], data: Option<&[u8]>) -> Result<Vec<u8>, &'static str> {
    let chunks = png_layout(b)?;
    let insert_at = chunks.first().map_or(PNG_SIG.len(), |c| c.end);
    let mut out = Vec::with_capacity(b.len() + data.map_or(0, <[u8]>::len) + 12);
    out.extend_from_slice(b.get(..insert_at).ok_or("truncated PNG")?);
    if let Some(data) = data {
        out.extend_from_slice(&png_chunk(typ, data)?);
    }
    for c in chunks.iter().skip(1) {
        if &c.typ == typ && c.data.starts_with(key) {
            continue;
        }
        out.extend_from_slice(b.get(c.start..c.end).ok_or("truncated PNG chunk")?);
    }
    let last_end = chunks.last().map_or(insert_at, |c| c.end);
    out.extend_from_slice(b.get(last_end..).unwrap_or_default());
    Ok(out)
}

/// A complete PNG chunk: length, type, data and CRC.
fn png_chunk(typ: &[u8; 4], data: &[u8]) -> Result<Vec<u8>, &'static str> {
    let len = u32::try_from(data.len()).map_err(|_| PNG_TOO_LARGE)?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(typ);
    crc.update(data);
    let mut out = Vec::with_capacity(data.len() + 12);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
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

    #[test]
    fn jpeg_iptc_keywords_round_trip_and_an_empty_list_removes_them() {
        let kws = vec!["Lisbon".to_string(), "Trip".to_string()];
        let out = set_iptc_keywords(&jpeg(), &kws).expect("jpeg");
        let iim = embedded(&out).iptc.expect("IPTC block");
        assert_eq!(crate::iptc::parse_iptc(&iim).keywords, kws);
        let cleared = set_iptc_keywords(&out, &[]).expect("jpeg again");
        assert!(embedded(&cleared).iptc.is_none());
    }

    #[test]
    fn png_iptc_keywords_are_stored_in_a_raw_profile_chunk() {
        let kws = vec!["Lisbon".to_string()];
        let out = set_iptc_keywords(&png(), &kws).expect("png");
        let resources = png_iptc_resources(&out).expect("readable").expect("profile present");
        assert!(resources.windows(b"Lisbon".len()).any(|w| w == b"Lisbon"));
        let cleared = set_iptc_keywords(&out, &[]).expect("png again");
        assert_eq!(png_iptc_resources(&cleared), Ok(None));
    }

    #[test]
    fn unreadable_photoshop_segment_is_an_error_and_leaves_the_file_alone() {
        let mut payload = JPEG_PHOTOSHOP_SIG.to_vec();
        payload.extend_from_slice(b"not an 8BIM block");
        let bad = put_jpeg_segment(&jpeg(), JPEG_APP13, JPEG_PHOTOSHOP_SIG, Some(&payload)).expect("segment");
        assert!(set_iptc_keywords(&bad, &["x".to_string()]).is_err());
    }

    #[test]
    fn iptc_is_refused_for_other_formats() {
        assert!(set_iptc_keywords(b"GIF89a", &["x".to_string()]).is_err());
    }
}
