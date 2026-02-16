//! RA-TLS attestation server for AMD SEV-SNP.
//!
//! Listens on vsock port 4091 and serves TLS connections with an
//! RA-TLS certificate that embeds the SNP attestation report.
//! Clients verify the TEE attestation during the TLS handshake
//! by inspecting the custom X.509 extensions in the server certificate.
//!
//! ## Protocol
//!
//! 1. Server generates a P-384 key pair on startup
//! 2. Server obtains an SNP report with SHA-384(public_key) as report_data
//! 3. Server creates a self-signed X.509 cert embedding the report
//! 4. Client connects, TLS handshake delivers the cert
//! 5. Client's custom verifier extracts and verifies the SNP report
//! 6. After handshake, client sends a simple request, server responds with status

use std::io::Write;
#[cfg(target_os = "linux")]
use std::io::Read;

use tracing::info;
#[cfg(target_os = "linux")]
use tracing::warn;

/// Vsock port for the attestation server.
pub const ATTEST_VSOCK_PORT: u32 = a3s_transport::ports::TEE_CHANNEL;

/// SNP attestation report size (AMD SEV-SNP ABI spec v1.52).
#[cfg(target_os = "linux")]
const SNP_REPORT_SIZE: usize = 1184;

/// Size of the report_data field in the SNP report request.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SNP_USER_DATA_SIZE: usize = 64;

/// OID for the SNP attestation report extension.
/// Must match the OID in runtime/src/tee/ratls.rs.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const OID_SNP_REPORT: &[u64] = &[1, 3, 6, 1, 4, 1, 58270, 1, 1];

/// OID for the certificate chain extension.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const OID_CERT_CHAIN: &[u64] = &[1, 3, 6, 1, 4, 1, 58270, 1, 2];

// ============================================================================
// Public entry point
// ============================================================================

/// Run the RA-TLS attestation server on vsock port 4091.
///
/// On Linux with SEV-SNP (or simulation mode), generates an RA-TLS
/// certificate and serves TLS connections. Clients verify the TEE
/// attestation during the TLS handshake.
///
/// On non-Linux platforms, this is a no-op (development stub).
pub fn run_attest_server() -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting RA-TLS attestation server on vsock port {}", ATTEST_VSOCK_PORT);

    #[cfg(target_os = "linux")]
    {
        run_ratls_server()?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("RA-TLS attestation server not available on non-Linux platform (development mode)");
    }

    Ok(())
}

// ============================================================================
// RA-TLS server (Linux only)
// ============================================================================

