//! CRI ImageService implementation.
//!
//! Maps CRI image operations to A3S Box ImageStore and ImagePuller.

use std::sync::Arc;

use tonic::{Request, Response, Status};

use a3s_box_runtime::oci::{ImagePuller, ImageStore, RegistryAuth};

use crate::cri_api::image_service_server::ImageService;
use crate::cri_api::*;
use crate::error::box_error_to_status;

/// A3S Box implementation of the CRI ImageService.
pub struct BoxImageService {
    image_store: Arc<ImageStore>,
    image_puller: Arc<ImagePuller>,
}

impl BoxImageService {
    /// Create a new BoxImageService.
    pub fn new(image_store: Arc<ImageStore>, auth: RegistryAuth) -> Self {
        let image_puller = Arc::new(ImagePuller::new(image_store.clone(), auth));
        Self {
            image_store,
            image_puller,
        }
    }
}

#[tonic::async_trait]
impl ImageService for BoxImageService {
    async fn list_images(
        &self,
        request: Request<ListImagesRequest>,
    ) -> Result<Response<ListImagesResponse>, Status> {
        let _req = request.into_inner();

        let stored_images = self.image_store.list().await;

        let images: Vec<Image> = stored_images
            .into_iter()
            .map(|img| Image {
                id: img.digest.clone(),
                repo_tags: vec![img.reference.clone()],
                repo_digests: vec![format!("{}@{}", img.reference, img.digest)],
                size: img.size_bytes,
                uid: None,
                username: String::new(),
                spec: Some(ImageSpec {
                    image: img.reference,
                    annotations: Default::default(),
                }),
                pinned: false,
            })
            .collect();

        Ok(Response::new(ListImagesResponse { images }))
    }

    async fn image_status(
        &self,
        request: Request<ImageStatusRequest>,
    ) -> Result<Response<ImageStatusResponse>, Status> {
        let req = request.into_inner();
        let image_spec = req
            .image
            .ok_or_else(|| Status::invalid_argument("image spec required"))?;

        let stored = self.image_store.get(&image_spec.image).await;

        let image = stored.map(|img| Image {
            id: img.digest.clone(),
            repo_tags: vec![img.reference.clone()],
            repo_digests: vec![format!("{}@{}", img.reference, img.digest)],
            size: img.size_bytes,
            uid: None,
            username: String::new(),
            spec: Some(ImageSpec {
                image: img.reference,
                annotations: Default::default(),
            }),
            pinned: false,
        });

        Ok(Response::new(ImageStatusResponse {
            image,
            info: Default::default(),
        }))
    }

    async fn pull_image(
        &self,
        request: Request<PullImageRequest>,
    ) -> Result<Response<PullImageResponse>, Status> {
        let req = request.into_inner();
        let image_spec = req
            .image
            .ok_or_else(|| Status::invalid_argument("image spec required"))?;

        tracing::info!(image = %image_spec.image, "CRI PullImage");

        let _oci_image = self
            .image_puller
            .pull(&image_spec.image)
            .await
            .map_err(box_error_to_status)?;

        // Return the image reference as the image_ref
        Ok(Response::new(PullImageResponse {
            image_ref: image_spec.image,
        }))
    }

    async fn remove_image(
        &self,
        request: Request<RemoveImageRequest>,
    ) -> Result<Response<RemoveImageResponse>, Status> {
        let req = request.into_inner();
        let image_spec = req
            .image
            .ok_or_else(|| Status::invalid_argument("image spec required"))?;

        tracing::info!(image = %image_spec.image, "CRI RemoveImage");

        self.image_store
            .remove(&image_spec.image)
            .await
            .map_err(box_error_to_status)?;

        Ok(Response::new(RemoveImageResponse {}))
    }

