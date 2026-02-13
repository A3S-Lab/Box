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
