//! MIME-type → media-category classification.
//!
//! Drives the per-side image / video / audio / gif counts on the dashboard.
//! GIFs are separated from still images deliberately — mimoto-style dashboards
//! count them in their own bucket because their effort/intent profile is
//! different from a photo (a GIF is "lol" energy; a photo is "look at this").
//!
//! Links are *not* a media category — they're detected from the message body
//! by [`crate::patterns::contains_link`], not from MIME types.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaCategory {
    Image,
    Video,
    Audio,
    Gif,
    /// Anything that doesn't fall into the above (vCards, calendar invites,
    /// generic application/* MIME types, etc.). Counted in totals but not
    /// surfaced as its own panel on the dashboard.
    Other,
}

/// Classify an attachment's MIME type. Lower-cased internally so callers don't
/// need to normalize.
///
/// `application/ogg` is treated as audio because Android sometimes sends
/// audio recordings under that MIME.
pub fn classify_media(mime: &str) -> MediaCategory {
    let mime_lower = mime.trim().to_ascii_lowercase();
    match mime_lower.as_str() {
        "image/gif" => MediaCategory::Gif,
        "application/ogg" => MediaCategory::Audio,
        m if m.starts_with("image/") => MediaCategory::Image,
        m if m.starts_with("video/") => MediaCategory::Video,
        m if m.starts_with("audio/") => MediaCategory::Audio,
        _ => MediaCategory::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_image_types() {
        assert_eq!(classify_media("image/jpeg"), MediaCategory::Image);
        assert_eq!(classify_media("image/png"), MediaCategory::Image);
        assert_eq!(classify_media("image/webp"), MediaCategory::Image);
        assert_eq!(classify_media("image/heic"), MediaCategory::Image);
        assert_eq!(classify_media("image/heif"), MediaCategory::Image);
    }

    #[test]
    fn gif_is_not_image() {
        assert_eq!(classify_media("image/gif"), MediaCategory::Gif);
    }

    #[test]
    fn classifies_videos() {
        assert_eq!(classify_media("video/mp4"), MediaCategory::Video);
        assert_eq!(classify_media("video/3gpp"), MediaCategory::Video);
        assert_eq!(classify_media("video/quicktime"), MediaCategory::Video);
        assert_eq!(classify_media("video/webm"), MediaCategory::Video);
    }

    #[test]
    fn classifies_audio_including_ogg() {
        assert_eq!(classify_media("audio/mp3"), MediaCategory::Audio);
        assert_eq!(classify_media("audio/mpeg"), MediaCategory::Audio);
        assert_eq!(classify_media("audio/aac"), MediaCategory::Audio);
        assert_eq!(classify_media("application/ogg"), MediaCategory::Audio);
    }

    #[test]
    fn unknown_mimes_are_other() {
        assert_eq!(classify_media("text/x-vCard"), MediaCategory::Other);
        assert_eq!(classify_media("application/pdf"), MediaCategory::Other);
        assert_eq!(classify_media(""), MediaCategory::Other);
    }

    #[test]
    fn case_and_whitespace_insensitive() {
        assert_eq!(classify_media("IMAGE/JPEG"), MediaCategory::Image);
        assert_eq!(classify_media("  image/gif  "), MediaCategory::Gif);
    }
}
