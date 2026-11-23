//! Content-type bucket detection. Header is the primary signal; magic bytes
//! confirm or override (servers lie about binary blobs as
//! `application/octet-stream`).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Image,
    Pdf,
    Audio,
    Video,
    Text,
}

impl Bucket {
    /// The dashboard's byte cap for this bucket.
    pub fn cap(self, media: &crate::settings::ai::AiMedia) -> bytesize::ByteSize {
        match self {
            Bucket::Image => media.max_image_size,
            Bucket::Pdf => media.max_pdf_size,
            Bucket::Audio => media.max_audio_size,
            Bucket::Video => media.max_video_size,
            Bucket::Text => media.max_text_size,
        }
    }
}

/// Best-effort bucket selection using `Content-Type` header first, then magic
/// bytes from `infer`. Returns `None` if neither maps to a supported bucket.
pub fn detect(content_type_header: &str, leading_bytes: &[u8]) -> Option<Bucket> {
    let media_type = content_type_header
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if let Some(b) = bucket_from_media_type(&media_type) {
        return Some(b);
    }

    let kind = infer::get(leading_bytes)?;
    bucket_from_media_type(kind.mime_type())
}

fn bucket_from_media_type(mime: &str) -> Option<Bucket> {
    match mime {
        "image/png" | "image/jpeg" | "image/jpg" | "image/webp" | "image/gif" => {
            Some(Bucket::Image)
        }
        "application/pdf" => Some(Bucket::Pdf),
        m if m.starts_with("audio/") => Some(Bucket::Audio),
        m if m.starts_with("video/") => Some(Bucket::Video),
        "text/html"
        | "application/xhtml+xml"
        | "application/xml"
        | "text/xml"
        | "text/plain"
        | "application/json" => Some(Bucket::Text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_maps_to_bucket_ignoring_params() {
        for (header, expected) in [
            ("image/jpeg; charset=binary", Bucket::Image),
            ("text/html; charset=utf-8", Bucket::Text),
            ("audio/mpeg", Bucket::Audio),
            ("video/mp4", Bucket::Video),
        ] {
            assert_eq!(detect(header, &[]), Some(expected), "{header}");
        }
    }

    #[test]
    fn header_lies_magic_corrects_to_pdf() {
        let bytes = b"%PDF-1.7\n%...";
        assert_eq!(detect("application/octet-stream", bytes), Some(Bucket::Pdf));
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(detect("application/x-bogus", &[0x00, 0x01]), None);
    }

    #[test]
    fn header_missing_magic_png_returns_image() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(detect("", &png), Some(Bucket::Image));
    }
}