/// Generate an RA-TLS certificate and serve TLS over vsock.
#[cfg(target_os = "linux")]
fn run_ratls_server() -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::{
        accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
    };
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::Arc;
    use std::time::Duration;
    use tracing::error;

    // Step 1: Generate key pair and RA-TLS certificate
    let (tls_config, cert_der, snp_report) = generate_ratls_config()?;
    let tls_config = Arc::new(tls_config);
    let snp_report = Arc::new(snp_report);

    info!(
        cert_size = cert_der.len(),
        "RA-TLS certificate generated, starting TLS listener"
    );

    // Step 2: Bind vsock listener
    let sock_fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;

    let addr = VsockAddr::new(libc::VMADDR_CID_ANY, ATTEST_VSOCK_PORT);
    bind(sock_fd.as_raw_fd(), &addr)?;
    listen(&sock_fd, Backlog::new(4)?)?;

    info!("RA-TLS attestation server listening on vsock port {}", ATTEST_VSOCK_PORT);

    // Step 3: Accept loop
    loop {
        match accept(sock_fd.as_raw_fd()) {
            Ok(client_fd) => {
                let client = unsafe { OwnedFd::from_raw_fd(client_fd) };
                let config = Arc::clone(&tls_config);
                let report = Arc::clone(&snp_report);
                if let Err(e) = handle_tls_connection(client, config, report) {
                    warn!("RA-TLS connection failed: {}", e);
                }
            }
            Err(e) => {
                error!("RA-TLS accept failed: {}", e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

// ============================================================================
// RA-TLS certificate generation
// ============================================================================

/// Generate a rustls ServerConfig with an RA-TLS certificate.
///
/// 1. Generate a P-384 key pair
/// 2. Hash the public key to create report_data
/// 3. Get an SNP report (or simulated) with that report_data
/// 4. Embed the report in a self-signed X.509 certificate
/// 5. Build a rustls ServerConfig
///
/// Returns (ServerConfig, cert_der, report_bytes).
#[cfg(target_os = "linux")]
fn generate_ratls_config() -> Result<(rustls::ServerConfig, Vec<u8>, Vec<u8>), Box<dyn std::error::Error>> {
    use rcgen::{
        CertificateParams, CustomExtension, DistinguishedName, DnType, KeyPair,
        PKCS_ECDSA_P384_SHA384,
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use sha2::{Digest, Sha256};

    // Generate P-384 key pair
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384)
        .map_err(|e| format!("Failed to generate key pair: {}", e))?;

    // Hash public key to create report_data (first 64 bytes of SHA-256)
    let pub_key_der = key_pair.public_key_der();
    let hash = Sha256::digest(&pub_key_der);
    let mut report_data = [0u8; SNP_USER_DATA_SIZE];
    let copy_len = hash.len().min(SNP_USER_DATA_SIZE);
    report_data[..copy_len].copy_from_slice(&hash[..copy_len]);

    // Get attestation report
    let (report_bytes, cert_chain_json) = if is_simulate_mode() {
        info!("Generating simulated RA-TLS attestation report");
        let report = build_simulated_report(&report_data);
        let chain_json = b"{}".to_vec();
        (report, chain_json)
    } else {
        info!("Requesting hardware SNP report for RA-TLS certificate");
        let resp = get_snp_report(&report_data)
            .map_err(|e| format!("Failed to get SNP report: {}", e))?;
        let chain_json = serde_json::to_vec(&resp.cert_chain)
            .unwrap_or_else(|_| b"{}".to_vec());
        (resp.report, chain_json)
    };

    // Build X.509 certificate with SNP report extensions
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "A3S Box RA-TLS");
    dn.push(DnType::OrganizationName, "A3S Lab");
    params.distinguished_name = dn;

    // Add SNP report as custom extension
    let snp_report = report_bytes.clone();
    let report_ext = CustomExtension::from_oid_content(OID_SNP_REPORT, report_bytes);
    params.custom_extensions.push(report_ext);

    // Add certificate chain as custom extension
    let chain_ext = CustomExtension::from_oid_content(OID_CERT_CHAIN, cert_chain_json);
    params.custom_extensions.push(chain_ext);

    // Self-sign
    let cert = params.self_signed(&key_pair)
        .map_err(|e| format!("Failed to generate RA-TLS certificate: {}", e))?;

    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();

    // Build rustls ServerConfig
    let _ = rustls::crypto::ring::default_provider().install_default();

    let tls_cert = CertificateDer::from(cert_der.clone());
    let tls_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![tls_cert], tls_key)
        .map_err(|e| format!("Failed to create TLS config: {}", e))?;

    Ok((config, cert_der, snp_report))
}

/// Directory where injected secrets are stored (tmpfs, never persisted to disk).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SECRETS_DIR: &str = "/run/secrets";

// ============================================================================
// TLS connection handler
// ============================================================================

/// Handle a single TLS connection over vsock.
///
/// Performs the TLS handshake (which delivers the RA-TLS certificate),
/// then reads a Frame-based request and routes it:
/// - `status` — Returns TEE status
/// - `secrets` — Receives and stores secrets
/// - `seal` — Seal data bound to TEE identity
/// - `unseal` — Unseal previously sealed data
/// - `process` — Forward to local agent
#[cfg(target_os = "linux")]
fn handle_tls_connection(
    fd: std::os::fd::OwnedFd,
    config: std::sync::Arc<rustls::ServerConfig>,
    snp_report: std::sync::Arc<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::tee::{AttestRequest, AttestRoute};
    use std::os::fd::{AsRawFd, FromRawFd};
    use tracing::debug;

    let raw_fd = fd.as_raw_fd();
    let tcp_stream = unsafe { std::net::TcpStream::from_raw_fd(raw_fd) };

    let conn = rustls::ServerConnection::new(config)
        .map_err(|e| format!("TLS connection init failed: {}", e))?;

    let mut tls = rustls::StreamOwned::new(conn, tcp_stream);

    // Read a Frame from the TLS stream
    match read_frame(&mut tls) {
        Ok(Some(frame)) => {
            if frame.0 != 0x01 {
                // Not a Data frame — send error
                debug!("RA-TLS received non-data frame type: 0x{:02x}", frame.0);
                send_error_response(&mut tls, "Expected Data frame");
            } else {
                // Parse the JSON request envelope
                match serde_json::from_slice::<AttestRequest>(&frame.1) {
                    Ok(req) => {
                        debug!("RA-TLS request: route={:?}", req.route);
                        match req.route {
                            AttestRoute::Secrets => {
                                handle_secret_injection(&req.payload, &mut tls);
                            }
                            AttestRoute::Seal => {
                                handle_seal_request(&req.payload, &snp_report, &mut tls);
                            }
                            AttestRoute::Unseal => {
                                handle_unseal_request(&req.payload, &snp_report, &mut tls);
                            }
                            AttestRoute::Process => {
                                handle_process_request(&req.payload, &mut tls);
                            }
                            AttestRoute::Status => {
                                send_data_response(
                                    &mut tls,
                                    b"{\"status\":\"ok\",\"tee\":true}",
                                );
                            }
                        }
                    }
                    Err(e) => {
                        debug!("RA-TLS invalid request JSON: {}", e);
                        send_error_response(
                            &mut tls,
                            &format!("Invalid request JSON: {}", e),
                        );
                    }
                }
            }
        }
        Ok(None) => {
            debug!("RA-TLS client disconnected after handshake");
        }
        Err(e) => {
            debug!("RA-TLS frame read error: {}", e);
        }
    }

    // Prevent double-close: OwnedFd and TcpStream both own the fd
    std::mem::forget(fd);
    Ok(())
}

/// Read a single frame from a synchronous stream.
/// Returns (frame_type, payload) or None on EOF.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn read_frame(r: &mut impl std::io::Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let frame_type = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }
    Ok(Some((frame_type, payload)))
}

/// Write a frame to a synchronous stream.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn write_frame(w: &mut impl Write, frame_type: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    let mut header = [0u8; 5];
    header[0] = frame_type;
    header[1..5].copy_from_slice(&len.to_be_bytes());
    w.write_all(&header)?;
    if !payload.is_empty() {
        w.write_all(payload)?;
    }
    Ok(())
}

/// Send a Data frame response (success).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn send_data_response(tls: &mut impl Write, body: &[u8]) {
    let _ = write_frame(tls, 0x01, body); // FrameType::Data
}

/// Send an Error frame response.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn send_error_response(tls: &mut impl Write, message: &str) {
    let _ = write_frame(tls, 0x04, message.as_bytes()); // FrameType::Error
}

// ============================================================================
// Secret injection
// ============================================================================

