use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardContentsOptions, NSPasteboardItem};
use objc2_foundation::{NSArray, NSString, NSURL};

use crate::filebundle;

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

pub(super) fn publish_files(paths: &[PathBuf]) -> Result<()> {
    let pasteboard = NSPasteboard::generalPasteboard();
    publish_files_to(&pasteboard, paths)
}

fn publish_files_to(pasteboard: &NSPasteboard, paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        bail!("publish an empty file selection to the macOS pasteboard");
    }

    let file_url_type = NSString::from_str("public.file-url");
    let mut objects = Vec::with_capacity(paths.len());
    for path in paths {
        if !path.exists() {
            bail!("clipboard file does not exist: {}", path.display());
        }

        let item = NSPasteboardItem::new();
        let file_url = NSString::from_str(&filebundle::path_to_uri(path));
        if !item.setString_forType(&file_url, &file_url_type) {
            bail!("publish file URL to macOS pasteboard");
        }

        objects.push(ProtocolObject::from_retained(item));
    }

    pasteboard.prepareForNewContentsWithOptions(NSPasteboardContentsOptions::CurrentHostOnly);
    let objects = NSArray::from_retained_slice(&objects);
    if !pasteboard.writeObjects(&objects) {
        bail!("publish files to macOS pasteboard");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_an_image_file_as_a_file_instead_of_raw_image_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("message.png");
        let png = b"\x89PNG\r\n\x1a\nfixture";
        std::fs::write(&path, png).unwrap();
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();

        publish_files_to(&pasteboard, std::slice::from_ref(&path)).unwrap();

        let items = pasteboard.pasteboardItems().unwrap();
        assert_eq!(items.len(), 1);
        let item = items.iter().next().unwrap();
        assert_eq!(
            item.stringForType(&NSString::from_str("public.file-url"))
                .unwrap()
                .to_string(),
            filebundle::path_to_uri(&path)
        );
        assert!(item.dataForType(&NSString::from_str("public.png")).is_none());
        assert!(item.dataForType(&NSString::from_str("public.tiff")).is_none());
    }

    #[test]
    fn publishes_every_file_as_a_native_pasteboard_item() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.txt");
        let second = directory.path().join("folder");
        std::fs::write(&first, "first").unwrap();
        std::fs::create_dir(&second).unwrap();
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();

        publish_files_to(&pasteboard, &[first.clone(), second.clone()]).unwrap();

        let items = pasteboard.pasteboardItems().unwrap();
        assert_eq!(items.len(), 2);
        let urls = items
            .iter()
            .map(|item| {
                item.stringForType(&NSString::from_str("public.file-url"))
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            urls,
            [filebundle::path_to_uri(&first), filebundle::path_to_uri(&second)]
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
