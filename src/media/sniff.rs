//! Media-type detection from the leading bytes of a file.
//!
//! A declared media type is a claim made by whoever submitted the case, and a
//! case may be submitted by a system Refinery does not control. The media
//! store therefore measures the type rather than trusting it: a PNG header is
//! evidence, `Content-Type: image/png` is not. Detection is deliberately
//! narrow — it recognises the container formats Refinery is willing to hand to
//! a provider and says nothing about anything else, because a permissive
//! sniffer that guesses is worse than one that admits it does not know.

/// What sniffing concluded about a byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniffed {
    /// The bytes carry a recognised container signature.
    Known(&'static str),
    /// The bytes are valid UTF-8 with no control characters that would mark
    /// them as binary. Any `text/*` declaration is consistent with this.
    Text,
    /// The bytes match no signature Refinery recognises.
    Unknown,
}

impl Sniffed {
    /// The measured media type, when one was recognised.
    pub fn media_type(self) -> Option<&'static str> {
        match self {
            Sniffed::Known(media_type) => Some(media_type),
            Sniffed::Text | Sniffed::Unknown => None,
        }
    }
}

/// How many leading bytes detection needs. Every signature below fits well
/// inside this, and the text check reads no further either: a file whose first
/// kilobyte is clean text but whose tail is binary is still accepted as text,
/// which matches how every other sniffer on the machine behaves.
pub const SNIFF_PREFIX_BYTES: usize = 1024;

/// Measure the media type of a byte stream.
pub fn sniff(bytes: &[u8]) -> Sniffed {
    let prefix = &bytes[..bytes.len().min(SNIFF_PREFIX_BYTES)];

    if let Some(media_type) = signature(prefix) {
        return Sniffed::Known(media_type);
    }
    if looks_like_text(prefix) {
        return Sniffed::Text;
    }
    Sniffed::Unknown
}

/// Match the container signatures Refinery recognises.
fn signature(prefix: &[u8]) -> Option<&'static str> {
    if prefix.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if prefix.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if prefix.starts_with(b"GIF87a") || prefix.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if prefix.len() >= 12 && prefix.starts_with(b"RIFF") && &prefix[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if prefix.starts_with(b"BM") {
        return Some("image/bmp");
    }
    if prefix.starts_with(b"%PDF-") {
        return Some("application/pdf");
    }
    if prefix.starts_with(b"\x1a\x45\xdf\xa3") {
        // Matroska and WebM share the EBML header; the DocType names which.
        // The DocType element sits within the first EBML header, well inside
        // this window.
        return Some(if contains(&prefix[..64.min(prefix.len())], b"webm") {
            "video/webm"
        } else {
            "video/x-matroska"
        });
    }
    if prefix.len() >= 12 && &prefix[4..8] == b"ftyp" {
        return Some(isobmff_media_type(&prefix[8..12]));
    }
    None
}

/// ISO base media files all start `ftyp`; the brand that follows decides
/// whether the provider should be told this is MP4 or QuickTime.
fn isobmff_media_type(brand: &[u8]) -> &'static str {
    match brand {
        b"qt  " => "video/quicktime",
        _ => "video/mp4",
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Whether a prefix reads as text rather than binary.
///
/// The rule matches the repository connector's binary detection: a NUL byte,
/// or an unusual density of other control characters, means binary. Invalid
/// UTF-8 is binary too, since Refinery only ever hands text to a provider as a
/// string.
fn looks_like_text(prefix: &[u8]) -> bool {
    let text = match std::str::from_utf8(prefix) {
        Ok(text) => text,
        // A truncated multi-byte character at the prefix boundary is not
        // evidence of binary content, so retry on the valid part.
        Err(error) if error.valid_up_to() > 0 && error.error_len().is_none() => {
            match std::str::from_utf8(&prefix[..error.valid_up_to()]) {
                Ok(text) => text,
                Err(_) => return false,
            }
        }
        Err(_) => return false,
    };
    !text
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_signatures_are_recognised() {
        assert_eq!(
            sniff(b"\x89PNG\r\n\x1a\n\x00").media_type(),
            Some("image/png")
        );
        assert_eq!(
            sniff(&[0xFF, 0xD8, 0xFF, 0xE0]).media_type(),
            Some("image/jpeg")
        );
        assert_eq!(sniff(b"GIF89a....").media_type(), Some("image/gif"));
        assert_eq!(sniff(b"%PDF-1.7\n").media_type(), Some("application/pdf"));
    }

    #[test]
    fn an_iso_base_media_file_is_video_and_its_brand_picks_the_type() {
        let mp4 = [b"\x00\x00\x00\x18".as_slice(), b"ftyp", b"isom"].concat();
        assert_eq!(sniff(&mp4).media_type(), Some("video/mp4"));
        let mov = [b"\x00\x00\x00\x14".as_slice(), b"ftyp", b"qt  "].concat();
        assert_eq!(sniff(&mov).media_type(), Some("video/quicktime"));
    }

    #[test]
    fn a_webm_ebml_header_is_distinguished_from_matroska() {
        let mut webm = vec![0x1a, 0x45, 0xdf, 0xa3];
        webm.extend_from_slice(b"\x01\x00\x00\x00\x00\x00\x00\x1fDocTypewebm\x00\x00\x00\x00\x00");
        assert_eq!(sniff(&webm).media_type(), Some("video/webm"));
        let mut mkv = vec![0x1a, 0x45, 0xdf, 0xa3];
        mkv.extend_from_slice(b"\x01\x00\x00\x00\x00\x00\x00\x1fDocTypematroska\x00\x00");
        assert_eq!(sniff(&mkv).media_type(), Some("video/x-matroska"));
    }

    #[test]
    fn plain_utf8_is_text_and_nul_bytes_are_not() {
        assert_eq!(sniff("hello\nworld\t".as_bytes()), Sniffed::Text);
        assert_eq!(sniff("héllo — ünïcode".as_bytes()), Sniffed::Text);
        assert_eq!(sniff(b"before\x00after"), Sniffed::Unknown);
        assert_eq!(sniff(&[0xC3, 0x28]), Sniffed::Unknown);
    }

    #[test]
    fn a_multi_byte_character_split_by_the_prefix_boundary_is_still_text() {
        let mut bytes = "a".repeat(SNIFF_PREFIX_BYTES - 1).into_bytes();
        bytes.extend_from_slice("é".as_bytes());
        assert_eq!(sniff(&bytes), Sniffed::Text);
    }
}
