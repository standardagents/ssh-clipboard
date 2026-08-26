use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem};
use objc2_foundation::{NSArray, NSData, NSString};
use objc2_uniform_type_identifiers::UTType;

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
        if !image_types.insert(format.clone()) {
            continue;
        }
        set_data(&item, &format, &representation.data)?;
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

fn set_data(item: &NSPasteboardItem, format: &str, bytes: &[u8]) -> Result<()> {
    let data = NSData::with_bytes(bytes);
    if !item.setData_forType(&data, &NSString::from_str(format)) {
        bail!("publish {format} to macOS pasteboard");
    }
    Ok(())
}

pub(super) fn native_pasteboard_format(format: &str) -> Option<String> {
    match format {
        "NSStringPboardType" | "STRING" | "TEXT" | "UTF8_STRING" => {
            return Some("public.utf8-plain-text".to_owned());
        }
        "NSHTMLPboardType" => return Some("public.html".to_owned()),
        "NSRTFPboardType" => return Some("public.rtf".to_owned()),
        "NSTIFFPboardType" => return Some("public.tiff".to_owned()),
        _ => {}
    }

    if !format.contains('/') {
        return is_valid_uti(format).then(|| format.to_owned());
    }
    let mime_type = format
        .split_once(';')
        .map_or(format, |(mime_type, _)| mime_type)
        .trim();
    let (type_name, subtype) = mime_type.split_once('/')?;
    if type_name.is_empty() || subtype.is_empty() {
        return None;
    }
    if mime_type.eq_ignore_ascii_case("text/plain")
        && format.split(';').skip(1).map(str::trim).any(|parameter| {
            parameter.eq_ignore_ascii_case("charset=utf-8") || parameter.eq_ignore_ascii_case("charset=utf8")
        })
    {
        return Some("public.utf8-plain-text".to_owned());
    }
    UTType::typeWithMIMEType(&NSString::from_str(mime_type))
        .map(|native_type| native_type.identifier().to_string())
}

fn is_valid_uti(format: &str) -> bool {
    format.contains('.')
        && format
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

fn native_image_format(format: &str) -> Option<String> {
    let native_type = if format.contains('/') {
        let mime_type = format
            .split_once(';')
            .map_or(format, |(mime_type, _)| mime_type)
            .trim();
        UTType::typeWithMIMEType(&NSString::from_str(mime_type))?
    } else {
        UTType::typeWithIdentifier(&NSString::from_str(format))?
    };
    let image_type = UTType::typeWithIdentifier(&NSString::from_str("public.image"))?;
    native_type
        .conformsToType(&image_type)
        .then(|| native_type.identifier().to_string())
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

    #[test]
    fn resolves_portable_mime_types_to_macos_pasteboard_types() {
        assert_eq!(
            native_pasteboard_format("image/png").as_deref(),
            Some("public.png")
        );
        assert_eq!(
            native_pasteboard_format("text/html; charset=utf-8").as_deref(),
            Some("public.html")
        );
        assert_eq!(
            native_pasteboard_format("text/plain;charset=utf-8").as_deref(),
            Some("public.utf8-plain-text")
        );
        assert_eq!(
            native_pasteboard_format("UTF8_STRING").as_deref(),
            Some("public.utf8-plain-text")
        );
        assert_eq!(
            native_pasteboard_format("public.png").as_deref(),
            Some("public.png")
        );
        assert_eq!(
            native_pasteboard_format("com.example.custom-data").as_deref(),
            Some("com.example.custom-data")
        );
        assert_eq!(native_pasteboard_format("INVALID_ATOM"), None);

        let custom = native_pasteboard_format("chromium/x-web-custom-data").unwrap();
        assert!(!custom.contains('/'));
        assert!(UTType::typeWithIdentifier(&NSString::from_str(&custom)).is_some());
    }

    #[test]
    fn publishes_a_portable_mime_image_as_a_macos_image_type() {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let item = NSPasteboardItem::new();
        let png = b"\x89PNG\r\n\x1a\nfixture";
        let format = native_pasteboard_format("image/png").unwrap();
        set_data(&item, &format, png).unwrap();

        pasteboard.clearContents();
        let objects = NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)]);
        assert!(pasteboard.writeObjects(&objects));
        assert_eq!(
            pasteboard
                .dataForType(&NSString::from_str("public.png"))
                .unwrap()
                .to_vec(),
            png
        );
    }

    #[test]
    fn publishes_utf8_mime_text_as_macos_pasteboard_text() {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let item = NSPasteboardItem::new();
        let text = "portable text";
        let format = native_pasteboard_format("text/plain;charset=utf-8").unwrap();
        set_data(&item, &format, text.as_bytes()).unwrap();

        pasteboard.clearContents();
        let objects = NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)]);
        assert!(pasteboard.writeObjects(&objects));
        assert_eq!(
            pasteboard
                .pasteboardItems()
                .unwrap()
                .iter()
                .next()
                .unwrap()
                .stringForType(&NSString::from_str("public.utf8-plain-text"))
                .unwrap()
                .to_string(),
            text
        );
    }
}