/// Secret injection request from the host.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct SecretInjectionRequest {
    /// Secrets to inject as key-value pairs.
    secrets: Vec<SecretEntry>,
}

/// A single secret entry.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct SecretEntry {
    /// Secret name (used as filename and env var name).
    name: String,
    /// Secret value.
    value: String,
    /// Whether to set as environment variable (default: true).
    #[serde(default = "default_true")]
    set_env: bool,
}

#[cfg(target_os = "linux")]
fn default_true() -> bool {
    true
}

/// Secret injection response.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize)]
struct SecretInjectionResponse {
    /// Number of secrets injected.
    injected: usize,
    /// Any errors encountered (non-fatal).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<String>,
}

/// Handle a secrets request: store secrets to /run/secrets/ and set env vars.
#[cfg(target_os = "linux")]
fn handle_secret_injection(payload: &serde_json::Value, tls: &mut impl Write) {
    let req: SecretInjectionRequest = match serde_json::from_value(payload.clone()) {
        Ok(r) => r,
        Err(e) => {
            send_error_response(tls, &format!("Invalid secrets payload: {}", e));
            return;
        }
    };

    let mut injected = 0;
    let mut errors = Vec::new();

    // Ensure secrets directory exists
    if let Err(e) = std::fs::create_dir_all(SECRETS_DIR) {
        send_error_response(tls, &format!("Failed to create secrets dir: {}", e));
        return;
    }

    for entry in &req.secrets {
        // Validate name (alphanumeric, underscore, dash, dot only)
        if !is_valid_secret_name(&entry.name) {
            errors.push(format!("Invalid secret name: {}", entry.name));
            continue;
        }

        // Write to /run/secrets/<name>
        let path = format!("{}/{}", SECRETS_DIR, entry.name);
        match std::fs::write(&path, entry.value.as_bytes()) {
            Ok(()) => {
                // Set restrictive permissions (owner read only)
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400));
                }

                // Set environment variable if requested
                if entry.set_env {
                    std::env::set_var(&entry.name, &entry.value);
                }

                injected += 1;
                info!("Secret injected: {}", entry.name);
            }
            Err(e) => {
                errors.push(format!("Failed to write {}: {}", entry.name, e));
            }
        }
    }

    let response = SecretInjectionResponse { injected, errors };
    let body = serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"injected\":0}".to_vec());
    send_data_response(tls, &body);
}

/// Validate a secret name: alphanumeric, underscore, dash, dot only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.contains('/')
        && !name.contains('\0')
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
}

// ============================================================================
// Message processing (POST /process)
// ============================================================================

/// Process request from the host via SafeClaw.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct ProcessRequest {
    /// Session identifier.
    session_id: String,
    /// Message content to process.
    content: String,
    /// Request type: "process_message", "init_session", "terminate_session".
    #[serde(default = "default_request_type")]
    request_type: String,
}

#[cfg(target_os = "linux")]
fn default_request_type() -> String {
    "process_message".to_string()
}

/// Process response returned to the host.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize)]
struct ProcessResponse {
    /// Session identifier.
    session_id: String,
    /// Response content from the TEE-resident agent.
    content: String,
    /// Whether processing succeeded.
    success: bool,
    /// Error message if processing failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Handle a process request: forward message to the local agent for processing.
///
/// The guest agent runs as a separate process inside the TEE. This handler
/// receives messages from the host (via RA-TLS), forwards them to the agent,
/// and returns the agent's response.
#[cfg(target_os = "linux")]
fn handle_process_request(payload: &serde_json::Value, tls: &mut impl Write) {
    let req: ProcessRequest = match serde_json::from_value(payload.clone()) {
        Ok(r) => r,
        Err(e) => {
            send_error_response(tls, &format!("Invalid process payload: {}", e));
            return;
        }
    };

    info!(
        session_id = %req.session_id,
        request_type = %req.request_type,
        content_len = req.content.len(),
        "Processing message in TEE"
    );

    // Forward to the local agent process via localhost HTTP.
    // The agent listens on 127.0.0.1:8080 inside the guest.
    let response = match forward_to_agent(&req) {
        Ok(content) => ProcessResponse {
            session_id: req.session_id,
            content,
            success: true,
            error: None,
        },
        Err(e) => {
            warn!("Agent processing failed: {}", e);
            ProcessResponse {
                session_id: req.session_id,
                content: String::new(),
                success: false,
                error: Some(e),
            }
        }
    };

    let body = serde_json::to_vec(&response).unwrap_or_else(|_| {
        b"{\"success\":false,\"error\":\"serialize\"}".to_vec()
    });
    if response.success {
        send_data_response(tls, &body);
    } else {
        send_error_response(tls, &String::from_utf8_lossy(&body));
    }
}

/// Forward a process request to the local agent via HTTP.
///
/// The agent runs inside the TEE and listens on localhost. This keeps
/// the attestation server (vsock-facing) separate from the agent (internal).
#[cfg(target_os = "linux")]
fn forward_to_agent(req: &ProcessRequest) -> std::result::Result<String, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let agent_addr = "127.0.0.1:8080";

