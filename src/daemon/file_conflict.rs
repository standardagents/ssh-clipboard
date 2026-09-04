use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::filebundle;
use crate::model::{Clip, Representation};

const CLAIM_LIFETIME: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClaimKind {
    Local,
    Received,
}

pub(super) struct FileClaim {
    clip: Arc<Clip>,
    kind: ClaimKind,
    claimed_at: Instant,
    restore: Option<Arc<Clip>>,
}

impl FileClaim {
    pub(super) fn new(clip: Arc<Clip>, kind: ClaimKind) -> Self {
        Self {
            clip,
            kind,
            claimed_at: Instant::now(),
            restore: None,
        }
    }

    pub(super) fn kind(&self) -> ClaimKind {
        self.kind
    }

    pub(super) fn clip(&self) -> Arc<Clip> {
        Arc::clone(self.restore.as_ref().unwrap_or(&self.clip))
    }

    pub(super) fn with_restore(mut self, restore: Arc<Clip>) -> Self {
        self.restore = Some(restore);
        self
    }

    pub(super) fn expired(&self) -> bool {
        self.claimed_at.elapsed() > CLAIM_LIFETIME
    }

    pub(super) fn matches_lossy_echo(&self, representations: &[Representation]) -> bool {
        if representations.is_empty() || has_file_selection(representations) {
            return false;
        }

        representations.iter().any(|candidate| {
            is_text_format(&candidate.format)
                && self
                    .clip
                    .representations
                    .iter()
                    .any(|claimed| is_text_format(&claimed.format) && claimed.data == candidate.data)
        })
    }
}

pub(super) fn has_file_bundle(representations: &[Representation]) -> bool {
    representations
        .iter()
        .any(|representation| representation.format == filebundle::BUNDLE_FORMAT)
}

fn has_file_selection(representations: &[Representation]) -> bool {
    representations.iter().any(|representation| {
        matches!(
            representation.format.as_str(),
            filebundle::BUNDLE_FORMAT | "public.file-url" | "NSFilenamesPboardType" | "text/uri-list"
        )
    })
}

fn is_text_format(format: &str) -> bool {
    matches!(
        format,
        "public.utf8-plain-text"
            | "public.utf16-external-plain-text"
            | "public.plain-text"
            | "NSStringPboardType"
            | "UTF8_STRING"
            | "text/plain"
            | "text/plain;charset=utf-8"
            | "com.apple.traditional-mac-plain-text"
            | "CorePasteboardFlavorType 0x54455854"
            | "CorePasteboardFlavorType 0x75743136"
    )
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn representation(format: &str, data: &[u8]) -> Representation {
        Representation {
            item: 0,
            format: format.into(),
            data: data.to_vec(),
        }
    }

    fn file_claim() -> FileClaim {
        FileClaim::new(
            Arc::new(Clip::new(
                Uuid::new_v4(),
                vec![
                    representation(filebundle::BUNDLE_FORMAT, b"bundle"),
                    representation("public.utf8-plain-text", b"report.pdf"),
                ],
            )),
            ClaimKind::Local,
        )
    }

    #[test]
    fn matches_a_filename_only_clipboard_bridge_echo() {
        assert!(file_claim().matches_lossy_echo(&[
            representation("NSStringPboardType", b"report.pdf"),
            representation("public.tiff", b"generic icon"),
        ]));
    }

    #[test]
    fn received_claim_keeps_source_filename_when_restored_clip_has_only_urls() {
        let restored = Arc::new(Clip::new(
            Uuid::new_v4(),
            vec![representation("text/uri-list", b"file:///target/report.pdf")],
        ));
        let claim = file_claim().with_restore(Arc::clone(&restored));
        assert!(claim.matches_lossy_echo(&[representation("NSStringPboardType", b"report.pdf")]));
        assert_eq!(claim.clip(), restored);
    }

    #[test]
    fn preserves_real_file_selections_and_unrelated_text() {
        let claim = file_claim();
        assert!(!claim.matches_lossy_echo(&[
            representation("public.file-url", b"file:///tmp/report.pdf"),
            representation("public.utf8-plain-text", b"report.pdf"),
        ]));
        assert!(!claim.matches_lossy_echo(&[representation("public.utf8-plain-text", b"different text",)]));
    }
}
