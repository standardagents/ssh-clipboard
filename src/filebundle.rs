use std::collections::HashSet;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::{ensure_private_dir, paths};
use crate::model::Representation;

pub const BUNDLE_FORMAT: &str = "application/x-ssh-clipboard-file-bundle";
#[cfg(test)]
mod finder_tests;
const MAGIC: &[u8; 5] = b"SCBF1";
const URI_ENCODE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    roots: Vec<String>,
    entries: Vec<Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    path: String,
    kind: EntryKind,
    size: u64,
    #[serde(default)]
    mode: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link_target: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EntryKind {
    File,
    Directory,
    Symlink,
}

pub fn attach_bundle(representations: &mut Vec<Representation>, max_remaining: u64) -> Result<()> {
    let source_paths = representations
        .iter()
        .filter(|representation| is_uri_format(&representation.format))
        .flat_map(|representation| parse_uri_candidates(&representation.data))
        .collect::<Vec<_>>();
    attach_bundle_from_paths(representations, &source_paths, max_remaining)
}

pub(crate) fn attach_bundle_from_paths(
    representations: &mut Vec<Representation>,
    source_paths: &[PathBuf],
    max_remaining: u64,
) -> Result<()> {
    if representations
        .iter()
        .any(|representation| representation.format == BUNDLE_FORMAT)
    {
        return Ok(());
    }
    let mut source_paths = source_paths.to_vec();
    let mut seen = HashSet::new();
    source_paths.retain(|path| seen.insert(path.clone()));
    if source_paths.is_empty() {
        return Ok(());
    }
    let bundle = encode(&source_paths, max_remaining)?;
    representations.push(Representation {
        item: 0,
        format: BUNDLE_FORMAT.into(),
        data: bundle,
    });
    Ok(())
}

pub fn materialize(clip_id: Uuid, representations: &[Representation]) -> Result<Vec<Representation>> {
    let Some(bundle) = representations
        .iter()
        .find(|representation| representation.format == BUNDLE_FORMAT)
    else {
        return Ok(representations.to_vec());
    };
    let directory = paths()?
        .state_dir
        .join("files")
        .join(clip_id.simple().to_string());
    let files = decode(&bundle.data, &directory)?;
    let uri_bytes = files
        .iter()
        .map(|path| path_to_uri(path))
        .collect::<Vec<_>>()
        .join("\r\n")
        .into_bytes();
    let mut rewritten = representations
        .iter()
        .filter(|representation| {
            representation.format != BUNDLE_FORMAT
                && !is_uri_format(&representation.format)
                && representation.format != "application/x-kde-cutselection"
        })
        .cloned()
        .collect::<Vec<_>>();
    rewritten.push(Representation {
        item: 0,
        format: "text/uri-list".into(),
        data: uri_bytes,
    });
    Ok(rewritten)
}

#[must_use]
pub fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    parse_uri_candidates(bytes)
        .into_iter()
        .filter(|path| fs::symlink_metadata(path).is_ok())
        .collect()
}

fn parse_uri_candidates(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(uri_to_path)
        .collect()
}

fn encode(sources: &[PathBuf], max_bytes: u64) -> Result<Vec<u8>> {
    let mut manifest = Manifest {
        roots: Vec::new(),
        entries: Vec::new(),
    };
    let mut bodies = Vec::new();
    let mut used_roots = HashSet::new();
    let mut total = 0_u64;
    for source in sources {
        // Do not canonicalize: Finder copies the selected link itself, and
        // resolving it could also rename the selection or copy unrelated data.
        let base = source
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("clipboard-file");
        let root = unique_name(base, &mut used_roots);
        manifest.roots.push(root.clone());
        collect_entries(
            source,
            Path::new(&root),
            &mut manifest.entries,
            &mut bodies,
            &mut total,
            max_bytes,
        )?;
        if total > max_bytes {
            bail!("copied files total {total} bytes; remaining clipboard limit is {max_bytes}");
        }
    }
    let header = serde_json::to_vec(&manifest)?;
    let header_len = u32::try_from(header.len()).context("file manifest is too large")?;
    let overhead = u64::from(header_len) + 9;
    if total.saturating_add(overhead) > max_bytes {
        bail!("copied file bundle exceeds the remaining {max_bytes} byte clipboard limit");
    }
    let capacity = usize::try_from(total.saturating_add(overhead)).context("file bundle is too large")?;
    let mut output = Vec::with_capacity(capacity);
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&header_len.to_be_bytes());
    output.extend_from_slice(&header);
    for body in bodies {
        output.extend_from_slice(&body);
    }
    Ok(output)
}

