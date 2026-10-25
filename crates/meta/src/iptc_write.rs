//! IPTC keywords as Photoshop image resources (the `0x0404` block of a JPEG APP13 segment) and as
//! the `Raw profile type iptc` text of a PNG. Only the keyword and version records are rewritten.

const RES_IPTC: u16 = 0x0404;
const IIM_MARKER: u8 = 0x1c;
const IIM_CHARSET: u8 = 90;
const IIM_VERSION: u8 = 0;
const IIM_KEYWORD: u8 = 25;
const KEYWORD_MAX: usize = 64;
const UTF8_CHARSET: &[u8] = b"\x1b%G";
const BAD: &str = "unreadable IPTC data";

/// `resources` (the Photoshop image resource blocks of a JPEG APP13 segment, after its
/// `Photoshop 3.0` header) with the IPTC keywords replaced by `keywords`. An empty list removes
/// them; other resource blocks and IIM records are copied as they are. Returns an empty vector when
/// nothing is left.
pub(crate) fn rewrite_resources(resources: &[u8], keywords: &[String]) -> Result<Vec<u8>, &'static str> {
    let values: Vec<&str> = keywords.iter().map(|k| cut(k.trim())).filter(|k| !k.is_empty()).collect();
    let mut out = Vec::with_capacity(resources.len() + 64);
    let mut iim_found = false;
    let mut i = 0usize;
    while i < resources.len() {
        if resources.get(i..i + 4) != Some(b"8BIM") {
            return Err(BAD);
        }
        let id = be16(resources, i + 4).ok_or(BAD)?;
        let name_len = usize::from(*resources.get(i + 6).ok_or(BAD)?);
        let size_at = i + 6 + ((2 + name_len) & !1);
        let size = usize::try_from(be32(resources, size_at).ok_or(BAD)?).map_err(|_| BAD)?;
        let data_start = size_at + 4;
        let data = resources.get(data_start..data_start + size).ok_or(BAD)?;
        let next = (data_start + size + (size & 1)).min(resources.len());
        if id == RES_IPTC {
            iim_found = true;
            let iim = build_iim(&iim_records(data)?, &values)?;
            if !iim.is_empty() {
                push_block(&mut out, id, &iim)?;
            }
        } else {
            out.extend_from_slice(resources.get(i..next).ok_or(BAD)?);
        }
        i = next;
    }
    if !iim_found {
        let iim = build_iim(&[], &values)?;
        if !iim.is_empty() {
            push_block(&mut out, RES_IPTC, &iim)?;
        }
    }
    Ok(out)
}

/// ExifTool's text form of a binary profile, as a PNG `Raw profile type` chunk stores it.
pub(crate) fn encode_raw_profile(data: &[u8]) -> Vec<u8> {
    let mut out = format!("\nIPTC profile\n{:>8}\n", data.len()).into_bytes();
    for line in data.chunks(36) {
        for b in line {
            out.extend_from_slice(format!("{b:02x}").as_bytes());
        }
        out.push(b'\n');
    }
    out
}

/// The binary profile in a PNG `Raw profile type` text, or `None` when the text is malformed.
pub(crate) fn decode_raw_profile(text: &[u8]) -> Option<Vec<u8>> {
    let body = text.strip_prefix(b"\nIPTC profile\n")?;
    let nl = body.iter().position(|&c| c == b'\n')?;
    let (len_line, hex) = body.split_at(nl);
    let len: usize = std::str::from_utf8(len_line).ok()?.trim().parse().ok()?;
    let digits: Vec<u8> = hex.iter().copied().filter(u8::is_ascii_hexdigit).collect();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    let bytes = digits.chunks(2).map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()).collect::<Option<Vec<u8>>>()?;
    (bytes.len() == len).then_some(bytes)
}

/// The IIM records of an IPTC block as (record, dataset, value).
fn iim_records(b: &[u8]) -> Result<Vec<(u8, u8, &[u8])>, &'static str> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let head: &[u8; 5] = b.get(i..i + 5).and_then(|s| s.try_into().ok()).ok_or(BAD)?;
        let [marker, record, dataset, hi, lo] = *head;
        if marker != IIM_MARKER || hi & 0x80 != 0 {
            return Err(BAD);
        }
        let len = usize::from(u16::from_be_bytes([hi, lo]));
        out.push((record, dataset, b.get(i + 5..i + 5 + len).ok_or(BAD)?));
        i += 5 + len;
    }
    Ok(out)
}

