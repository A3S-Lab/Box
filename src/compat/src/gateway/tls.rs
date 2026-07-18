use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;

use super::{DataPlaneGatewayError, DataPlaneGatewayResult};

const MAX_TLS_FILE_BYTES: u64 = 4 * 1024 * 1024;

pub(super) async fn load_server_config(
    certificate_path: &Path,
    private_key_path: &Path,
) -> DataPlaneGatewayResult<Arc<ServerConfig>> {
    let certificate_bytes = read_limited(certificate_path, true).await?;
    let private_key_bytes = read_limited(private_key_path, false).await?;

    let certificates = rustls_pemfile::certs(&mut Cursor::new(certificate_bytes))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| DataPlaneGatewayError::ReadCertificate {
            path: certificate_path.to_path_buf(),
            source,
        })?;
    if certificates.is_empty() {
        return Err(DataPlaneGatewayError::MissingCertificate(
            certificate_path.to_path_buf(),
        ));
    }
    let private_key = rustls_pemfile::private_key(&mut Cursor::new(private_key_bytes))
        .map_err(|source| DataPlaneGatewayError::ReadPrivateKey {
            path: private_key_path.to_path_buf(),
            source,
        })?
        .ok_or_else(|| DataPlaneGatewayError::MissingPrivateKey(private_key_path.to_path_buf()))?;

    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(DataPlaneGatewayError::InvalidTls)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

async fn read_limited(path: &Path, certificate: bool) -> DataPlaneGatewayResult<Vec<u8>> {
    let metadata = tokio::fs::metadata(path).await.map_err(|source| {
        if certificate {
            DataPlaneGatewayError::ReadCertificate {
                path: path.to_path_buf(),
                source,
            }
        } else {
            DataPlaneGatewayError::ReadPrivateKey {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_TLS_FILE_BYTES {
        let source = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "TLS file must be a non-empty regular file no larger than {MAX_TLS_FILE_BYTES} bytes"
            ),
        );
        return Err(if certificate {
            DataPlaneGatewayError::ReadCertificate {
                path: path.to_path_buf(),
                source,
            }
        } else {
            DataPlaneGatewayError::ReadPrivateKey {
                path: path.to_path_buf(),
                source,
            }
        });
    }
    tokio::fs::read(path).await.map_err(|source| {
        if certificate {
            DataPlaneGatewayError::ReadCertificate {
                path: path.to_path_buf(),
                source,
            }
        } else {
            DataPlaneGatewayError::ReadPrivateKey {
                path: path.to_path_buf(),
                source,
            }
        }
    })
}