    let mut stream = TcpStream::connect(agent_addr).map_err(|e| {
        format!("Cannot connect to agent at {}: {}", agent_addr, e)
    })?;

    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| format!("Failed to set read timeout: {}", e))?;

    // Build JSON payload for the agent
    let payload = serde_json::json!({
        "session_id": req.session_id,
        "content": req.content,
        "request_type": req.request_type,
    });
    let payload_bytes = serde_json::to_vec(&payload)
        .map_err(|e| format!("Failed to serialize agent request: {}", e))?;

    // Send HTTP POST to agent
    let http_request = format!(
        "POST /process HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload_bytes.len()
    );
    stream
        .write_all(http_request.as_bytes())
        .map_err(|e| format!("Failed to write to agent: {}", e))?;
    stream
        .write_all(&payload_bytes)
        .map_err(|e| format!("Failed to write payload to agent: {}", e))?;

    // Read response
    let mut response = Vec::with_capacity(65536);
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("Failed to read agent response: {}", e))?;

    let response_str = String::from_utf8_lossy(&response);

    // Parse HTTP response body
    let body = response_str
        .find("\r\n\r\n")
        .map(|pos| &response_str[pos + 4..])
        .unwrap_or(&response_str);

    // Extract content from agent response JSON
    let agent_resp: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("Invalid agent response JSON: {}", e))?;

    agent_resp
        .get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "Agent response missing 'content' field".to_string())
}

// ============================================================================
// Sealed storage (guest-side)
// ============================================================================

/// Seal request from the host.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct SealRequest {
    /// Data to seal (base64-encoded).
    data: String,
    /// Application-specific context for key derivation.
    context: String,
    /// Sealing policy: "MeasurementAndChip", "MeasurementOnly", or "ChipOnly".
    #[serde(default = "default_policy")]
    policy: String,
}

#[cfg(target_os = "linux")]
fn default_policy() -> String {
    "MeasurementAndChip".to_string()
}

/// Seal response returned to the host.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize)]
struct SealResponse {
    /// Sealed blob (base64-encoded): nonce || ciphertext || tag.
    blob: String,
    /// Policy used for sealing.
    policy: String,
    /// Context used for key derivation.
    context: String,
}

/// Unseal request from the host.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct UnsealRequest {
    /// Sealed blob (base64-encoded).
    blob: String,
    /// Context used during sealing.
    context: String,
    /// Sealing policy used during sealing.
    #[serde(default = "default_policy")]
    policy: String,
}

/// Unseal response returned to the host.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize)]
struct UnsealResponse {
    /// Decrypted data (base64-encoded).
    data: String,
}

/// HKDF salt — must match runtime/src/tee/sealed.rs.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const HKDF_SALT: &[u8] = b"a3s-sealed-storage-v1";

/// Handle a seal request: encrypt data bound to TEE identity.
#[cfg(target_os = "linux")]
fn handle_seal_request(payload: &serde_json::Value, snp_report: &[u8], tls: &mut impl Write) {
    use base64::Engine;
    use ring::aead::{self, Aad, BoundKey, Nonce, NonceSequence, NONCE_LEN};

    let req: SealRequest = match serde_json::from_value(payload.clone()) {
        Ok(r) => r,
        Err(e) => {
            send_error_response(tls, &format!("Invalid seal payload: {}", e));
            return;
        }
    };

    // Decode plaintext from base64
    let plaintext = match base64::engine::general_purpose::STANDARD.decode(&req.data) {
        Ok(d) => d,
        Err(e) => {
            send_error_response(tls, &format!("Invalid base64 data: {}", e));
            return;
        }
    };

    // Derive sealing key
    let key = match derive_guest_sealing_key(snp_report, &req.context, &req.policy) {
        Ok(k) => k,
        Err(e) => {
            send_error_response(tls, &e);
            return;
        }
    };

    // Generate random nonce
    let rng = ring::rand::SystemRandom::new();
    let mut nonce_bytes = [0u8; NONCE_LEN];
    if ring::rand::SecureRandom::fill(&rng, &mut nonce_bytes).is_err() {
        send_error_response(tls, "Failed to generate nonce");
        return;
    }

    // Encrypt with AES-256-GCM
    let mut in_out = plaintext;
    let unbound_key = match aead::UnboundKey::new(&aead::AES_256_GCM, &key) {
        Ok(k) => k,
        Err(_) => {
            send_error_response(tls, "Failed to create encryption key");
            return;
        }
    };

    struct SingleNonce(Option<[u8; 12]>);
    impl NonceSequence for SingleNonce {
        fn advance(&mut self) -> std::result::Result<Nonce, ring::error::Unspecified> {
            self.0.take().map(Nonce::assume_unique_for_key).ok_or(ring::error::Unspecified)
        }
    }

    let mut sealing_key = aead::SealingKey::new(unbound_key, SingleNonce(Some(nonce_bytes)));
    if sealing_key
        .seal_in_place_append_tag(Aad::from(req.context.as_bytes()), &mut in_out)
        .is_err()
    {
        send_error_response(tls, "Encryption failed");
        return;
    }

    // Build blob: nonce || ciphertext || tag
    let mut blob = Vec::with_capacity(NONCE_LEN + in_out.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&in_out);

    let response = SealResponse {
        blob: base64::engine::general_purpose::STANDARD.encode(&blob),
        policy: req.policy,
        context: req.context,
    };

    let body = serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"error\":\"serialize\"}".to_vec());
    send_data_response(tls, &body);
    info!("Sealed {} bytes of data", blob.len());
}

