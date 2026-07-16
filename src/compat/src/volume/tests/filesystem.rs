#![cfg(unix)]

use std::os::unix::fs::symlink;
use std::sync::Arc;

use tempfile::tempdir;
use tokio::io::AsyncReadExt;

use super::super::*;

async fn filesystem() -> (tempfile::TempDir, std::path::PathBuf, VolumeFilesystem) {
    let directory = tempdir().unwrap();
    let root = directory.path().join("volume");
    std::fs::create_dir(&root).unwrap();
    let filesystem = VolumeFilesystem::new(Arc::new(IdentityVolumeIdMapper::current()));
    filesystem.initialize_root(&root).await.unwrap();
    (directory, root, filesystem)
}

#[tokio::test]
async fn streams_atomic_writes_and_returns_stable_depth_limited_metadata() {
    let (_directory, root, filesystem) = filesystem().await;
    filesystem
        .make_dir(
            &root,
            "/nested/deep",
            VolumeMetadataUpdate::default(),
            true,
        )
        .await
        .unwrap();

    let mut initial = filesystem
        .begin_write(
            &root,
            "/nested/deep/data.txt",
            VolumeMetadataUpdate::default(),
            true,
        )
        .await
        .unwrap();
    initial.write_all(b"old").await.unwrap();
    initial.finish().await.unwrap();

    let mut replacement = filesystem
        .begin_write(
            &root,
            "/nested/deep/data.txt",
            VolumeMetadataUpdate::default(),
            true,
        )
        .await
        .unwrap();
    replacement.write_all(b"new-").await.unwrap();
    replacement.write_all(b"value").await.unwrap();

    let visible_during_upload = filesystem.list(&root, "/nested/deep", 1).await.unwrap();
    assert_eq!(
        visible_during_upload
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["/nested/deep/data.txt"]
    );
    assert_eq!(read(&filesystem, &root, "/nested/deep/data.txt").await, b"old");

    let entry = replacement.finish().await.unwrap();
    assert_eq!(entry.size, 9);
    assert_eq!(entry.mode, 0o644);
    assert_eq!(entry.uid, 0);
    assert_eq!(entry.gid, 0);
    assert_eq!(
        read(&filesystem, &root, "/nested/deep/data.txt").await,
        b"new-value"
    );

    assert_eq!(paths(filesystem.list(&root, "/", 1).await.unwrap()), vec!["/nested"]);
    assert_eq!(
        paths(filesystem.list(&root, "/", 2).await.unwrap()),
        vec!["/nested", "/nested/deep"]
    );
    assert_eq!(
        paths(filesystem.list(&root, "/", 3).await.unwrap()),
        vec![
            "/nested",
            "/nested/deep",
            "/nested/deep/data.txt"
        ]
    );

    let updated = filesystem
        .update_metadata(
            &root,
            "/nested/deep/data.txt",
            VolumeMetadataUpdate {
                mode: Some(0o600),
                ..VolumeMetadataUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.mode, 0o600);
    assert!(matches!(
        filesystem
            .begin_write(
                &root,
                "/nested/deep/data.txt",
                VolumeMetadataUpdate::default(),
                false,
            )
            .await,
        Err(VolumeContentError::Conflict)
    ));

    let mut abandoned = filesystem
        .begin_write(
            &root,
            "/nested/deep/abandoned.txt",
            VolumeMetadataUpdate::default(),
            true,
        )
        .await
        .unwrap();
    abandoned.write_all(b"partial").await.unwrap();
    drop(abandoned);
    assert!(matches!(
        filesystem
            .stat(&root, "/nested/deep/abandoned.txt")
            .await,
        Err(VolumeContentError::NotFound)
    ));

    filesystem.remove(&root, "/nested").await.unwrap();
    assert!(filesystem.list(&root, "/", 3).await.unwrap().is_empty());
}

#[tokio::test]
async fn rejects_traversal_reserved_paths_symlink_escape_and_root_mutation() {
    let (_directory, root, filesystem) = filesystem().await;
    let outside = tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
    symlink(outside.path(), root.join("escape")).unwrap();

    let link = filesystem.stat(&root, "/escape").await.unwrap();
    assert_eq!(link.entry_type, VolumeEntryType::Symlink);
    assert!(matches!(
        filesystem.stat(&root, "/escape/secret.txt").await,
        Err(VolumeContentError::InvalidPath(_))
    ));
    assert!(matches!(
        filesystem
            .begin_write(
                &root,
                "/escape/new.txt",
                VolumeMetadataUpdate::default(),
                true,
            )
            .await,
        Err(VolumeContentError::InvalidPath(_))
    ));

    for path in ["relative", "/../escape", "/nested/../escape", "/.a3s-upload-user"] {
        assert!(matches!(
            filesystem.stat(&root, path).await,
            Err(VolumeContentError::InvalidPath(_))
        ));
    }
    assert!(matches!(
        filesystem.remove(&root, "/").await,
        Err(VolumeContentError::InvalidPath(_))
    ));
    assert!(matches!(
        filesystem
            .make_dir(&root, "/", VolumeMetadataUpdate::default(), true)
            .await,
        Err(VolumeContentError::InvalidPath(_))
    ));
    assert!(matches!(
        filesystem.list(&root, "/", MAX_DIRECTORY_DEPTH + 1).await,
        Err(VolumeContentError::InvalidPath(_))
    ));

    filesystem.remove(&root, "/escape").await.unwrap();
    assert_eq!(std::fs::read(outside.path().join("secret.txt")).unwrap(), b"secret");
}

async fn read(filesystem: &VolumeFilesystem, root: &std::path::Path, path: &str) -> Vec<u8> {
    let mut file = filesystem.open_file(root, path).await.unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.unwrap();
    bytes
}

fn paths(entries: Vec<VolumeEntry>) -> Vec<String> {
    entries.into_iter().map(|entry| entry.path).collect()
}
