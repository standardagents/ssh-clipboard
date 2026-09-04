use super::*;

#[test]
fn all_regular_file_extensions_and_extensionless_files_round_trip() {
    let source = tempfile::tempdir().unwrap();
    let payload = (0..=255).collect::<Vec<u8>>();
    let names = [
        "manual.pdf",
        "disk.dmg",
        "installer.pkg",
        "sheet.numbers",
        "archive.zip",
        "movie.mp4",
        "unknown.custom",
        "no extension",
    ];
    let sources = names
        .iter()
        .map(|name| {
            let path = source.path().join(name);
            fs::write(&path, &payload).unwrap();
            path
        })
        .collect::<Vec<_>>();
    let bytes = encode(&sources, 1_000_000).unwrap();
    let target = tempfile::tempdir().unwrap();
    let received = decode(&bytes, &target.path().join("received")).unwrap();
    for (path, name) in received.iter().zip(names) {
        assert_eq!(path.file_name().unwrap(), name);
        assert_eq!(fs::read(path).unwrap(), payload);
    }
}

#[cfg(unix)]
#[test]
fn app_bundle_preserves_internal_links_executables_and_empty_directories() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let source = tempfile::tempdir().unwrap();
    let app = source.path().join("Example.app");
    fs::create_dir_all(app.join("Contents/Frameworks/Demo.framework/Versions/A")).unwrap();
    fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    fs::create_dir_all(app.join("Contents/Resources/empty")).unwrap();
    let binary = app.join("Contents/MacOS/example");
    fs::write(&binary, b"fixture executable, not run").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(
        "A",
        app.join("Contents/Frameworks/Demo.framework/Versions/Current"),
    )
    .unwrap();
    symlink("missing", app.join("broken link")).unwrap();
    let bytes = encode(&[app], 1_000_000).unwrap();
    let target = tempfile::tempdir().unwrap();
    let received = decode(&bytes, &target.path().join("received")).unwrap().remove(0);
    assert_eq!(
        fs::read_link(received.join("Contents/Frameworks/Demo.framework/Versions/Current")).unwrap(),
        Path::new("A")
    );
    assert_eq!(
        fs::read_link(received.join("broken link")).unwrap(),
        Path::new("missing")
    );
    assert_eq!(
        fs::metadata(received.join("Contents/MacOS/example"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert!(received.join("Contents/Resources/empty").is_dir());
}

#[cfg(unix)]
#[test]
fn selected_symlink_is_not_dereferenced_or_renamed() {
    let source = tempfile::tempdir().unwrap();
    let link = source.path().join("chosen link");
    std::os::unix::fs::symlink("unavailable-target", &link).unwrap();
    let bytes = encode(&[link], 1024).unwrap();
    let target = tempfile::tempdir().unwrap();
    let received = decode(&bytes, &target.path().join("received")).unwrap();
    assert_eq!(received[0].file_name().unwrap(), "chosen link");
    assert_eq!(
        fs::read_link(&received[0]).unwrap(),
        Path::new("unavailable-target")
    );
    assert_eq!(decode(&bytes, &target.path().join("received")).unwrap(), received);
}

#[test]
fn rejects_writing_through_a_peer_supplied_symlink_before_extraction() {
    let target = tempfile::tempdir().unwrap();
    let manifest = Manifest {
        roots: vec!["escape".into()],
        entries: vec![
            Entry {
                path: "escape".into(),
                kind: EntryKind::Symlink,
                size: 0,
                mode: 0,
                link_target: Some(target.path().to_string_lossy().into_owned()),
            },
            Entry {
                path: "escape/overwrite".into(),
                kind: EntryKind::File,
                size: 0,
                mode: 0,
                link_target: None,
            },
        ],
    };
    let header = serde_json::to_vec(&manifest).unwrap();
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&u32::try_from(header.len()).unwrap().to_be_bytes());
    bytes.extend_from_slice(&header);
    assert!(decode(&bytes, &target.path().join("received")).is_err());
    assert!(!target.path().join("overwrite").exists());
    assert!(!target.path().join("received").exists());
}

#[test]
fn gnome_only_selection_is_bundled_without_a_uri_list_alias() {
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("manual.pdf");
    fs::write(&file, b"pdf bytes").unwrap();
    let mut representations = vec![Representation {
        item: 0,
        format: "x-special/gnome-copied-files".into(),
        data: format!("copy\n{}", path_to_uri(&file)).into_bytes(),
    }];
    attach_bundle(&mut representations, 1024).unwrap();
    assert!(representations.iter().any(|r| r.format == BUNDLE_FORMAT));
}
