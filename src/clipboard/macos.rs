use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem};
use objc2_foundation::{NSArray, NSData, NSString, NSURL};

use crate::filebundle;
use crate::model::Representation;

use super::{is_file_format, is_internal_marker, is_sensitive_marker};

pub(super) fn capture_file_paths() -> Result<Vec<PathBuf>> {
    let pasteboard = NSPasteboard::generalPasteboard();
    capture_file_paths_from(&pasteboard)
}

fn capture_file_paths_from(pasteboard: &NSPasteboard) -> Result<Vec<PathBuf>> {
    let Some(items) = pasteboard.pasteboardItems() else {
        return Ok(Vec::new());
    };
    let file_url_type = NSString::from_str("public.file-url");
    let file_url_types = NSArray::from_retained_slice(std::slice::from_ref(&file_url_type));
    let mut paths = Vec::new();
    let mut advertised_file = false;

    for item in items {
        if item.availableTypeFromArray(&file_url_types).is_none() {
            continue;
        }
        advertised_file = true;
        let value = item
            .stringForType(&file_url_type)
            .context("read a macOS clipboard file URL")?;
        let url = NSURL::URLWithString(&value).context("parse a macOS clipboard file URL")?;
        if !url.isFileURL() {
            bail!("macOS clipboard advertised a non-file URL as a file");
        }
        let path_url = url
            .filePathURL()
            .context("resolve a macOS clipboard file reference URL")?;
        let path = path_url.path().context("resolve a macOS clipboard file path")?;
        let path = PathBuf::from(path.to_string());
        std::fs::metadata(&path).with_context(|| format!("access copied file {}", path.display()))?;
        paths.push(path);
    }

    if advertised_file && paths.is_empty() {
        bail!("macOS clipboard advertised files but none could be resolved");
    }
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    Ok(paths)
}

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
    fn resolves_native_file_urls_from_the_pasteboard() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first file.txt");
        let second = directory.path().join("second file.txt");
        std::fs::write(&first, "first").unwrap();
        std::fs::write(&second, "second").unwrap();
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let urls = [
            NSURL::fileURLWithPath(&NSString::from_str(&first.to_string_lossy())),
            NSURL::fileURLWithPath(&NSString::from_str(&second.to_string_lossy())),
        ];
        let objects = NSArray::from_retained_slice(
            &urls
                .into_iter()
                .map(ProtocolObject::from_retained)
                .collect::<Vec<_>>(),
        );
        pasteboard.clearContents();
        assert!(pasteboard.writeObjects(&objects));

        assert_eq!(capture_file_paths_from(&pasteboard).unwrap(), [first, second]);
    }

    #[test]
    fn rejects_an_unresolvable_advertised_file() {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let item = NSPasteboardItem::new();
        assert!(item.setString_forType(
            &NSString::from_str("file:///definitely/missing/ssh-clipboard-test"),
            &NSString::from_str("public.file-url"),
        ));
        let objects = NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)]);
        pasteboard.clearContents();
        assert!(pasteboard.writeObjects(&objects));

        assert!(capture_file_paths_from(&pasteboard).is_err());
    }
}