/// Handle an unseal request: decrypt data using TEE identity.
#[cfg(target_os = "linux")]
fn handle_unseal_request(payload: &serde_json::Value, snp_report: &[u8], tls: &mut impl Write) {
    use base64::Engine;
    use ring::aead::{self, Aad, BoundKey, Nonce, NonceSequence, NONCE_LEN};

    let req: UnsealRequest = match serde_json::from_value(payload.clone()) {
        Ok(r) => r,
        Err(e) => {
            send_error_response(tls, &format!("Invalid unseal payload: {}", e));
            return;
        }
    };

    // Decode sealed blob from base64
    let blob = match base64::engine::general_purpose::STANDARD.decode(&req.blob) {
        Ok(d) => d,
        Err(e) => {
            send_error_response(tls, &format!("Invalid base64 blob: {}", e));
            return;
        }
    };

    if blob.len() < NONCE_LEN + aead::AES_256_GCM.tag_len() {
        send_error_response(tls, "Sealed blob too short");
        return;
    }

    // Derive sealing key
    let key = match derive_guest_sealing_key(snp_report, &req.context, &req.policy) {
        Ok(k) => k,
        Err(e) => {
            send_error_response(tls, &e);
            return;
        }
    };

    // Split nonce and ciphertext
    let nonce_bytes: [u8; NONCE_LEN] = match blob[..NONCE_LEN].try_into() {
        Ok(n) => n,
        Err(_) => {
            send_error_response(tls, "Invalid nonce");
            return;
        }
    };
    let mut in_out = blob[NONCE_LEN..].to_vec();

    // Decrypt with AES-256-GCM
    let unbound_key = match aead::UnboundKey::new(&aead::AES_256_GCM, &key) {
        Ok(k) => k,
        Err(_) => {
            send_error_response(tls, "Failed to create decryption key");
            return;
        }
    };

    struct SingleNonce(Option<[u8; 12]>);
    impl NonceSequence for SingleNonce {
        fn advance(&mut self) -> std::result::Result<Nonce, ring::error::Unspecified> {
            self.0.take().map(Nonce::assume_unique_for_key).ok_or(ring::error::Unspecified)
        }
    }

    let mut opening_key = aead::OpeningKey::new(unbound_key, SingleNonce(Some(nonce_bytes)));
    let plaintext = match opening_key.open_in_place(Aad::from(req.context.as_bytes()), &mut in_out) {
        Ok(pt) => pt,
        Err(_) => {
            send_error_response(tls, "Unseal failed: TEE identity mismatch or data corrupted");
            return;
        }
    };

    let response = UnsealResponse {
        data: base64::engine::general_purpose::STANDARD.encode(plaintext),
    };

    let body = serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"error\":\"serialize\"}".to_vec());
    send_data_response(tls, &body);
    info!("Unsealed data successfully");
}

/// Derive a 256-bit sealing key from the SNP report using HKDF-SHA256.
///
/// Algorithm matches `runtime/src/tee/sealed.rs::derive_sealing_key`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn derive_guest_sealing_key(
    report: &[u8],
    context: &str,
    policy: &str,
) -> std::result::Result<[u8; 32], String> {
    use ring::hkdf;

    if report.len() < 0x1E0 {
        return Err("Report too short to extract sealing identity".to_string());
    }

    let measurement = &report[0x90..0xC0]; // 48 bytes
    let chip_id = &report[0x1A0..0x1E0]; // 64 bytes

    let ikm = match policy {
        "MeasurementAndChip" => {
            let mut v = Vec::with_capacity(112);
            v.extend_from_slice(measurement);
            v.extend_from_slice(chip_id);
            v
        }
        "MeasurementOnly" => measurement.to_vec(),
        "ChipOnly" => chip_id.to_vec(),
        _ => {
            let mut v = Vec::with_capacity(112);
            v.extend_from_slice(measurement);
            v.extend_from_slice(chip_id);
            v
        }
    };

    struct HkdfLen(usize);
    impl hkdf::KeyType for HkdfLen {
        fn len(&self) -> usize {
            self.0
        }
    }

    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, HKDF_SALT);
    let prk = salt.extract(&ikm);
    let info = [context.as_bytes()];
    let okm = prk
        .expand(&info, HkdfLen(32))
        .map_err(|_| "HKDF expand failed".to_string())?;

    let mut key = [0u8; 32];
    okm.fill(&mut key)
        .map_err(|_| "HKDF fill failed".to_string())?;

    Ok(key)
}

// ============================================================================
// Simulation mode
// ============================================================================

/// Check if TEE simulation mode is enabled via environment variable.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_simulate_mode() -> bool {
    std::env::var("A3S_TEE_SIMULATE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Simulated SNP report version marker (0xA3 = "A3S").
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SIMULATED_REPORT_VERSION: u32 = 0xA3;

/// Generate a simulated 1184-byte SNP report.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn build_simulated_report(report_data: &[u8; SNP_USER_DATA_SIZE]) -> Vec<u8> {
    let mut report = vec![0u8; 1184];

    // version at 0x00 — simulated marker
    report[0x00..0x04].copy_from_slice(&SIMULATED_REPORT_VERSION.to_le_bytes());
    // guest_svn at 0x04
    report[0x04..0x08].copy_from_slice(&1u32.to_le_bytes());
    // policy at 0x08
    report[0x08..0x10].copy_from_slice(&0u64.to_le_bytes());
    // current_tcb at 0x38
    report[0x38] = 3;   // boot_loader
    report[0x39] = 0;   // tee
    report[0x3E] = 8;   // snp
    report[0x3F] = 115; // microcode
    // report_data at 0x50
    report[0x50..0x90].copy_from_slice(report_data);
    // measurement at 0x90 (deterministic fake)
    for i in 0..48 {
        report[0x90 + i] = (i as u8).wrapping_mul(0xA3);
    }
    // chip_id at 0x1A0 (all 0xA3)
    for b in &mut report[0x1A0..0x1E0] {
        *b = 0xA3;
    }
    // signature at 0x2A0 — left as zeros (simulation marker)

    report
}

// ============================================================================
// SNP report types (local to avoid cross-crate dependency)
// ============================================================================

/// Certificate chain from the SNP extended report.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize, Default)]
struct CertChain {
    vcek: Vec<u8>,
    ask: Vec<u8>,
    ark: Vec<u8>,
}