    async fn image_fs_info(
        &self,
        _request: Request<ImageFsInfoRequest>,
    ) -> Result<Response<ImageFsInfoResponse>, Status> {
        let total_bytes = self.image_store.total_size().await;

        let usage = FilesystemUsage {
            timestamp: chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            fs_id: Some(FilesystemIdentifier {
                mountpoint: self.image_store.store_dir().to_string_lossy().to_string(),
            }),
            used_bytes: Some(UInt64Value { value: total_bytes }),
            inodes_used: None,
        };

        Ok(Response::new(ImageFsInfoResponse {
            image_filesystems: vec![usage],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_runtime::oci::ImageStore;

    /// Create a test ImageStore backed by a temp directory.
    fn make_test_store() -> (Arc<ImageStore>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("images");
        let store = ImageStore::new(&store_dir, 100 * 1024 * 1024).unwrap();
        (Arc::new(store), tmp)
    }

    /// Create a BoxImageService for testing.
    fn make_test_service() -> (BoxImageService, tempfile::TempDir) {
        let (store, tmp) = make_test_store();
        let svc = BoxImageService::new(store, RegistryAuth::anonymous());
        (svc, tmp)
    }

    /// Put a fake image into the store for testing.
    async fn put_test_image(store: &ImageStore, reference: &str, digest: &str) {
        let tmp = tempfile::tempdir().unwrap();
        // Create a minimal OCI layout
        std::fs::write(
            tmp.path().join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();
        let blobs_dir = tmp.path().join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        std::fs::write(blobs_dir.join("dummy"), b"test layer data").unwrap();

        store.put(reference, digest, tmp.path()).await.unwrap();
    }

    // ── ListImages ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_list_images_empty() {
        let (svc, _tmp) = make_test_service();
        let resp = svc
            .list_images(Request::new(ListImagesRequest { filter: None }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.images.is_empty());
    }

    #[tokio::test]
    async fn test_list_images_with_entries() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;
        put_test_image(&svc.image_store, "alpine:3.18", "sha256:bbb222").await;

        let resp = svc
            .list_images(Request::new(ListImagesRequest { filter: None }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.images.len(), 2);

        // Verify image fields
        let refs: Vec<&str> = resp
            .images
            .iter()
            .flat_map(|i| &i.repo_tags)
            .map(|s| s.as_str())
            .collect();
        assert!(refs.contains(&"nginx:latest"));
        assert!(refs.contains(&"alpine:3.18"));
    }

    // ── ImageStatus ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_image_status_missing_spec() {
        let (svc, _tmp) = make_test_service();
        let result = svc
            .image_status(Request::new(ImageStatusRequest {
                image: None,
                verbose: false,
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_image_status_not_found() {
        let (svc, _tmp) = make_test_service();
        let resp = svc
            .image_status(Request::new(ImageStatusRequest {
                image: Some(ImageSpec {
                    image: "nonexistent:latest".to_string(),
                    annotations: Default::default(),
                }),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.image.is_none());
    }

    #[tokio::test]
    async fn test_image_status_found() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        let resp = svc
            .image_status(Request::new(ImageStatusRequest {
                image: Some(ImageSpec {
                    image: "nginx:latest".to_string(),
                    annotations: Default::default(),
                }),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner();

        let image = resp.image.unwrap();
        assert_eq!(image.id, "sha256:aaa111");
        assert!(image.repo_tags.contains(&"nginx:latest".to_string()));
    }

    // ── RemoveImage ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remove_image_missing_spec() {
        let (svc, _tmp) = make_test_service();
        let result = svc
            .remove_image(Request::new(RemoveImageRequest { image: None }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_remove_image_not_found() {
        let (svc, _tmp) = make_test_service();
        let result = svc
            .remove_image(Request::new(RemoveImageRequest {
                image: Some(ImageSpec {
                    image: "nonexistent:latest".to_string(),
                    annotations: Default::default(),
                }),
            }))
            .await;
        // ImageStore.remove returns an error for missing images
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_remove_image_success() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        svc.remove_image(Request::new(RemoveImageRequest {
            image: Some(ImageSpec {
                image: "nginx:latest".to_string(),
                annotations: Default::default(),
            }),
        }))
        .await
        .unwrap();

        // Verify it's gone
        assert!(svc.image_store.get("nginx:latest").await.is_none());
    }

    // ── ImageFsInfo ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_image_fs_info_empty() {
        let (svc, _tmp) = make_test_service();
        let resp = svc
            .image_fs_info(Request::new(ImageFsInfoRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.image_filesystems.len(), 1);
        let fs = &resp.image_filesystems[0];
        assert_eq!(fs.used_bytes.as_ref().unwrap().value, 0);
    }

    #[tokio::test]
    async fn test_image_fs_info_with_images() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        let resp = svc
            .image_fs_info(Request::new(ImageFsInfoRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.image_filesystems.len(), 1);
        let fs = &resp.image_filesystems[0];
        assert!(fs.used_bytes.as_ref().unwrap().value > 0);
        assert!(fs.fs_id.is_some());
    }
}
