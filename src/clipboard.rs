use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
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
            let files_published = !native_files.is_empty();
            if files_published {
                contents.push(ClipboardContent::Files(native_files));
            }
            for (native_format, data) in
                generic_publish_entries(representations, publish_target(), files_published)
            {
                contents.push(ClipboardContent::Other(native_format.into_owned(), data.to_vec()));
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

/// Which native backend a representation's type string is being published to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublishTarget {
    /// macOS `NSPasteboard`: the type must be a valid Uniform Type Identifier.
    AppKit,
    /// `wl-clipboard` and X11: arbitrary MIME-type and selection-atom strings
    /// are accepted verbatim.
    Passthrough,
}

/// The publish target for the backend compiled into this build.
const fn publish_target() -> PublishTarget {
    if cfg!(target_os = "macos") {
        PublishTarget::AppKit
    } else {
        PublishTarget::Passthrough
    }
}

/// Translates a wire clipboard format name into the type string the local
/// native backend accepts when publishing, or `None` when the backend cannot
/// represent it and the representation must be dropped.
///
/// X11 selection atoms (`STRING`, `TEXT`, `UTF8_STRING`) and raw MIME types
/// (`text/plain`, `text/html`) are not valid UTIs; modern `AppKit` silently
/// discards them and leaves the pasteboard empty
/// (standardagents/ssh-clipboard#8). `wl-copy` and X11 accept any string, so
/// the passthrough target returns every name unchanged.
fn publish_format(format: &str, target: PublishTarget) -> Option<Cow<'_, str>> {
    match target {
        PublishTarget::Passthrough => Some(Cow::Borrowed(format)),
        PublishTarget::AppKit => {
            if let Some(uti) = appkit_uti(format) {
                Some(Cow::Borrowed(uti))
            } else if is_uti_like(format) {
                Some(Cow::Borrowed(format))
            } else {
                None
            }
        }
    }
}

/// Preference among wire format names that collapse to the same native type.
/// Higher wins. UTF-8-explicit text sources outrank the legacy X11 `STRING`
/// and `TEXT` atoms, whose bytes are Latin-1 under ICCCM.
fn source_rank(format: &str) -> u8 {
    match format.trim().to_ascii_lowercase().as_str() {
        "public.utf8-plain-text" => 5,
        "utf8_string" | "text/plain;charset=utf-8" | "text/html;charset=utf-8" => 4,
        "string" => 1,
        "text" => 2,
        _ => 3,
    }
}

/// Resolves the ordered, de-duplicated `(native type, bytes)` pairs to publish
/// as generic pasteboard data for a clip's non-file representations.
///
/// Empty, internal-marker, and sensitive-marker formats are skipped. File-URL
/// formats are skipped only when `files_published` is set (the caller already
/// pushed a `ClipboardContent::Files` entry); otherwise an unresolvable
/// `text/uri-list` — e.g. an `https://` list from a browser — is passed through
/// so non-macOS backends do not regress. Remaining names are normalized through
/// [`publish_format`]; when several wire names collapse to the same native type,
/// the bytes from the highest-ranked source name are kept (see [`source_rank`]);
/// distinct native types keep their first-seen order.
fn generic_publish_entries(
    representations: &[Representation],
    target: PublishTarget,
    files_published: bool,
) -> Vec<(Cow<'_, str>, &[u8])> {
    let mut index_by_type: HashMap<String, usize> = HashMap::new();
    let mut entries: Vec<(Cow<'_, str>, &[u8], u8)> = Vec::with_capacity(representations.len());
    for representation in representations {
        let format = representation.format.as_str();
        if format.trim().is_empty()
            || is_internal_marker(format)
            || is_sensitive_marker(format)
            || (is_file_format(format) && files_published)
        {
            continue;
        }
        let Some(native_format) = publish_format(format, target) else {
            continue;
        };
        let rank = source_rank(format);
        match index_by_type.get(native_format.as_ref()) {
            Some(&i) if rank > entries[i].2 => {
                entries[i] = (native_format, representation.data.as_slice(), rank);
            }
            Some(_) => {}
            None => {
                index_by_type.insert(native_format.as_ref().to_owned(), entries.len());
                entries.push((native_format, representation.data.as_slice(), rank));
            }
        }
    }
    entries
        .into_iter()
        .map(|(format, data, _)| (format, data))
        .collect()
}

/// Maps a known X11 selection-atom name or MIME type to the macOS UTI that
/// carries the same bytes. Matching is case-insensitive and ignores
/// surrounding whitespace.
fn appkit_uti(format: &str) -> Option<&'static str> {
    match format.trim().to_ascii_lowercase().as_str() {
        "string"
        | "text"
        | "utf8_string"
        | "text/plain"
        | "text/plain;charset=utf-8"
        | "public.utf8-plain-text"
        | "nsstringpboardtype" => Some("public.utf8-plain-text"),
        "html" | "text/html" | "text/html;charset=utf-8" | "public.html" => Some("public.html"),
        "text/rtf" | "application/rtf" | "public.rtf" => Some("public.rtf"),
        "image/png" | "png" | "public.png" => Some("public.png"),
        "image/jpeg" | "image/jpg" | "jpeg" | "public.jpeg" => Some("public.jpeg"),
        "image/tiff" | "tiff" | "public.tiff" => Some("public.tiff"),
        "image/gif" | "gif" | "com.compuserve.gif" => Some("com.compuserve.gif"),
        "application/pdf" | "pdf" | "com.adobe.pdf" => Some("com.adobe.pdf"),
        _ => None,
    }
}

