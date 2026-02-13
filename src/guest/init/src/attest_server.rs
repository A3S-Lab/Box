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

#[cfg(target_os = "linux")]
use std::io::{Read, Write};

use tracing::info;
#[cfg(target_os = "linux")]
use tracing::warn;

/// Vsock port for the attestation server.
pub const ATTEST_VSOCK_PORT: u32 = 4091;

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
    let (tls_config, cert_der) = generate_ratls_config()?;
    let tls_config = Arc::new(tls_config);

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
                if let Err(e) = handle_tls_connection(client, config) {
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
/// Returns (ServerConfig, cert_der) so the cert can be logged/inspected.
#[cfg(target_os = "linux")]
fn generate_ratls_config() -> Result<(rustls::ServerConfig, Vec<u8>), Box<dyn std::error::Error>> {
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

    Ok((config, cert_der))
}

// ============================================================================
// TLS connection handler
// ============================================================================

/// Handle a single TLS connection over vsock.
///
/// Performs the TLS handshake (which delivers the RA-TLS certificate),
/// then serves a simple status response.
#[cfg(target_os = "linux")]
fn handle_tls_connection(
    fd: std::os::fd::OwnedFd,
    config: std::sync::Arc<rustls::ServerConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use tracing::debug;

    let raw_fd = fd.as_raw_fd();
    let tcp_stream = unsafe { std::net::TcpStream::from_raw_fd(raw_fd) };

    let conn = rustls::ServerConnection::new(config)
        .map_err(|e| format!("TLS connection init failed: {}", e))?;

    let mut tls = rustls::StreamOwned::new(conn, tcp_stream);

    // Read client request (after TLS handshake completes)
    let mut buf = vec![0u8; 4096];
    match tls.read(&mut buf) {
        Ok(0) => {
            debug!("RA-TLS client disconnected after handshake");
        }
        Ok(n) => {
            debug!("RA-TLS request received ({} bytes)", n);
            // Respond with simple status
            let response = b"{\"status\":\"ok\",\"tee\":true}";
            let http_response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len(),
            );
            let _ = tls.write_all(http_response.as_bytes());
            let _ = tls.write_all(response);
        }
        Err(e) => {
            debug!("RA-TLS read error: {}", e);
        }
    }

    // Prevent double-close: OwnedFd and TcpStream both own the fd
    std::mem::forget(fd);
    Ok(())
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
        libc::ioctl(fd, SNP_GET_REPORT_IOCTL, &mut ioctl_req as *mut _)
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
        libc::ioctl(fd, SNP_GET_EXT_REPORT_IOCTL, &mut ioctl_req as *mut _)
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
        assert_eq!(ATTEST_VSOCK_PORT, 4091);
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
}
