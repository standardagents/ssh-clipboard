use super::*;
use crate::update::NpmDistribution;

#[tokio::test]
#[ignore = "downloads a published package from npm; run explicitly for release verification"]
async fn published_linux_peer_binary_downloads_and_reuses_offline() {
    let root = tempfile::tempdir().unwrap();
    let path = resolve(root.path(), CURRENT_VERSION, "linux-amd64", || {
        download(CURRENT_VERSION, "linux-amd64")
    })
    .await
    .unwrap();
    let bytes = tokio::fs::read(&path).await.unwrap();
    validate_executable_target(&bytes, "linux-amd64").unwrap();
    assert!(bytes.len() > 1024);
    assert_eq!(
        resolve(root.path(), CURRENT_VERSION, "linux-amd64", || async {
            bail!("offline")
        })
        .await
        .unwrap(),
        path
    );
}

fn fixture(target: &str) -> Vec<u8> {
    crate::update::tests::fake_binary(target)
}

#[tokio::test]
async fn standalone_download_is_cached_and_reused_offline() {
    let root = tempfile::tempdir().unwrap();
    let expected = fixture("linux-amd64");
    let path = resolve(root.path(), "0.2.11", "linux-amd64", || async {
        Ok(expected.clone())
    })
    .await
    .unwrap();
    assert_eq!(tokio::fs::read(&path).await.unwrap(), expected);
    let cached = resolve(root.path(), "0.2.11", "linux-amd64", || async {
        bail!("offline")
    })
    .await
    .unwrap();
    assert_eq!(path, cached);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[tokio::test]
async fn corrupt_cache_is_replaced_from_verified_download() {
    let root = tempfile::tempdir().unwrap();
    let path = resolve(root.path(), "0.2.11", "linux-amd64", || async {
        Ok(fixture("linux-amd64"))
    })
    .await
    .unwrap();
    tokio::fs::write(&path, b"corrupt").await.unwrap();
    assert!(
        resolve(root.path(), "0.2.11", "linux-amd64", || async {
            bail!("offline")
        })
        .await
        .is_err()
    );
    let repaired = resolve(root.path(), "0.2.11", "linux-amd64", || async {
        Ok(fixture("linux-amd64"))
    })
    .await
    .unwrap();
    assert_eq!(path, repaired);
    assert_eq!(tokio::fs::read(path).await.unwrap(), fixture("linux-amd64"));
}

#[tokio::test]
async fn versions_and_architectures_have_separate_cache_entries() {
    let root = tempfile::tempdir().unwrap();
    let first = resolve(root.path(), "0.2.11", "linux-amd64", || async {
        Ok(fixture("linux-amd64"))
    })
    .await
    .unwrap();
    assert!(
        resolve(root.path(), "0.2.12", "linux-amd64", || async {
            bail!("offline")
        })
        .await
        .is_err()
    );
    assert!(
        resolve(root.path(), "0.2.11", "linux-arm64", || async {
            bail!("offline")
        })
        .await
        .is_err()
    );
    let second = resolve(root.path(), "0.2.11", "darwin-arm64", || async {
        Ok(fixture("darwin-arm64"))
    })
    .await
    .unwrap();
    assert_ne!(first, second);
}

#[tokio::test]
async fn wrong_architecture_and_download_failures_leave_no_usable_entry() {
    let root = tempfile::tempdir().unwrap();
    assert!(
        resolve(root.path(), "0.2.11", "linux-amd64", || async {
            Ok(fixture("darwin-arm64"))
        })
        .await
        .is_err()
    );
    assert!(!root.path().join("0.2.11/linux-amd64/ssh-clipboard").exists());
    assert!(
        resolve(root.path(), "0.2.11", "linux-amd64", || async {
            bail!("integrity mismatch")
        })
        .await
        .is_err()
    );
}

#[test]
fn metadata_is_pinned_to_the_requested_release() {
    assert_eq!(
        release_url("0.2.11").unwrap().as_str(),
        "https://registry.npmjs.org/ssh-clipboard/0.2.11"
    );
    assert!(release_url("../latest").is_err());
    assert!(release_url("latest").is_err());
    assert!(release_url("0.2.12-dev.abcdef0").is_ok());
    let release = NpmRelease {
        version: "0.2.12".into(),
        dist: NpmDistribution {
            tarball: String::new(),
            integrity: String::new(),
        },
    };
    assert!(require_version(&release, "0.2.11").is_err());
}
