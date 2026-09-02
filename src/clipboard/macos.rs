use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem};
use objc2_foundation::{NSArray, NSData, NSString};

use crate::filebundle;
use crate::model::Representation;

use super::{is_file_format, is_internal_marker, is_sensitive_marker};

pub(super) fn publish_single_file(path: &Path, representations: &[Representation]) -> Result<()> {
    let pasteboard = NSPasteboard::generalPasteboard();
    publish_single_file_to(&pasteboard, path, representations)
}

fn publish_single_file_to(
    pasteboard: &NSPasteboard,
    path: &Path,
    representations: &[Representation],
) -> Result<()> {
    if !path.is_file() {
        bail!("clipboard file does not exist: {}", path.display());
    }

    let item = NSPasteboardItem::new();
    let file_url_type = NSString::from_str("public.file-url");
    let file_url = NSString::from_str(&filebundle::path_to_uri(path));
    if !item.setString_forType(&file_url, &file_url_type) {
        bail!("publish file URL to macOS pasteboard");
    }

    let mut image_types = HashSet::new();
    for representation in representations {
        if is_file_format(&representation.format)
            || is_internal_marker(&representation.format)
            || is_sensitive_marker(&representation.format)
        {
            continue;
        }
        let Some(format) = native_image_format(&representation.format) else {
            continue;
        };
        if !image_types.insert(format) {
            continue;
        }
        set_data(&item, format, &representation.data)?;
    }

    if image_types.is_empty() {
        let bytes = std::fs::read(path).with_context(|| format!("read copied image {}", path.display()))?;
        if let Some(format) = detect_image_format(path, &bytes) {
            set_data(&item, format, &bytes)?;
        }
    }

    pasteboard.clearContents();
    let objects = NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)]);
    if !pasteboard.writeObjects(&objects) {
        bail!("publish file and image to macOS pasteboard");
    }
    Ok(())
}

/// Maps a portable MIME name onto the macOS pasteboard type carrying the same
/// bytes. `add_portable_aliases` is the outbound half of this pairing; without
/// the inbound half every representation from a Wayland or X11 peer reaches
/// NSPasteboard as an invalid UTI and the publish clears the pasteboard.
pub(super) fn native_pasteboard_type(format: &str) -> Option<String> {
    if is_pasteboard_type(format) {
        return Some(format.to_owned());
    }
    // MIME names carry parameters such as `text/plain;charset=utf-8`.
    let base = format
        .split(';')
        .next()
        .unwrap_or(format)
        .trim()
        .to_ascii_lowercase();
    let native = match base.as_str() {
        "text/plain" | "text" | "string" | "utf8_string" => "public.utf8-plain-text",
        "text/html" => "public.html",
        "text/rtf" | "application/rtf" => "public.rtf",
        "image/png" => "public.png",
        "image/jpeg" | "image/jpg" => "public.jpeg",
        "image/tiff" => "public.tiff",
        "image/gif" => "com.compuserve.gif",
        "image/heic" => "public.heic",
        "image/heif" => "public.heif",
        "image/webp" => "org.webmproject.webp",
        "application/pdf" => "com.adobe.pdf",
        "text/uri-list" => "public.file-url",
        _ => return None,
    };
    Some(native.to_owned())
}

/// Recognizes the reverse-DNS UTIs a macOS peer sends, which pass through
/// unchanged.
fn is_pasteboard_type(format: &str) -> bool {
    format.contains('.')
        && !format.contains('/')
        && format
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+' | '_'))
}

fn set_data(item: &NSPasteboardItem, format: &str, bytes: &[u8]) -> Result<()> {
    let data = NSData::with_bytes(bytes);
    if !item.setData_forType(&data, &NSString::from_str(format)) {
        bail!("publish {format} to macOS pasteboard");
    }
    Ok(())
}

fn native_image_format(format: &str) -> Option<&'static str> {
    match format {
        "public.png" | "image/png" => Some("public.png"),
        "public.jpeg" | "image/jpeg" | "image/jpg" => Some("public.jpeg"),
        "public.tiff" | "image/tiff" => Some("public.tiff"),
        "com.compuserve.gif" | "image/gif" => Some("com.compuserve.gif"),
        "public.heic" | "image/heic" => Some("public.heic"),
        "public.heif" | "image/heif" => Some("public.heif"),
        "org.webmproject.webp" | "image/webp" => Some("org.webmproject.webp"),
        _ => None,
    }
}

fn detect_image_format(path: &Path, bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("public.png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("public.jpeg");
    }
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Some("public.tiff");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("com.compuserve.gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("org.webmproject.webp");
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return match &bytes[8..12] {
            b"heic" | b"heix" | b"hevc" | b"hevx" => Some("public.heic"),
            b"mif1" | b"msf1" => Some("public.heif"),
            _ => None,
        };
    }

    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("public.png"),
        Some("jpg" | "jpeg") => Some("public.jpeg"),
        Some("tif" | "tiff") => Some("public.tiff"),
        Some("gif") => Some("com.compuserve.gif"),
        Some("heic") => Some("public.heic"),
        Some("heif") => Some("public.heif"),
        Some("webp") => Some("org.webmproject.webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_portable_mime_names_onto_pasteboard_types() {
        assert_eq!(
            native_pasteboard_type("text/plain;charset=utf-8").as_deref(),
            Some("public.utf8-plain-text")
        );
        assert_eq!(
            native_pasteboard_type("UTF8_STRING").as_deref(),
            Some("public.utf8-plain-text")
        );
        assert_eq!(native_pasteboard_type("image/png").as_deref(), Some("public.png"));
        assert_eq!(
            native_pasteboard_type("text/html").as_deref(),
            Some("public.html")
        );
    }

    #[test]
    fn passes_pasteboard_types_through_and_drops_unmappable_names() {
        assert_eq!(
            native_pasteboard_type("public.utf8-plain-text").as_deref(),
            Some("public.utf8-plain-text")
        );
        assert_eq!(
            native_pasteboard_type("org.webmproject.webp").as_deref(),
            Some("org.webmproject.webp")
        );
        assert_eq!(native_pasteboard_type("application/x-custom"), None);
        assert_eq!(native_pasteboard_type("NeXT TIFF v4.0 pasteboard type"), None);
    }

    #[test]
    fn publishes_a_file_url_and_image_on_the_same_pasteboard_item() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("message.png");
        let png = b"\x89PNG\r\n\x1a\nfixture";
        std::fs::write(&path, png).unwrap();
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();

        publish_single_file_to(&pasteboard, &path, &[]).unwrap();

        let items = pasteboard.pasteboardItems().unwrap();
        assert_eq!(items.len(), 1);
        let item = items.iter().next().unwrap();
        assert_eq!(
            item.stringForType(&NSString::from_str("public.file-url"))
                .unwrap()
                .to_string(),
            filebundle::path_to_uri(&path)
        );
        assert_eq!(
            item.dataForType(&NSString::from_str("public.png"))
                .unwrap()
                .to_vec(),
            png
        );
    }

    #[test]
    fn recognizes_common_image_file_signatures() {
        assert_eq!(
            detect_image_format(Path::new("attachment"), b"\xff\xd8\xffbody"),
            Some("public.jpeg")
        );
        assert_eq!(
            detect_image_format(Path::new("attachment"), b"\0\0\0\x18ftypheicbody"),
            Some("public.heic")
        );
        assert_eq!(
            detect_image_format(Path::new("attachment.webp"), b"not enough bytes"),
            Some("org.webmproject.webp")
        );
    }
}