/// Attestation response (used internally for SNP report + certs).
#[cfg(target_os = "linux")]
struct AttestResponse {
    report: Vec<u8>,
    cert_chain: CertChain,
}

// ============================================================================
// SNP ioctl interface (/dev/sev-guest)
// ============================================================================

/// Get an SNP attestation report from the hardware via `/dev/sev-guest`.
#[cfg(target_os = "linux")]
fn get_snp_report(report_data: &[u8; SNP_USER_DATA_SIZE]) -> Result<AttestResponse, String> {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;

    let dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/sev-guest")
        .or_else(|_| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/sev")
        })
        .map_err(|e| format!("Cannot open SEV device: {} (is this a SEV-SNP VM?)", e))?;

    let fd = dev.as_raw_fd();

    // First try SNP_GET_EXT_REPORT (report + certs)
    match snp_get_ext_report(fd, report_data) {
        Ok(resp) => return Ok(resp),
        Err(e) => {
            tracing::debug!("SNP_GET_EXT_REPORT failed ({}), falling back to SNP_GET_REPORT", e);
        }
    }

    // Fallback: SNP_GET_REPORT (report only, no certs)
    let report = snp_get_report(fd, report_data)?;
    Ok(AttestResponse {
        report,
        cert_chain: CertChain::default(),
    })
}

// ============================================================================
// SNP ioctl structures (from linux/sev-guest.h)
// ============================================================================

#[cfg(target_os = "linux")]
const SNP_GET_REPORT_IOCTL: libc::c_ulong = 0xc018_5300;
#[cfg(target_os = "linux")]
const SNP_GET_EXT_REPORT_IOCTL: libc::c_ulong = 0xc018_5302;

#[cfg(target_os = "linux")]
#[repr(C)]
struct SnpReportReq {
    user_data: [u8; 64],
    vmpl: u32,
    rsvd: [u8; 28],
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct SnpReportResp {
    status: u32,
    report_size: u32,
    rsvd: [u8; 24],
    report: [u8; SNP_REPORT_SIZE],
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct SnpGuestRequestIoctl {
    msg_version: u8,
    req_data: u64,
    resp_data: u64,
    fw_err: u64,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct SnpExtReportReq {
    data: SnpReportReq,
    certs_address: u64,
    certs_len: u32,
}

/// Get SNP report via SNP_GET_REPORT ioctl.
#[cfg(target_os = "linux")]
fn snp_get_report(
    fd: libc::c_int,
    report_data: &[u8; SNP_USER_DATA_SIZE],
) -> Result<Vec<u8>, String> {
    let mut req = SnpReportReq {
        user_data: [0u8; 64],
        vmpl: 0,
        rsvd: [0u8; 28],
    };
    req.user_data.copy_from_slice(report_data);

    let mut resp = SnpReportResp {
        status: 0,
        report_size: 0,
        rsvd: [0u8; 24],
        report: [0u8; SNP_REPORT_SIZE],
    };

    let mut ioctl_req = SnpGuestRequestIoctl {
        msg_version: 1,
        req_data: &req as *const _ as u64,
        resp_data: &mut resp as *mut _ as u64,
        fw_err: 0,
    };

    let ret = unsafe {
        libc::ioctl(fd, SNP_GET_REPORT_IOCTL as libc::Ioctl, &mut ioctl_req as *mut _)
    };

    if ret != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(format!(
            "SNP_GET_REPORT ioctl failed: {} (fw_err: {:#x})",
            errno, ioctl_req.fw_err
        ));
    }

    if resp.status != 0 {
        return Err(format!("SNP_GET_REPORT firmware error: {:#x}", resp.status));
    }

    Ok(resp.report.to_vec())
}

/// Get SNP extended report (report + certificate chain).
#[cfg(target_os = "linux")]
fn snp_get_ext_report(
    fd: libc::c_int,
    report_data: &[u8; SNP_USER_DATA_SIZE],
) -> Result<AttestResponse, String> {
    const CERTS_BUF_SIZE: usize = 16384;
    let mut certs_buf = vec![0u8; CERTS_BUF_SIZE];

    let mut report_req = SnpReportReq {
        user_data: [0u8; 64],
        vmpl: 0,
        rsvd: [0u8; 28],
    };
    report_req.user_data.copy_from_slice(report_data);

    let mut ext_req = SnpExtReportReq {
        data: report_req,
        certs_address: certs_buf.as_mut_ptr() as u64,
        certs_len: CERTS_BUF_SIZE as u32,
    };

    let mut resp = SnpReportResp {
        status: 0,
        report_size: 0,
        rsvd: [0u8; 24],
        report: [0u8; SNP_REPORT_SIZE],
    };

    let mut ioctl_req = SnpGuestRequestIoctl {
        msg_version: 1,
        req_data: &mut ext_req as *mut _ as u64,
        resp_data: &mut resp as *mut _ as u64,
        fw_err: 0,
    };

    let ret = unsafe {
        libc::ioctl(fd, SNP_GET_EXT_REPORT_IOCTL as libc::Ioctl, &mut ioctl_req as *mut _)
    };

    if ret != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(format!(
            "SNP_GET_EXT_REPORT ioctl failed: {} (fw_err: {:#x})",
            errno, ioctl_req.fw_err
        ));
    }

    if resp.status != 0 {
        return Err(format!("SNP_GET_EXT_REPORT firmware error: {:#x}", resp.status));
    }

    let cert_chain = parse_cert_table(&certs_buf, ext_req.certs_len as usize);

    Ok(AttestResponse {
        report: resp.report.to_vec(),
        cert_chain,
    })
}

/// Parse the SNP certificate table returned by SNP_GET_EXT_REPORT.
#[cfg(target_os = "linux")]
fn parse_cert_table(buf: &[u8], len: usize) -> CertChain {
    const VCEK_GUID: [u8; 16] = guid_bytes("63da758d-e664-4564-adc5-f4b93be8accd");
    const ASK_GUID: [u8; 16] = guid_bytes("4ab7b379-bbac-4fe4-a02f-05aef327c782");
    const ARK_GUID: [u8; 16] = guid_bytes("c0b406a4-a803-4952-9743-3fb6014cd0ae");

    let mut chain = CertChain::default();
    if len < 24 {
        return chain;
    }

    let mut pos = 0;
    while pos + 24 <= len {
        let guid = &buf[pos..pos + 16];
        if guid.iter().all(|&b| b == 0) {
            break;
        }

        let offset = u32::from_le_bytes(buf[pos + 16..pos + 20].try_into().unwrap_or([0; 4])) as usize;
        let cert_len = u32::from_le_bytes(buf[pos + 20..pos + 24].try_into().unwrap_or([0; 4])) as usize;

        if offset + cert_len <= len {
            let cert_data = buf[offset..offset + cert_len].to_vec();
            if guid == VCEK_GUID {
                chain.vcek = cert_data;
            } else if guid == ASK_GUID {
                chain.ask = cert_data;
            } else if guid == ARK_GUID {
                chain.ark = cert_data;
            }
        }

        pos += 24;
    }

    chain
}

/// Convert a UUID string to little-endian bytes (AMD SEV-SNP format).
#[cfg(target_os = "linux")]
const fn guid_bytes(uuid: &str) -> [u8; 16] {
    let b = uuid.as_bytes();
    let mut out = [0u8; 16];

    let mut hex = [0u8; 32];
    let mut hi = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'-' {
            hex[hi] = hex_val(b[i]);
            hi += 1;
        }
        i += 1;
    }