/// Whether `format` already looks like a reverse-DNS Uniform Type Identifier
/// (for example `public.rtfd` or `com.apple.webarchive`) and is safe to hand
/// to `NSPasteboard` unchanged.
fn is_uti_like(format: &str) -> bool {
    !format.is_empty()
        && !format.contains('/')
        && !format.contains(';')
        && !format.chars().any(char::is_whitespace)
        && format
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && format.contains('.')
        && format.split('.').all(|segment| !segment.is_empty())
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
    fn appkit_maps_x11_and_mime_text_names_to_one_uti() {
        for name in [
            "STRING",
            "TEXT",
            "UTF8_STRING",
            "text/plain",
            "text/plain;charset=utf-8",
        ] {
            assert_eq!(
                publish_format(name, PublishTarget::AppKit).as_deref(),
                Some("public.utf8-plain-text"),
                "{name} should map to public.utf8-plain-text"
            );
        }
    }

    #[test]
    fn appkit_maps_known_rich_and_image_mime_types() {
        for (name, uti) in [
            ("text/html", "public.html"),
            ("HTML", "public.html"),
            ("text/rtf", "public.rtf"),
            ("application/rtf", "public.rtf"),
            ("image/png", "public.png"),
            ("PNG", "public.png"),
            ("image/jpeg", "public.jpeg"),
            ("image/tiff", "public.tiff"),
            ("TIFF", "public.tiff"),
            ("image/gif", "com.compuserve.gif"),
            ("application/pdf", "com.adobe.pdf"),
        ] {
            assert_eq!(
                publish_format(name, PublishTarget::AppKit).as_deref(),
                Some(uti),
                "{name}"
            );
        }
    }

    #[test]
    fn appkit_passes_through_reverse_dns_utis_and_drops_the_rest() {
        assert_eq!(
            publish_format("com.apple.webarchive", PublishTarget::AppKit).as_deref(),
            Some("com.apple.webarchive")
        );
        assert_eq!(
            publish_format("public.utf8-plain-text", PublishTarget::AppKit).as_deref(),
            Some("public.utf8-plain-text")
        );
        assert_eq!(
            publish_format("public.plain-text", PublishTarget::AppKit).as_deref(),
            Some("public.plain-text")
        );
        assert_eq!(
            publish_format("public.text", PublishTarget::AppKit).as_deref(),
            Some("public.text")
        );
        for dropped in ["TARGETS", "MULTIPLE", "x-special/nautilus-clipboard", ""] {
            assert_eq!(
                publish_format(dropped, PublishTarget::AppKit),
                None,
                "{dropped:?} should be dropped"
            );
        }
    }

    #[test]
    fn is_uti_like_accepts_reverse_dns_and_rejects_malformed_names() {
        assert!(is_uti_like("dyn.ah62d4rv4ge80s5dbq"));
        assert!(is_uti_like("com.apple.flat-rtfd"));
        assert!(!is_uti_like("foo."));
        assert!(!is_uti_like(".foo"));
        assert!(!is_uti_like("my.type!"));
    }

    #[test]
    fn passthrough_target_preserves_every_name_verbatim() {
        for name in [
            "STRING",
            "text/plain",
            "x-special/gnome-copied-files",
            "public.png",
            "TARGETS",
        ] {
            assert_eq!(
                publish_format(name, PublishTarget::Passthrough).as_deref(),
                Some(name)
            );
        }
    }

    #[test]
    fn generic_entries_collapse_duplicate_text_flavors_for_appkit() {
        let representations = vec![
            Representation {
                item: 0,
                format: "STRING".into(),
                data: b"latin1-bytes".to_vec(),
            },
            Representation {
                item: 0,
                format: "UTF8_STRING".into(),
                data: b"utf8-bytes".to_vec(),
            },
            Representation {
                item: 0,
                format: "text/plain".into(),
                data: b"utf8-bytes".to_vec(),
            },
        ];
        let entries = generic_publish_entries(&representations, PublishTarget::AppKit, false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0.as_ref(), "public.utf8-plain-text");
        assert_eq!(
            entries[0].1, b"utf8-bytes",
            "the UTF-8 source's bytes must win over STRING"
        );
    }

    #[test]
    fn source_rank_prefers_utf8_text_over_legacy_atoms() {
        assert!(source_rank("UTF8_STRING") > source_rank("STRING"));
        assert!(source_rank("text/plain;charset=utf-8") > source_rank("TEXT"));
        assert!(source_rank("public.utf8-plain-text") > source_rank("UTF8_STRING"));
        assert!(source_rank("text/plain") > source_rank("STRING"));
        assert!(source_rank("TEXT") > source_rank("STRING"));
    }

    #[test]
    fn passthrough_keeps_uri_list_when_no_files_were_published() {
        let representations = vec![Representation {
            item: 0,
            format: "text/uri-list".into(),
            data: b"https://example.com/\r\n".to_vec(),
        }];
        let with = generic_publish_entries(&representations, PublishTarget::Passthrough, true);
        assert!(
            with.is_empty(),
            "when Files was published, the raw uri-list is suppressed"
        );
        let without = generic_publish_entries(&representations, PublishTarget::Passthrough, false);
        assert_eq!(without.len(), 1);
        assert_eq!(without[0].0.as_ref(), "text/uri-list");
        assert_eq!(without[0].1, b"https://example.com/\r\n");
    }

    #[test]
    fn generic_entries_skip_markers_files_and_unmappable_names_for_appkit() {
        let representations = vec![
            Representation {
                item: 0,
                format: "text/uri-list".into(),
                data: b"file:///tmp/x".to_vec(),
            },
            Representation {
                item: 0,
                format: "org.nspasteboard.TransientType".into(),
                data: b"1".to_vec(),
            },
            Representation {
                item: 0,
                format: "org.nspasteboard.ConcealedType".into(),
                data: b"secret".to_vec(),
            },
            Representation {
                item: 0,
                format: "TARGETS".into(),
                data: b"x".to_vec(),
            },
            Representation {
                item: 0,
                format: "  ".into(),
                data: b"x".to_vec(),
            },
        ];
        assert!(generic_publish_entries(&representations, PublishTarget::AppKit, true).is_empty());
    }

    #[test]
    fn generic_entries_keep_distinct_appkit_types_in_order() {
        let representations = vec![
            Representation {
                item: 0,
                format: "text/html".into(),
                data: b"<p>hi</p>".to_vec(),
            },
            Representation {
                item: 0,
                format: "STRING".into(),
                data: b"hi".to_vec(),
            },
        ];
        let entries = generic_publish_entries(&representations, PublishTarget::AppKit, false);
        assert_eq!(
            entries.iter().map(|(f, _)| f.as_ref()).collect::<Vec<_>>(),
            vec!["public.html", "public.utf8-plain-text"]
        );
    }

    #[test]
    fn generic_entries_preserve_all_names_on_passthrough() {
        let representations = vec![
            Representation {
                item: 0,
                format: "STRING".into(),
                data: b"a".to_vec(),
            },
            Representation {
                item: 0,
                format: "text/plain".into(),
                data: b"a".to_vec(),
            },
            Representation {
                item: 0,
                format: "x-special/gnome-copied-files".into(),
                data: b"a".to_vec(),
            },
        ];
        let entries = generic_publish_entries(&representations, PublishTarget::Passthrough, false);
        assert_eq!(
            entries.iter().map(|(f, _)| f.as_ref()).collect::<Vec<_>>(),
            vec!["STRING", "text/plain", "x-special/gnome-copied-files"]
        );
    }
}
