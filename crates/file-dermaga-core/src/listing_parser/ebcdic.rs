//! EBCDIC detection and translation (code page 037 subset) for mainframe
//! listings that are sent without conversion to ASCII.

/// Heuristic: does this start of a listing look like EBCDIC text?
///
/// Requires no ASCII line feed, a high share of bytes in the EBCDIC
/// letter/digit/space ranges, frequent EBCDIC spaces (0x40) with hardly any
/// ASCII spaces, and an EBCDIC line end (0x15 or 0x25) unless `at_end` says
/// this is the whole listing. ASCII text cannot pass because ASCII letters
/// and digits lie outside the EBCDIC alphanumeric ranges.
pub(crate) fn looks_like_ebcdic(data: &[u8], at_end: bool) -> bool {
    if data.is_empty() || data.contains(&b'\n') {
        return false;
    }
    let (mut alnum, mut space, mut ascii_space, mut eol) = (0usize, 0usize, 0usize, 0usize);
    for &b in data {
        match b {
            0x40 => space += 1,
            0x20 => ascii_space += 1,
            0x15 | 0x25 => eol += 1,
            0x81..=0x89
            | 0x91..=0x99
            | 0xA2..=0xA9
            | 0xC1..=0xC9
            | 0xD1..=0xD9
            | 0xE2..=0xE9
            | 0xF0..=0xF9 => alnum += 1,
            _ => {}
        }
    }
    if eol == 0 && !at_end {
        return false;
    }
    let total = data.len();
    space > 0
        && ascii_space * 4 <= space
        && space * 20 >= total
        && (alnum + space + eol) * 10 >= total * 7
}

/// Translate one EBCDIC (CP037) byte to ASCII. Line ends become `\n`/`\r`,
/// unknown characters become `?`.
pub(crate) fn to_ascii(b: u8) -> u8 {
    match b {
        0x00 => 0,
        0x05 => b'\t',
        0x0D => b'\r',
        0x15 | 0x25 => b'\n',
        0x40 => b' ',
        0x4B => b'.',
        0x4C => b'<',
        0x4D => b'(',
        0x4E => b'+',
        0x4F => b'|',
        0x50 => b'&',
        0x5A => b'!',
        0x5B => b'$',
        0x5C => b'*',
        0x5D => b')',
        0x5E => b';',
        0xB0 => b'^',
        0x60 => b'-',
        0x61 => b'/',
        0x6B => b',',
        0x6C => b'%',
        0x6D => b'_',
        0x6E => b'>',
        0x6F => b'?',
        0x79 => b'`',
        0x7A => b':',
        0x7B => b'#',
        0x7C => b'@',
        0x7D => b'\'',
        0x7E => b'=',
        0x7F => b'"',
        0x81..=0x89 => b'a' + (b - 0x81),
        0x91..=0x99 => b'j' + (b - 0x91),
        0xA1 => b'~',
        0xA2..=0xA9 => b's' + (b - 0xA2),
        0xBA => b'[',
        0xBB => b']',
        0xC0 => b'{',
        0xC1..=0xC9 => b'A' + (b - 0xC1),
        0xD0 => b'}',
        0xD1..=0xD9 => b'J' + (b - 0xD1),
        0xE0 => b'\\',
        0xE2..=0xE9 => b'S' + (b - 0xE2),
        0xF0..=0xF9 => b'0' + (b - 0xF0),
        _ => b'?',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits() {
        let s: Vec<u8> = [
            0xC1, 0xC9, 0xD1, 0xD9, 0xE2, 0xE9, 0x81, 0x89, 0x91, 0x99, 0xA2, 0xA9, 0xF0, 0xF9,
        ]
        .iter()
        .map(|&b| to_ascii(b))
        .collect();
        assert_eq!(s, b"AIJRSZaijrsz09");
    }

    #[test]
    fn ascii_never_detected() {
        let ascii = b"-rw-r--r--   1 alice    staff        48213 Mar 14 09:15 report.pdf\r";
        assert!(!looks_like_ebcdic(ascii, true));
        assert!(!looks_like_ebcdic("ноя ноя ноя ноя ноя".as_bytes(), true));
        assert!(!looks_like_ebcdic(b"", true));
        assert!(!looks_like_ebcdic(&[0x40u8; 600], false));
    }
}