/// The records of an IIM block with the version and keyword records replaced by `values`; a
/// UTF-8 charset record is added when a keyword needs one and none is present.
fn build_iim(records: &[(u8, u8, &[u8])], values: &[&str]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::new();
    let has_charset = records.iter().any(|&(r, d, _)| r == 1 && d == IIM_CHARSET);
    if !has_charset && values.iter().any(|v| !v.is_ascii()) {
        push_iim(&mut out, 1, IIM_CHARSET, UTF8_CHARSET)?;
    }
    for &(record, dataset, value) in records {
        if record == 2 && (dataset == IIM_VERSION || dataset == IIM_KEYWORD) {
            continue;
        }
        push_iim(&mut out, record, dataset, value)?;
    }
    if !values.is_empty() {
        push_iim(&mut out, 2, IIM_VERSION, &[0, 4])?;
        for value in values {
            push_iim(&mut out, 2, IIM_KEYWORD, value.as_bytes())?;
        }
    }
    Ok(out)
}

fn push_iim(out: &mut Vec<u8>, record: u8, dataset: u8, value: &[u8]) -> Result<(), &'static str> {
    let len = u16::try_from(value.len()).map_err(|_| BAD)?;
    out.extend_from_slice(&[IIM_MARKER, record, dataset]);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value);
    Ok(())
}

fn push_block(out: &mut Vec<u8>, id: u16, data: &[u8]) -> Result<(), &'static str> {
    let size = u32::try_from(data.len()).map_err(|_| BAD)?;
    out.extend_from_slice(b"8BIM");
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(data);
    if !data.len().is_multiple_of(2) {
        out.push(0);
    }
    Ok(())
}

/// A keyword cut to the IPTC limit of 64 bytes, at a character boundary.
fn cut(s: &str) -> &str {
    let mut end = s.len().min(KEYWORD_MAX);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default()
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    b.get(at..at + 2).and_then(|s| s.try_into().ok()).map(u16::from_be_bytes)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).and_then(|s| s.try_into().ok()).map(u32::from_be_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The data of the single `0x0404` block in `out`, without its pad byte.
    fn block_data(out: &[u8]) -> &[u8] {
        let size = be32(out, 8).map_or(0, |s| usize::try_from(s).unwrap_or(0));
        out.get(12..12 + size).unwrap_or_default()
    }

    #[test]
    fn raw_profile_round_trips() {
        let data: Vec<u8> = (0..100u8).collect();
        assert_eq!(decode_raw_profile(&encode_raw_profile(&data)), Some(data));
        assert_eq!(decode_raw_profile(b"\nIPTC profile\n       9\nzz\n"), None);
    }

    #[test]
    fn keywords_replace_old_ones_and_keep_other_records() {
        let mut iim = Vec::new();
        push_iim(&mut iim, 2, 25, b"old").expect("record");
        push_iim(&mut iim, 2, 120, b"caption").expect("record");
        let mut resources = Vec::new();
        push_block(&mut resources, RES_IPTC, &iim).expect("block");
        let out = rewrite_resources(&resources, &["Lisbon".to_owned(), " ".to_owned()]).expect("rewrite");
        let block = block_data(&out);
        let records = iim_records(block).expect("records");
        let keywords: Vec<&[u8]> = records.iter().filter(|r| r.1 == 25).map(|r| r.2).collect();
        assert_eq!(keywords, vec![b"Lisbon".as_slice()]);
        assert!(records.iter().any(|r| r.0 == 2 && r.1 == 120));
    }

    #[test]
    fn empty_keyword_list_removes_the_block_when_nothing_else_is_left() {
        let mut iim = Vec::new();
        push_iim(&mut iim, 2, 25, b"old").expect("record");
        let mut resources = Vec::new();
        push_block(&mut resources, RES_IPTC, &iim).expect("block");
        assert!(rewrite_resources(&resources, &[]).expect("rewrite").is_empty());
    }

    #[test]
    fn keywords_are_cut_to_64_bytes_on_a_character_boundary() {
        let long = format!("{}é", "a".repeat(63));
        let out = rewrite_resources(&[], &[long]).expect("rewrite");
        let records = iim_records(block_data(&out)).expect("records");
        let keyword = records.iter().find(|r| r.1 == 25).map(|r| r.2.len());
        assert_eq!(keyword, Some(63));
    }
}
