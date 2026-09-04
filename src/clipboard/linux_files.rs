use std::path::Path;

use crate::{filebundle::path_to_uri, model::Representation};

/// File-manager copy formats shared by X11 and Wayland. Do not publish sender
/// paths, native macOS types, rendered icons, or an inherited cut operation.
pub(super) fn representations(paths: &[String]) -> Vec<Representation> {
    let uris = paths
        .iter()
        .map(|path| path_to_uri(Path::new(path)))
        .collect::<Vec<_>>();
    let copied = format!("copy\n{}", uris.join("\n")).into_bytes();
    [
        ("text/uri-list", uris.join("\r\n").into_bytes()),
        ("x-special/gnome-copied-files", copied.clone()),
        ("x-special/nautilus-clipboard", copied),
        ("application/x-kde-cutselection", b"0".to_vec()),
    ]
    .into_iter()
    .map(|(format, data)| Representation {
        item: 0,
        format: format.into(),
        data,
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_encoded_local_paths_and_copy_semantics_for_all_file_types() {
        let paths = vec![
            "/tmp/Manual #1.pdf".into(),
            "/tmp/磁盘.dmg".into(),
            "/tmp/Installer.pkg".into(),
        ];
        let formats = representations(&paths);
        assert_eq!(formats.len(), 4);
        assert_eq!(
            String::from_utf8_lossy(&formats[0].data),
            "file:///tmp/Manual%20%231.pdf\r\nfile:///tmp/%E7%A3%81%E7%9B%98.dmg\r\nfile:///tmp/Installer.pkg"
        );
        assert!(formats[1].data.starts_with(b"copy\nfile://"));
        assert_eq!(formats[1].data, formats[2].data);
        assert_eq!(formats[3].data, b"0");
        assert!(
            formats
                .iter()
                .all(|r| !r.format.starts_with("public.") && r.format != "text/plain")
        );
    }
}