fn collect_entries(
    source: &Path,
    relative: &Path,
    entries: &mut Vec<Entry>,
    bodies: &mut Vec<Vec<u8>>,
    total: &mut u64,
    max_bytes: u64,
) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        crate::clipboard::macos::file_access::coordinated_read(source, |source| {
            collect_accessible_entries(source, relative, entries, bodies, total, max_bytes)
        })
    }
    #[cfg(not(target_os = "macos"))]
    collect_accessible_entries(source, relative, entries, bodies, total, max_bytes)
}

fn collect_accessible_entries(
    source: &Path,
    relative: &Path,
    entries: &mut Vec<Entry>,
    bodies: &mut Vec<Vec<u8>>,
    total: &mut u64,
    max_bytes: u64,
) -> Result<()> {
    let metadata =
        fs::symlink_metadata(source).with_context(|| format!("inspect copied item {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(source)?;
        entries.push(Entry {
            path: relative_to_string(relative)?,
            kind: EntryKind::Symlink,
            size: 0,
            mode: 0,
            link_target: Some(
                target
                    .to_str()
                    .context("non-UTF-8 symbolic link target")?
                    .to_owned(),
            ),
        });
        return Ok(());
    }
    if metadata.is_dir() {
        entries.push(Entry {
            path: relative_to_string(relative)?,
            kind: EntryKind::Directory,
            size: 0,
            mode: mode(&metadata),
            link_target: None,
        });
        let mut children = fs::read_dir(source)
            .with_context(|| format!("read copied folder {}", source.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            collect_entries(
                &child.path(),
                &relative.join(child.file_name()),
                entries,
                bodies,
                total,
                max_bytes,
            )?;
        }
    } else if metadata.is_file() {
        let size = metadata.len();
        let next_total = total.checked_add(size).context("copied file size overflow")?;
        if next_total > max_bytes {
            bail!("copied files total {next_total} bytes; remaining clipboard limit is {max_bytes}");
        }
        let data = fs::read(source).with_context(|| format!("read copied file {}", source.display()))?;
        if u64::try_from(data.len()).context("copied file is too large")? != size {
            bail!("copied file changed while it was being read");
        }
        *total = next_total;
        entries.push(Entry {
            path: relative_to_string(relative)?,
            kind: EntryKind::File,
            size,
            mode: mode(&metadata),
            link_target: None,
        });
        bodies.push(data);
    } else {
        bail!(
            "unsupported filesystem item {} (not a file, folder, or symbolic link)",
            source.display()
        );
    }
    Ok(())
}

fn decode(bytes: &[u8], destination: &Path) -> Result<Vec<PathBuf>> {
    let mut cursor = Cursor::new(bytes);
    let mut magic = [0; 5];
    cursor.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("invalid copied-file bundle magic");
    }
    let mut header_len = [0; 4];
    cursor.read_exact(&mut header_len)?;
    let header_len = u32::from_be_bytes(header_len) as usize;
    if header_len == 0 || header_len > 16 * 1024 * 1024 {
        bail!("invalid copied-file manifest length");
    }
    let mut header = vec![0; header_len];
    cursor.read_exact(&mut header)?;
    let manifest: Manifest = serde_json::from_slice(&header)?;
    let roots = manifest
        .roots
        .iter()
        .map(|root| safe_relative(root))
        .collect::<Result<Vec<_>>>()?;
    validate_entries(&manifest)?;
    let mut body_bytes = 0_u64;
    for entry in &manifest.entries {
        safe_relative(&entry.path)?;
        if matches!(entry.kind, EntryKind::File) {
            body_bytes = body_bytes
                .checked_add(entry.size)
                .context("copied-file bundle size overflow")?;
        }
    }
    let expected = cursor
        .position()
        .checked_add(body_bytes)
        .context("copied-file bundle size overflow")?;
    if expected != u64::try_from(bytes.len()).unwrap_or(u64::MAX) {
        bail!("copied-file bundle body size does not match its manifest");
    }
    if destination.exists() {
        let materialized = roots
            .iter()
            .map(|root| destination.join(root))
            .collect::<Vec<_>>();
        if materialized.iter().all(|path| fs::symlink_metadata(path).is_ok()) {
            return Ok(materialized);
        }
        bail!("existing copied-file bundle is incomplete");
    }
    ensure_private_dir(destination)?;
    for entry in &manifest.entries {
        let relative = safe_relative(&entry.path)?;
        let path = destination.join(relative);
        match entry.kind {
            // Links are created last, so no extraction write can follow a
            // peer-supplied link outside the private staging directory.
            EntryKind::Symlink => {}
            EntryKind::Directory => fs::create_dir_all(&path)?,
            EntryKind::File => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let size = usize::try_from(entry.size).context("copied file is too large")?;
                let mut data = vec![0; size];
                cursor.read_exact(&mut data)?;
                let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
                file.write_all(&data)?;
                file.sync_all()?;
            }
        }
    }
    for entry in manifest.entries.iter().rev() {
        let path = destination.join(&entry.path);
        if matches!(entry.kind, EntryKind::Symlink) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            create_symlink(
                entry
                    .link_target
                    .as_deref()
                    .context("missing symbolic link target")?,
                &path,
            )?;
        } else {
            // Restrictive directory permissions must not prevent extracting
            // their children. Never chmod a symbolic link's target.
            set_mode(&path, entry.mode)?;
        }
    }
    if cursor.position() != u64::try_from(bytes.len()).unwrap_or(u64::MAX) {
        bail!("copied-file bundle contains trailing data");
    }
    let materialized = roots
        .iter()
        .map(|root| destination.join(root))
        .collect::<Vec<_>>();
    if !materialized.iter().all(|path| fs::symlink_metadata(path).is_ok()) {
        bail!("copied-file bundle did not materialize every root");
    }
    Ok(materialized)
}