    out[0] = hex[6] << 4 | hex[7];
    out[1] = hex[4] << 4 | hex[5];
    out[2] = hex[2] << 4 | hex[3];
    out[3] = hex[0] << 4 | hex[1];
    out[4] = hex[10] << 4 | hex[11];
    out[5] = hex[8] << 4 | hex[9];
    out[6] = hex[14] << 4 | hex[15];
    out[7] = hex[12] << 4 | hex[13];
    let mut j = 0;
    while j < 8 {
        out[8 + j] = hex[16 + j * 2] << 4 | hex[16 + j * 2 + 1];
        j += 1;
    }

    out
}

/// Convert a hex ASCII byte to its numeric value (const fn compatible).
#[cfg(target_os = "linux")]
const fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_attest_vsock_port_constant() {
        assert_eq!(ATTEST_VSOCK_PORT, a3s_transport::ports::TEE_CHANNEL);
    }

    #[test]
    fn test_is_simulate_mode_default() {
        // Should be false unless env var is set
        // (don't set it in tests to avoid side effects)
        let _ = is_simulate_mode();
    }

    #[test]
    fn test_build_simulated_report_size() {
        let data = [0u8; SNP_USER_DATA_SIZE];
        let report = build_simulated_report(&data);
        assert_eq!(report.len(), 1184);
    }

    #[test]
    fn test_build_simulated_report_version() {
        let data = [0u8; SNP_USER_DATA_SIZE];
        let report = build_simulated_report(&data);
        let version = u32::from_le_bytes(report[0..4].try_into().unwrap());
        assert_eq!(version, SIMULATED_REPORT_VERSION);
    }

    #[test]
    fn test_build_simulated_report_contains_report_data() {
        let mut data = [0u8; SNP_USER_DATA_SIZE];
        data[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let report = build_simulated_report(&data);
        assert_eq!(&report[0x50..0x54], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_oid_constants() {
        assert_eq!(OID_SNP_REPORT, &[1, 3, 6, 1, 4, 1, 58270, 1, 1]);
        assert_eq!(OID_CERT_CHAIN, &[1, 3, 6, 1, 4, 1, 58270, 1, 2]);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_guid_bytes() {
        let guid = guid_bytes("63da758d-e664-4564-adc5-f4b93be8accd");
        assert_eq!(guid[0], 0x8d);
        assert_eq!(guid[1], 0x75);
        assert_eq!(guid[2], 0xda);
        assert_eq!(guid[3], 0x63);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_hex_val() {
        assert_eq!(hex_val(b'0'), 0);
        assert_eq!(hex_val(b'9'), 9);
        assert_eq!(hex_val(b'a'), 10);
        assert_eq!(hex_val(b'f'), 15);
    }

    #[test]
    fn test_valid_secret_names() {
        assert!(is_valid_secret_name("API_KEY"));
        assert!(is_valid_secret_name("my-secret"));
        assert!(is_valid_secret_name("config.json"));
        assert!(is_valid_secret_name("a"));
        assert!(is_valid_secret_name("SECRET_123"));
    }

    #[test]
    fn test_invalid_secret_names() {
        assert!(!is_valid_secret_name(""));
        assert!(!is_valid_secret_name(".hidden"));
        assert!(!is_valid_secret_name("path/traversal"));
        assert!(!is_valid_secret_name("has space"));
        assert!(!is_valid_secret_name("null\0byte"));
        assert!(!is_valid_secret_name(&"x".repeat(257)));
    }

    #[test]
    fn test_secrets_dir_constant() {
        assert_eq!(SECRETS_DIR, "/run/secrets");
    }

    #[test]
    fn test_hkdf_salt_matches_runtime() {
        assert_eq!(HKDF_SALT, b"a3s-sealed-storage-v1");
    }

    /// Build a fake 1184-byte report with known measurement and chip_id.
    fn make_test_report() -> Vec<u8> {
        let mut report = vec![0u8; 1184];
        for i in 0..48 {
            report[0x90 + i] = (i as u8).wrapping_mul(0xA3);
        }
        for b in &mut report[0x1A0..0x1E0] {
            *b = 0xA3;
        }
        report
    }

    #[test]
    fn test_derive_guest_sealing_key_measurement_and_chip() {
        let report = make_test_report();
        let key = derive_guest_sealing_key(&report, "test-ctx", "MeasurementAndChip").unwrap();
        assert_eq!(key.len(), 32);
        // Key should be deterministic
        let key2 = derive_guest_sealing_key(&report, "test-ctx", "MeasurementAndChip").unwrap();
        assert_eq!(key, key2);
    }

    #[test]
    fn test_derive_guest_sealing_key_measurement_only() {
        let report = make_test_report();
        let key = derive_guest_sealing_key(&report, "ctx", "MeasurementOnly").unwrap();
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn test_derive_guest_sealing_key_chip_only() {
        let report = make_test_report();
        let key = derive_guest_sealing_key(&report, "ctx", "ChipOnly").unwrap();
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn test_derive_guest_sealing_key_different_contexts() {
        let report = make_test_report();
        let key_a = derive_guest_sealing_key(&report, "context-a", "MeasurementAndChip").unwrap();
        let key_b = derive_guest_sealing_key(&report, "context-b", "MeasurementAndChip").unwrap();
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn test_derive_guest_sealing_key_different_policies() {
        let report = make_test_report();
        let key_mc = derive_guest_sealing_key(&report, "ctx", "MeasurementAndChip").unwrap();
        let key_m = derive_guest_sealing_key(&report, "ctx", "MeasurementOnly").unwrap();
        let key_c = derive_guest_sealing_key(&report, "ctx", "ChipOnly").unwrap();
        assert_ne!(key_mc, key_m);
        assert_ne!(key_mc, key_c);
        assert_ne!(key_m, key_c);
    }

    #[test]
    fn test_derive_guest_sealing_key_report_too_short() {
        let short = vec![0u8; 100];
        let result = derive_guest_sealing_key(&short, "ctx", "MeasurementAndChip");
        assert!(result.is_err());
    }

    #[test]
    fn test_derive_guest_sealing_key_unknown_policy_defaults() {
        let report = make_test_report();
        // Unknown policy falls back to MeasurementAndChip
        let key_unknown = derive_guest_sealing_key(&report, "ctx", "Unknown").unwrap();
        let key_default = derive_guest_sealing_key(&report, "ctx", "MeasurementAndChip").unwrap();
        assert_eq!(key_unknown, key_default);
    }

    #[test]
    fn test_derive_guest_sealing_key_chip_only_survives_measurement_change() {
        let report = make_test_report();
        let key1 = derive_guest_sealing_key(&report, "ctx", "ChipOnly").unwrap();

        let mut changed = report.clone();
        changed[0x90] = 0xFF; // change measurement
        let key2 = derive_guest_sealing_key(&changed, "ctx", "ChipOnly").unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_derive_guest_sealing_key_measurement_only_survives_chip_change() {
        let report = make_test_report();
        let key1 = derive_guest_sealing_key(&report, "ctx", "MeasurementOnly").unwrap();

        let mut changed = report.clone();
        changed[0x1A0] = 0xFF; // change chip_id
        let key2 = derive_guest_sealing_key(&changed, "ctx", "MeasurementOnly").unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_process_request_deserialization() {
        let json = r#"{"session_id":"s1","content":"hello","request_type":"process_message"}"#;
        let _: serde_json::Value = serde_json::from_str(json).unwrap();
    }

    #[test]
    fn test_process_request_default_type() {
        let json = r#"{"session_id":"s1","content":"hello"}"#;
        let val: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(val["session_id"], "s1");
        assert_eq!(val["content"], "hello");
    }

    #[test]
    fn test_process_response_serialization() {
        let resp = serde_json::json!({
            "session_id": "s1",
            "content": "response text",
            "success": true,
        });
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("response text"));
        assert!(json.contains("\"success\":true"));
    }

    #[test]
    fn test_process_response_with_error() {
        let resp = serde_json::json!({
            "session_id": "s1",
            "content": "",
            "success": false,
            "error": "agent unreachable",
        });
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("agent unreachable"));
        assert!(json.contains("\"success\":false"));
    }

    #[test]
    fn test_frame_roundtrip() {
        let payload = b"hello frame";
        let mut buf = Vec::new();
        write_frame(&mut buf, 0x01, payload).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let (ft, data) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, 0x01);
        assert_eq!(data, payload);
    }

    #[test]
    fn test_frame_read_eof() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn test_frame_empty_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 0x04, b"").unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let (ft, data) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, 0x04);
        assert!(data.is_empty());
    }

    #[test]
    fn test_send_data_response() {
        let mut buf = Vec::new();
        send_data_response(&mut buf, b"{\"ok\":true}");
        let mut cursor = std::io::Cursor::new(buf);
        let (ft, data) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, 0x01); // Data
        assert_eq!(data, b"{\"ok\":true}");
    }

    #[test]
    fn test_send_error_response() {
        let mut buf = Vec::new();
        send_error_response(&mut buf, "something failed");
        let mut cursor = std::io::Cursor::new(buf);
        let (ft, data) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, 0x04); // Error
        assert_eq!(data, b"something failed");
    }
}
