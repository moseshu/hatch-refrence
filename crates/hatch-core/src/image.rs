//! User-supplied image validation: media-type sniffing by magic bytes
//! (never by file extension or a client-declared type alone), size and
//! count caps. Shared by the chat API and the CLI so both entry points
//! enforce the same rules before an image reaches the model.

use base64::Engine;

/// Most images one user turn may carry.
pub const MAX_IMAGES: usize = 4;
/// Largest decoded image, in bytes.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The media type an image's magic bytes declare, or `None` when the
/// bytes are not a PNG, JPEG, GIF, or WebP image.
pub fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Validate one base64 image against its declared media type: it must
/// decode, fit the size cap, and its magic bytes must match the
/// declared type.
pub fn validate_image(data_b64: &str, declared: &str) -> Result<(), String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|_| "image data is not valid base64".to_string())?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image is {} bytes, over the {MAX_IMAGE_BYTES}-byte limit",
            bytes.len()
        ));
    }
    match sniff_media_type(&bytes) {
        Some(actual) if actual == declared => Ok(()),
        Some(actual) => Err(format!(
            "image bytes are {actual}, not the declared {declared}"
        )),
        None => Err("image bytes are not a PNG, JPEG, GIF, or WebP image".to_string()),
    }
}

/// Encode raw image bytes for a turn: sniff the type, enforce the size
/// cap, base64-encode. Returns `(base64, media_type)`.
pub fn encode_image(bytes: &[u8]) -> Result<(String, &'static str), String> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image is {} bytes, over the {MAX_IMAGE_BYTES}-byte limit",
            bytes.len()
        ));
    }
    let media_type = sniff_media_type(bytes)
        .ok_or_else(|| "image bytes are not a PNG, JPEG, GIF, or WebP image".to_string())?;
    Ok((
        base64::engine::general_purpose::STANDARD.encode(bytes),
        media_type,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0rest";
    const GIF: &[u8] = b"GIF89a rest";
    const WEBP: &[u8] = b"RIFF\x04\x00\x00\x00WEBPrest";

    #[test]
    fn sniff_recognizes_the_four_formats() {
        assert_eq!(sniff_media_type(PNG), Some("image/png"));
        assert_eq!(sniff_media_type(JPEG), Some("image/jpeg"));
        assert_eq!(sniff_media_type(GIF), Some("image/gif"));
        assert_eq!(sniff_media_type(WEBP), Some("image/webp"));
        assert_eq!(sniff_media_type(b"plain text"), None);
        assert_eq!(sniff_media_type(b""), None);
    }

    #[test]
    fn validate_accepts_matching_and_rejects_the_rest() {
        let (b64, mt) = encode_image(PNG).unwrap();
        assert_eq!(mt, "image/png");
        validate_image(&b64, "image/png").unwrap();
        assert!(validate_image(&b64, "image/jpeg").is_err());
        assert!(validate_image("not-base64!", "image/png").is_err());
        let text = base64::engine::general_purpose::STANDARD.encode(b"hello");
        assert!(validate_image(&text, "image/png").is_err());
    }

    #[test]
    fn oversize_is_rejected_before_encoding() {
        let big = vec![0u8; MAX_IMAGE_BYTES + 1];
        assert!(encode_image(&big).is_err());
    }
}
