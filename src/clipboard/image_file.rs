//! Add a file-manager view of image pixels without changing their image formats.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::model::Representation;

pub(super) const MARKER: &str = "application/x-ssh-clipboard-image-file";

pub(super) fn materialize(representations: &[Representation], directory: &Path) -> Result<Option<PathBuf>> {
    // Prefer PNG when the clipboard offers both PNG and a much larger TIFF.
    // No decoding/re-encoding: the pasted file keeps the original image bytes.
    let selected = [
        ("image/png", "png"),
        ("public.png", "png"),
        ("image/jpeg", "jpg"),
        ("public.jpeg", "jpg"),
        ("image/tiff", "tiff"),
        ("public.tiff", "tiff"),
        ("image/gif", "gif"),
        ("com.compuserve.gif", "gif"),
        ("image/webp", "webp"),
        ("org.webmproject.webp", "webp"),
    ]
    .into_iter()
    .find_map(|(format, extension)| {
        representations
            .iter()
            .find(|r| r.format == format && !r.data.is_empty())
            .map(|r| (r, extension))
    });
    let Some((representation, extension)) = selected else {
        return Ok(None);
    };
    crate::config::ensure_private_dir(directory)?;
    let hash = format!("{:x}", Sha256::digest(&representation.data));
    let destination = directory.join(format!("Clipboard-{hash}.{extension}"));
    if let Ok(metadata) = fs::symlink_metadata(&destination) {
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "image cache is not a regular file"
        );
        anyhow::ensure!(
            fs::read(&destination)? == representation.data,
            "image cache contents changed"
        );
        return Ok(Some(destination));
    }
    let temporary = directory.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&representation.data)?;
        file.sync_all()?;
        fs::rename(&temporary, &destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(Some(destination))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_original_bytes_prefers_png_and_reuses_the_same_file() {
        let directory = tempfile::tempdir().unwrap();
        let images = vec![
            Representation {
                item: 0,
                format: "public.tiff".into(),
                data: vec![1; 100],
            },
            Representation {
                item: 0,
                format: "image/png".into(),
                data: vec![2; 10],
            },
        ];
        let path = materialize(&images, directory.path()).unwrap().unwrap();
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(fs::read(&path).unwrap(), images[1].data);
        assert_eq!(materialize(&images, directory.path()).unwrap().unwrap(), path);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        fs::write(&path, b"modified").unwrap();
        assert!(materialize(&images, directory.path()).is_err());
    }

    #[test]
    fn does_not_create_files_from_text_or_empty_images() {
        assert!(super::super::is_internal_marker(MARKER));
        let directory = tempfile::tempdir().unwrap();
        for (format, data) in [("text/plain", b"text".to_vec()), ("image/png", vec![])] {
            assert!(
                materialize(
                    &[Representation {
                        item: 0,
                        format: format.into(),
                        data
                    }],
                    directory.path()
                )
                .unwrap()
                .is_none()
            );
        }
    }
}
