use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

#[cfg(any(target_os = "macos", target_os = "linux"))]
use clipboard_rs::{ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext};

use crate::model::Representation;
use crate::{filebundle, filebundle::BUNDLE_FORMAT};

#[cfg(target_os = "macos")]
mod macos;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub representations: Vec<Representation>,
    pub fingerprint: [u8; 32],
}

impl Snapshot {
    #[must_use]
    pub fn new(mut representations: Vec<Representation>) -> Self {
        representations.sort_by(|left, right| {
            left.item
                .cmp(&right.item)
                .then_with(|| left.format.cmp(&right.format))
        });
        let fingerprint = fingerprint(&representations);
        Self {
            representations,
            fingerprint,
        }
    }
}

#[async_trait]
pub trait ClipboardBackend: Send + Sync {
    async fn capture(&self) -> Result<Option<Snapshot>>;
    async fn apply(&self, representations: &[Representation]) -> Result<Snapshot>;
    fn name(&self) -> &'static str;

    fn change_receiver(&self, _interval: Duration) -> Option<tokio::sync::mpsc::UnboundedReceiver<()>> {
        None
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub struct NativeClipboard {
    context: Mutex<clipboard_rs::ClipboardContext>,
    max_bytes: u64,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct ChangeHandler(tokio::sync::mpsc::UnboundedSender<()>);

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl ClipboardHandler for ChangeHandler {
    fn on_clipboard_change(&mut self) {
        let _ = self.0.send(());
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl NativeClipboard {
    pub fn new(max_bytes: u64) -> Result<Self> {
        use clipboard_rs::ClipboardContext;
        let context = ClipboardContext::new()
            .map_err(|error| anyhow::anyhow!("initialize native clipboard: {error}"))?;
        Ok(Self {
            context: Mutex::new(context),
            max_bytes,
        })
    }

    fn capture_sync(&self) -> Result<Option<Snapshot>> {
        use clipboard_rs::Clipboard;
        #[cfg(target_os = "macos")]
        use clipboard_rs::common::RustImage;

        let context = self
            .context
            .lock()
            .map_err(|_| anyhow::anyhow!("clipboard lock poisoned"))?;
        let mut formats = context
            .available_formats()
            .map_err(|error| anyhow::anyhow!("enumerate clipboard formats: {error}"))?;
        formats.sort();
        formats.dedup();
        if formats.iter().any(|format| is_sensitive_marker(format)) {
            return Ok(None);
        }
        let mut total = 0_u64;
        let mut representations = Vec::with_capacity(formats.len());
        for format in formats {
            if format.trim().is_empty() || is_internal_marker(&format) {
                continue;
            }
            let Ok(data) = context.get_buffer(&format) else {
                continue;
            };
            total = total
                .checked_add(u64::try_from(data.len()).unwrap_or(u64::MAX))
                .context("clipboard size overflow")?;
            if total > self.max_bytes {
                bail!(
                    "clipboard is {total} bytes; configured limit is {}",
                    self.max_bytes
                );
            }
            representations.push(Representation {
                item: 0,
                format,
                data,
            });
        }
        if let Ok(files) = context.get_files()
            && !files.is_empty()
        {
            let data = files
                .iter()
                .map(|file| {
                    if file.starts_with("file://") {
                        file.clone()
                    } else {
                        filebundle::path_to_uri(std::path::Path::new(file))
                    }
                })
                .collect::<Vec<_>>()
                .join("\r\n")
                .into_bytes();
            let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
            if !representations
                .iter()
                .any(|representation| representation.format == "text/uri-list")
                && total.saturating_add(size) <= self.max_bytes
            {
                total += size;
                representations.push(Representation {
                    item: 0,
                    format: "text/uri-list".into(),
                    data,
                });
            }
        }
        #[cfg(target_os = "macos")]
        if !representations
            .iter()
            .any(|representation| is_image_format(&representation.format))
            && let Ok(image) = context.get_image()
            && let Ok(png) = image.to_png()
        {
            let data = png.get_bytes().to_vec();
            let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
            if total.saturating_add(size) <= self.max_bytes {
                total += size;
                representations.push(Representation {
                    item: 0,
                    format: "public.png".into(),
                    data,
                });
            }
        }
        add_portable_aliases(&mut representations, self.max_bytes.saturating_sub(total));
        total = representations
            .iter()
            .map(|representation| u64::try_from(representation.data.len()).unwrap_or(u64::MAX))
            .sum();
        if let Err(error) =
            filebundle::attach_bundle(&mut representations, self.max_bytes.saturating_sub(total))
        {
            let has_rendered_image = representations
                .iter()
                .any(|representation| is_image_format(&representation.format));
            if !has_rendered_image || !is_permission_denied(&error) {
                return Err(error);
            }
        }
        if representations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Snapshot::new(representations)))
        }
    }

    fn apply_sync(&self, representations: &[Representation]) -> Result<Snapshot> {
        use clipboard_rs::{Clipboard, ClipboardContent};
        if representations.is_empty() {
            bail!("clipboard payload is empty");
        }
        let total = representations.iter().try_fold(0_u64, |total, representation| {
            total.checked_add(u64::try_from(representation.data.len()).unwrap_or(u64::MAX))
        });
        let total = total.context("clipboard size overflow")?;
        if total > self.max_bytes {
            bail!(
                "clipboard is {total} bytes; configured limit is {}",
                self.max_bytes
            );
        }
        let mut contents = Vec::with_capacity(representations.len());
        let native_files = clipboard_file_paths(representations);
        #[cfg(target_os = "macos")]
        if native_files.len() == 1 && std::path::Path::new(&native_files[0]).is_file() {
            let context = self
                .context
                .lock()
                .map_err(|_| anyhow::anyhow!("clipboard lock poisoned"))?;
            macos::publish_single_file(std::path::Path::new(&native_files[0]), representations)?;
            drop(context);
            return self
                .capture_sync()?
                .context("native clipboard was empty immediately after publishing");
        }
        if cfg!(target_os = "macos") && !native_files.is_empty() {
            // Finder requires the pasteboard selection to consist entirely of
            // NSURL file objects. clipboard-rs writes Files separately, so
            // appending generic formats creates another item and makes the
            // otherwise valid file selection non-pasteable in Finder.
            contents.push(ClipboardContent::Files(native_files));
        } else {
            let mut added_files = false;
            #[cfg(target_os = "macos")]
            let mut native_formats = MacosFormats::default();
            for representation in representations {
                if representation.format.trim().is_empty()
                    || is_internal_marker(&representation.format)
                    || is_sensitive_marker(&representation.format)
                {
                    continue;
                }
                if is_file_format(&representation.format) {
                    if !added_files && !native_files.is_empty() {
                        contents.push(ClipboardContent::Files(native_files.clone()));
                        added_files = true;
                        continue;
                    }
                    #[cfg(target_os = "macos")]
                    continue;
                }
                #[cfg(target_os = "macos")]
                let format = {
                    let Some(format) = native_formats.normalize(&representation.format) else {
                        continue;
                    };
                    format
                };
                #[cfg(not(target_os = "macos"))]
                let format = representation.format.clone();
                contents.push(ClipboardContent::Other(format, representation.data.clone()));
            }
        }
        if contents.is_empty() {
            bail!("clipboard payload has no safe representations");
        }
        let context = self
            .context
            .lock()
            .map_err(|_| anyhow::anyhow!("clipboard lock poisoned"))?;
        context
            .set(contents)
            .map_err(|error| anyhow::anyhow!("publish native clipboard formats: {error}"))?;
        drop(context);
        self.capture_sync()?
            .context("native clipboard was empty immediately after publishing")
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[async_trait]
impl ClipboardBackend for NativeClipboard {
    async fn capture(&self) -> Result<Option<Snapshot>> {
        self.capture_sync()
    }

    async fn apply(&self, representations: &[Representation]) -> Result<Snapshot> {
        self.apply_sync(representations)
    }

    fn name(&self) -> &'static str {
        #[cfg(target_os = "macos")]
        return "NSPasteboard";
        #[cfg(target_os = "linux")]
        return if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            "Wayland"
        } else {
            "X11"
        };
    }

    fn change_receiver(&self, interval: Duration) -> Option<tokio::sync::mpsc::UnboundedReceiver<()>> {
        #[cfg(target_os = "linux")]
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            // clipboard-rs' current Wayland watcher compares text and MIME names,
            // so it cannot notice two different images with the same MIME. Full
            // snapshot polling below is required for correctness on Wayland.
            return None;
        }
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = ClipboardWatcherContext::new_with_interval(interval).ok()?;
        watcher.add_handler(ChangeHandler(sender));
        std::thread::Builder::new()
            .name("ssh-clipboard-watcher".into())
            .spawn(move || watcher.start_watch())
            .ok()?;
        Some(receiver)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub struct NativeClipboard;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl NativeClipboard {
    pub fn new(_max_bytes: u64) -> Result<Self> {
        bail!("ssh-clipboard supports native clipboards on macOS and Linux")
    }
}

fn fingerprint(representations: &[Representation]) -> [u8; 32] {
    let mut digest = Sha256::new();
    for representation in representations {
        digest.update(representation.item.to_be_bytes());
        digest.update(
            u64::try_from(representation.format.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        digest.update(representation.format.as_bytes());
        digest.update(
            u64::try_from(representation.data.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        digest.update(&representation.data);
    }
    digest.finalize().into()
}

fn is_sensitive_marker(format: &str) -> bool {
    let lower = format.to_ascii_lowercase();
    lower.contains("concealedtype")
        || lower.contains("passwordmanagerhint")
        || lower.contains("keepass")
        || lower == "application/x-nspasteboard-concealed-type"
}

fn is_internal_marker(format: &str) -> bool {
    let lower = format.to_ascii_lowercase();
    lower.contains("transienttype") || lower.contains("autogeneratedtype") || format == BUNDLE_FORMAT
}

fn is_file_format(format: &str) -> bool {
    matches!(
        format,
        "text/uri-list" | "public.file-url" | "NSFilenamesPboardType"
    )
}

fn is_image_format(format: &str) -> bool {
    matches!(
        format,
        "public.png"
            | "image/png"
            | "public.jpeg"
            | "image/jpeg"
            | "image/jpg"
            | "public.tiff"
            | "image/tiff"
            | "com.compuserve.gif"
            | "image/gif"
            | "public.heic"
            | "image/heic"
            | "public.heif"
            | "image/heif"
            | "org.webmproject.webp"
            | "image/webp"
    )
}

#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
struct MacosFormats {
    added: HashSet<String>,
}

#[cfg(any(target_os = "macos", test))]
impl MacosFormats {
    fn normalize(&mut self, format: &str) -> Option<String> {
        let format = macos_clipboard_format(format)?.to_owned();
        self.added.insert(format.clone()).then_some(format)
    }
}

#[cfg(any(target_os = "macos", test))]
fn macos_clipboard_format(format: &str) -> Option<&str> {
    if format.eq_ignore_ascii_case("text/plain;charset=utf-8") {
        return Some("public.utf8-plain-text");
    }
    let mapped = match format {
        "UTF8_STRING" | "text/plain" | "public.plain-text" | "NSStringPboardType" => {
            "public.utf8-plain-text"
        }
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
        _ if is_valid_macos_uti(format) => format,
        _ => return None,
    };
    Some(mapped)
}

#[cfg(any(target_os = "macos", test))]
fn is_valid_macos_uti(format: &str) -> bool {
    let mut labels = format.split('.');
    let Some(first) = labels.next() else {
        return false;
    };
    let Some(second) = labels.next() else {
        return false;
    };
    valid_uti_label(first) && valid_uti_label(second) && labels.all(valid_uti_label)
}

#[cfg(any(target_os = "macos", test))]
fn valid_uti_label(label: &str) -> bool {
    label.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && label.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn is_permission_denied(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

fn clipboard_file_paths(representations: &[Representation]) -> Vec<String> {
    let mut seen = HashSet::new();
    representations
        .iter()
        .filter(|representation| is_file_format(&representation.format))
        .flat_map(|representation| filebundle::parse_uri_list(&representation.data))
        .filter(|path| seen.insert(path.clone()))
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn add_portable_aliases(representations: &mut Vec<Representation>, mut remaining: u64) {
    let originals = representations.clone();
    for representation in originals {
        let alias = match representation.format.as_str() {
            "public.utf8-plain-text" | "public.plain-text" | "NSStringPboardType" | "UTF8_STRING" => {
                "text/plain;charset=utf-8"
            }
            "public.html" => "text/html",
            "public.rtf" => "text/rtf",
            "public.png" => "image/png",
            "public.jpeg" => "image/jpeg",
            "public.tiff" => "image/tiff",
            "com.compuserve.gif" => "image/gif",
            "public.heic" => "image/heic",
            "public.heif" => "image/heif",
            "org.webmproject.webp" => "image/webp",
            "com.adobe.pdf" => "application/pdf",
            "public.file-url" => "text/uri-list",
            _ => continue,
        };
        if representations
            .iter()
            .any(|existing| existing.item == representation.item && existing.format == alias)
        {
            continue;
        }
        let size = u64::try_from(representation.data.len()).unwrap_or(u64::MAX);
        if size > remaining {
            continue;
        }
        remaining -= size;
        representations.push(Representation {
            item: representation.item,
            format: alias.to_owned(),
            data: representation.data,
        });
    }
}

#[cfg(test)]
pub mod test_support {
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;

    #[derive(Default)]
    pub struct MockClipboard {
        snapshot: AsyncMutex<Option<Snapshot>>,
    }

    impl MockClipboard {
        pub async fn replace(&self, representations: Vec<Representation>) {
            *self.snapshot.lock().await = Some(Snapshot::new(representations));
        }
    }

    #[async_trait]
    impl ClipboardBackend for MockClipboard {
        async fn capture(&self) -> Result<Option<Snapshot>> {
            Ok(self.snapshot.lock().await.clone())
        }

        async fn apply(&self, representations: &[Representation]) -> Result<Snapshot> {
            let snapshot = Snapshot::new(representations.to_vec());
            *self.snapshot.lock().await = Some(snapshot.clone());
            Ok(snapshot)
        }

        fn name(&self) -> &'static str {
            "mock"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_are_additive_and_preserve_original_bytes() {
        let mut representations = vec![Representation {
            item: 0,
            format: "public.tiff".into(),
            data: vec![1, 2, 3],
        }];
        add_portable_aliases(&mut representations, 3);
        assert_eq!(representations.len(), 2);
        assert_eq!(representations[0].format, "public.tiff");
        assert_eq!(representations[1].format, "image/tiff");
        assert_eq!(representations[0].data, representations[1].data);
    }

    #[test]
    fn finder_file_paths_are_complete_and_deduplicated() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.txt");
        let second = directory.path().join("second.txt");
        std::fs::write(&first, "first").unwrap();
        std::fs::write(&second, "second").unwrap();
        let first_uri = filebundle::path_to_uri(&first);
        let all_uris = format!("{first_uri}\r\n{}", filebundle::path_to_uri(&second));
        let representations = vec![
            Representation {
                item: 0,
                format: "public.file-url".into(),
                data: first_uri.into_bytes(),
            },
            Representation {
                item: 0,
                format: "text/uri-list".into(),
                data: all_uris.into_bytes(),
            },
        ];

        assert_eq!(
            clipboard_file_paths(&representations),
            vec![
                first.to_string_lossy().into_owned(),
                second.to_string_lossy().into_owned()
            ]
        );
    }

    #[test]
    fn password_manager_markers_block_the_entire_clipboard() {
        assert!(is_sensitive_marker("org.nspasteboard.ConcealedType"));
        assert!(is_sensitive_marker("x-kde-passwordManagerHint"));
    }

    #[test]
    fn only_permission_errors_are_safe_to_replace_with_a_rendered_image() {
        let permission = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("read copied file");
        let too_large = anyhow::anyhow!("copied-file bundle exceeds the clipboard limit");

        assert!(is_permission_denied(&permission));
        assert!(!is_permission_denied(&too_large));
    }

    #[test]
    fn fingerprint_includes_format_and_item() {
        let first = Snapshot::new(vec![Representation {
            item: 0,
            format: "text/plain".into(),
            data: b"same".to_vec(),
        }]);
        let second = Snapshot::new(vec![Representation {
            item: 0,
            format: "text/html".into(),
            data: b"same".to_vec(),
        }]);
        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn macos_normalizes_portable_formats_to_native_types() {
        for (portable, native) in [
            ("UTF8_STRING", "public.utf8-plain-text"),
            ("text/plain;charset=utf-8", "public.utf8-plain-text"),
            ("text/plain;charset=UTF-8", "public.utf8-plain-text"),
            ("public.plain-text", "public.utf8-plain-text"),
            ("NSStringPboardType", "public.utf8-plain-text"),
            ("text/html", "public.html"),
            ("text/rtf", "public.rtf"),
            ("image/png", "public.png"),
            ("image/jpeg", "public.jpeg"),
            ("image/tiff", "public.tiff"),
            ("image/gif", "com.compuserve.gif"),
            ("image/heic", "public.heic"),
            ("image/heif", "public.heif"),
            ("image/webp", "org.webmproject.webp"),
            ("application/pdf", "com.adobe.pdf"),
        ] {
            assert_eq!(macos_clipboard_format(portable), Some(native));
        }
        assert_eq!(
            macos_clipboard_format("public.utf8-plain-text"),
            Some("public.utf8-plain-text")
        );
        assert_eq!(
            macos_clipboard_format("com.example.custom"),
            Some("com.example.custom")
        );
        for unsupported in [
            "STRING",
            "TEXT",
            "COMPOUND_TEXT",
            "application/x-custom",
            "x-special/gnome-copied-files",
            "foo",
            "com..example",
            "com.example_thing",
        ] {
            assert_eq!(macos_clipboard_format(unsupported), None);
        }
    }

    #[test]
    fn macos_collapses_equivalent_formats_before_native_apply() {
        let mut formats = MacosFormats::default();

        assert_eq!(
            formats.normalize("UTF8_STRING"),
            Some("public.utf8-plain-text".to_owned())
        );
        assert_eq!(formats.normalize("text/plain"), None);
        assert_eq!(formats.normalize("text/plain;charset=UTF-8"), None);
        assert_eq!(formats.normalize("public.utf8-plain-text"), None);
        assert_eq!(formats.normalize("STRING"), None);
        assert_eq!(
            formats.normalize("com.example.custom"),
            Some("com.example.custom".to_owned())
        );
    }
}
