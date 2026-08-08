#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ContentClass {
    StructuredText,
    Text,
    Database,
    Archive,
    Image,
    Audio,
    Video,
    Binary,
}

pub(crate) fn sort_key(bytes: &[u8], entry_id: u64) -> (ContentClass, [u8; 8], u32, u64) {
    (
        classify(bytes),
        affinity_prefix(bytes),
        size_bucket(bytes.len()),
        entry_id,
    )
}

fn classify(bytes: &[u8]) -> ContentClass {
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(&[0x1f, 0x8b])
        || bytes.starts_with(b"7z\xbc\xaf\x27\x1c")
        || bytes.starts_with(b"Rar!")
    {
        return ContentClass::Archive;
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"BM")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
    {
        return ContentClass::Image;
    }
    if bytes.starts_with(b"fLaC")
        || bytes.starts_with(b"ID3")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE"))
    {
        return ContentClass::Audio;
    }
    if bytes.get(4..8) == Some(b"ftyp") || bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return ContentClass::Video;
    }
    if bytes.starts_with(b"SQLite format 3\0") {
        return ContentClass::Database;
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        let first = text
            .as_bytes()
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace());
        if matches!(first, Some(b'{') | Some(b'[') | Some(b'<')) {
            return ContentClass::StructuredText;
        }
        let sample = &text.as_bytes()[..text.len().min(256 * 1024)];
        let printable = sample
            .iter()
            .filter(|byte| byte.is_ascii_graphic() || byte.is_ascii_whitespace())
            .count();
        if sample.is_empty()
            || printable.saturating_mul(100) >= sample.len().saturating_mul(90)
        {
            return ContentClass::Text;
        }
    }
    ContentClass::Binary
}

fn affinity_prefix(bytes: &[u8]) -> [u8; 8] {
    let mut key = [0_u8; 8];
    let mut used = 0_usize;
    for byte in bytes.iter().copied().take(64 * 1024) {
        if byte.is_ascii_whitespace() {
            continue;
        }
        key[used] = byte.to_ascii_lowercase();
        used += 1;
        if used == key.len() {
            break;
        }
    }
    key
}

fn size_bucket(len: usize) -> u32 {
    if len == 0 {
        0
    } else {
        usize::BITS - len.leading_zeros()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_is_deterministic_and_format_aware() {
        assert_eq!(classify(b"{\"a\":1}"), ContentClass::StructuredText);
        assert_eq!(classify(b"hello world\n"), ContentClass::Text);
        assert_eq!(classify(b"PK\x03\x04rest"), ContentClass::Archive);
        assert_eq!(classify(b"\x89PNG\r\n\x1a\nrest"), ContentClass::Image);
    }

    #[test]
    fn entry_id_breaks_equal_affinity_ties() {
        assert!(sort_key(b"same", 1) < sort_key(b"same", 2));
    }
}