fn validate_entries(manifest: &Manifest) -> Result<()> {
    let mut kinds = std::collections::HashMap::new();
    for entry in &manifest.entries {
        let path = safe_relative(&entry.path)?;
        if kinds.insert(path, entry.kind).is_some() {
            bail!("duplicate path in copied-file manifest");
        }
        if matches!(entry.kind, EntryKind::Symlink)
            && entry
                .link_target
                .as_ref()
                .is_none_or(|target| target.is_empty() || target.contains('\0'))
        {
            bail!("invalid symbolic link target");
        }
    }
    for path in kinds.keys() {
        for ancestor in path.ancestors().skip(1) {
            if kinds
                .get(ancestor)
                .is_some_and(|kind| !matches!(kind, EntryKind::Directory))
            {
                bail!("copied-file path has a non-directory ancestor");
            }
        }
    }
    for root in &manifest.roots {
        if !kinds.contains_key(&safe_relative(root)?) {
            bail!("copied-file root missing from manifest");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &str, path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, path)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_symlink(_target: &str, _path: &Path) -> Result<()> {
    bail!("symbolic link transfer is unsupported on this platform");
}

fn safe_relative(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("unsafe path in copied-file bundle");
    }
    Ok(path.to_path_buf())
}

fn relative_to_string(path: &Path) -> Result<String> {
    safe_relative(&path.to_string_lossy())?;
    Ok(path.to_string_lossy().into_owned())
}

fn unique_name(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_owned()) {
        return base.to_owned();
    }
    let mut index = 2;
    loop {
        let candidate = format!("{base} {index}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        index += 1;
    }
}

fn is_uri_format(format: &str) -> bool {
    matches!(
        format,
        "text/uri-list"
            | "public.file-url"
            | "NSFilenamesPboardType"
            | "x-special/gnome-copied-files"
            | "x-special/nautilus-clipboard"
    )
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri
        .strip_prefix("file://localhost")
        .or_else(|| uri.strip_prefix("file://"))?;
    let decoded = percent_decode_str(encoded).decode_utf8().ok()?;
    Some(PathBuf::from(decoded.as_ref()))
}

#[must_use]
pub fn path_to_uri(path: &Path) -> String {
    format!(
        "file://{}",
        utf8_percent_encode(&path.to_string_lossy(), URI_ENCODE)
    )
}

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode(_metadata: &fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if mode != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_multiple_files_and_directories() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("one.txt"), b"one").unwrap();
        fs::create_dir(source.path().join("folder")).unwrap();
        fs::write(source.path().join("folder/two.bin"), [0, 1, 2, 255]).unwrap();
        let bundle = encode(
            &[source.path().join("one.txt"), source.path().join("folder")],
            1024 * 1024,
        )
        .unwrap();
        let target = tempfile::tempdir().unwrap();
        let destination = target.path().join("received");
        let roots = decode(&bundle, &destination).unwrap();
        assert_eq!(fs::read(&roots[0]).unwrap(), b"one");
        assert_eq!(fs::read(roots[1].join("two.bin")).unwrap(), [0, 1, 2, 255]);
    }

    #[test]
    fn rejects_path_traversal_from_an_untrusted_peer() {
        let manifest = Manifest {
            roots: vec!["escape".into()],
            entries: vec![Entry {
                path: "../escape".into(),
                kind: EntryKind::File,
                size: 0,
                mode: 0,
                link_target: None,
            }],
        };
        let header = serde_json::to_vec(&manifest).unwrap();
        let mut bundle = MAGIC.to_vec();
        bundle.extend_from_slice(&u32::try_from(header.len()).unwrap().to_be_bytes());
        bundle.extend_from_slice(&header);
        let target = tempfile::tempdir().unwrap();
        let destination = target.path().join("received");
        assert!(decode(&bundle, &destination).is_err());
    }

    #[test]
    fn file_uri_encoding_round_trips_spaces_and_unicode() {
        let path = Path::new("/tmp/My 世界 #1.png");
        let uri = path_to_uri(path);
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }

    #[test]
    fn overlapping_file_formats_do_not_duplicate_bundled_files() {
        let source = tempfile::tempdir().unwrap();
        let first = source.path().join("first.txt");
        let second = source.path().join("second.txt");
        fs::write(&first, "first").unwrap();
        fs::write(&second, "second").unwrap();
        let first_uri = path_to_uri(&first);
        let all_uris = format!("{first_uri}\r\n{}", path_to_uri(&second));
        let mut representations = vec![
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

        attach_bundle(&mut representations, 1024).unwrap();
        let bundle = representations
            .iter()
            .find(|representation| representation.format == BUNDLE_FORMAT)
            .unwrap();
        let target = tempfile::tempdir().unwrap();
        let destination = target.path().join("decoded");
        let files = decode(&bundle.data, &destination).unwrap();

        assert_eq!(files.len(), 2);
        assert_eq!(fs::read_to_string(&files[0]).unwrap(), "first");
        assert_eq!(fs::read_to_string(&files[1]).unwrap(), "second");
    }

    #[test]
    fn explicit_native_paths_create_a_bundle_without_a_uri_representation() {
        let source = tempfile::tempdir().unwrap();
        let path = source.path().join("native.txt");
        fs::write(&path, "native").unwrap();
        let mut representations = Vec::new();

        attach_bundle_from_paths(&mut representations, std::slice::from_ref(&path), 1024).unwrap();

        let bundle = representations
            .iter()
            .find(|representation| representation.format == BUNDLE_FORMAT)
            .unwrap();
        let target = tempfile::tempdir().unwrap();
        let files = decode(&bundle.data, &target.path().join("decoded")).unwrap();
        assert_eq!(fs::read_to_string(&files[0]).unwrap(), "native");
    }

    #[test]
    fn an_unreachable_file_url_is_an_error_instead_of_filename_only_success() {
        let mut representations = vec![Representation {
            item: 0,
            format: "public.file-url".into(),
            data: b"file:///definitely/missing/ssh-clipboard-test".to_vec(),
        }];

        assert!(attach_bundle(&mut representations, 1024).is_err());
        assert!(
            !representations
                .iter()
                .any(|representation| representation.format == BUNDLE_FORMAT)
        );
    }

    #[test]
    fn rejects_oversized_files_before_reading_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.bin");
        let file = fs::File::create(&path).unwrap();
        file.set_len(1024 * 1024).unwrap();
        assert!(encode(&[path], 1024).is_err());
    }
}
