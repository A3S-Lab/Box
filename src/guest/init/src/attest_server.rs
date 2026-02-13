//! Guest TEE attestation server for AMD SEV-SNP.
//!
//! Listens on vsock port 4091 and handles HTTP POST /attest requests.
//! Obtains hardware-signed SNP attestation reports via `/dev/sev-guest`
//! and returns them with the certificate chain.

#[cfg(target_os = "linux")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::io::Write;

use tracing::info;
#[cfg(target_os = "linux")]
use tracing::warn;

/// Vsock port for the attestation server.
pub const ATTEST_VSOCK_PORT: u32 = 4091;

/// SNP attestation report size (AMD SEV-SNP ABI spec v1.52).
#[cfg(target_os = "linux")]
const SNP_REPORT_SIZE: usize = 1184;

/// Size of the report_data field in the SNP report request.
const SNP_USER_DATA_SIZE: usize = 64;

/// Run the attestation server, listening on vsock port 4091.
///
/// On Linux with SEV-SNP, handles attestation requests via `/dev/sev-guest`.
/// On non-Linux platforms, this is a no-op (development stub).
pub fn run_attest_server() -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting attestation server on vsock port {}", ATTEST_VSOCK_PORT);

    #[cfg(target_os = "linux")]
    {
        run_vsock_server()?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("Attestation server not available on non-Linux platform (development mode)");
    }

    Ok(())
}

/// Linux vsock server implementation.
#[cfg(target_os = "linux")]
fn run_vsock_server() -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::{
        accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
    };
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::Duration;
    use tracing::error;

    let sock_fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;

    let addr = VsockAddr::new(libc::VMADDR_CID_ANY, ATTEST_VSOCK_PORT);
    bind(sock_fd.as_raw_fd(), &addr)?;
    listen(&sock_fd, Backlog::new(4)?)?;

    info!("Attestation server listening on vsock port {}", ATTEST_VSOCK_PORT);

    loop {
        match accept(sock_fd.as_raw_fd()) {
            Ok(client_fd) => {
                let client = unsafe { OwnedFd::from_raw_fd(client_fd) };
                if let Err(e) = handle_connection(client) {
                    warn!("Failed to handle attestation connection: {}", e);
                }
            }
            Err(e) => {
                error!("Attestation accept failed: {}", e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

// ============================================================================
// Request / Response types (kept local to avoid cross-crate dependency)
// ============================================================================

/// Attestation request from the host.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct AttestRequest {
    /// Nonce from the verifier (included in report_data to prevent replay).
    nonce: Vec<u8>,
    /// Optional additional data to bind into the report.
    #[serde(default)]
    user_data: Option<Vec<u8>>,
}

/// Attestation response returned to the host.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize)]
struct AttestResponse {
    /// Raw SNP attestation report bytes.
    report: Vec<u8>,
    /// Certificate chain for verification.
    cert_chain: CertChain,
}

/// Certificate chain from the SNP extended report.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize, Default)]
struct CertChain {
    vcek: Vec<u8>,
    ask: Vec<u8>,
    ark: Vec<u8>,
}

// ============================================================================
// Connection handler
// ============================================================================

/// Handle a single attestation connection.
#[cfg(target_os = "linux")]
fn handle_connection(fd: std::os::fd::OwnedFd) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use tracing::debug;

    let raw_fd = fd.as_raw_fd();
    let mut stream = unsafe { std::fs::File::from_raw_fd(raw_fd) };

    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        std::mem::forget(fd);
        return Ok(());
    }

    let request_str = String::from_utf8_lossy(&buf[..n]);
    debug!("Attestation request received ({} bytes)", n);

    // Parse HTTP body
    let body = match request_str.find("\r\n\r\n") {
        Some(pos) => &request_str[pos + 4..],
        None => {
            send_error_response(&mut stream, 400, "Malformed HTTP request")?;
            std::mem::forget(fd);
            return Ok(());
        }
    };

    let req: AttestRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            send_error_response(&mut stream, 400, &format!("Invalid JSON: {}", e))?;
            std::mem::forget(fd);
            return Ok(());
        }
    };

    // Build 64-byte report_data from nonce + optional user_data
    let report_data = build_report_data(&req.nonce, req.user_data.as_deref());

    // Get SNP attestation report from hardware
    let response = match get_snp_report(&report_data) {
        Ok(resp) => resp,
        Err(e) => {
            warn!("SNP attestation failed: {}", e);
            send_error_response(&mut stream, 500, &format!("Attestation failed: {}", e))?;
            std::mem::forget(fd);
            return Ok(());
        }
    };

    let response_body = serde_json::to_string(&response)?;
    let http_response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.len(),
        response_body,
    );
    stream.write_all(http_response.as_bytes())?;

    std::mem::forget(fd);
    Ok(())
}

/// Send an HTTP error response.
#[cfg(target_os = "linux")]
fn send_error_response(
    stream: &mut impl Write,
    status: u16,
    message: &str,
) -> Result<(), std::io::Error> {
    let body = format!(r#"{{"error":"{}"}}"#, message);
    let response = format!(
        "HTTP/1.1 {} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body,
    );
    stream.write_all(response.as_bytes())
}

// ============================================================================
// SNP report_data construction
// ============================================================================

/// Build the 64-byte report_data field from nonce and optional user_data.
///
/// If only nonce is provided, it is zero-padded to 64 bytes.
/// If user_data is also provided, nonce and user_data are concatenated
/// and truncated/padded to 64 bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn build_report_data(nonce: &[u8], user_data: Option<&[u8]>) -> [u8; SNP_USER_DATA_SIZE] {
    let mut data = [0u8; SNP_USER_DATA_SIZE];
    match user_data {
        Some(ud) => {
            if nonce.len() >= SNP_USER_DATA_SIZE {
                data.copy_from_slice(&nonce[..SNP_USER_DATA_SIZE]);
            } else {
                data[..nonce.len()].copy_from_slice(nonce);
                let remaining = SNP_USER_DATA_SIZE - nonce.len();
                let ud_copy = ud.len().min(remaining);
                data[nonce.len()..nonce.len() + ud_copy].copy_from_slice(&ud[..ud_copy]);
            }
        }
        None => {
            let len = nonce.len().min(SNP_USER_DATA_SIZE);
            data[..len].copy_from_slice(&nonce[..len]);
        }
    }
    data
}

// ============================================================================
// SNP ioctl interface (/dev/sev-guest)
// ============================================================================

/// Get an SNP attestation report from the hardware via `/dev/sev-guest`.
///
/// Uses the `SNP_GET_EXT_REPORT` ioctl to obtain both the attestation
/// report and the certificate chain (VCEK, ASK, ARK) in a single call.
#[cfg(target_os = "linux")]
fn get_snp_report(report_data: &[u8; SNP_USER_DATA_SIZE]) -> Result<AttestResponse, String> {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;

    // Try /dev/sev-guest (standard) then /dev/sev (fallback)
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

// ioctl numbers for /dev/sev-guest
// #define SNP_GET_REPORT     _IOWR('S', 0x0, struct snp_guest_request_ioctl)
// #define SNP_GET_EXT_REPORT _IOWR('S', 0x2, struct snp_ext_report_req)
#[cfg(target_os = "linux")]
const SNP_GET_REPORT_IOCTL: libc::c_ulong = 0xc018_5300; // _IOWR('S', 0x0, 24)
#[cfg(target_os = "linux")]
const SNP_GET_EXT_REPORT_IOCTL: libc::c_ulong = 0xc018_5302; // _IOWR('S', 0x2, 24)

/// snp_report_req: request structure for SNP_GET_REPORT
#[cfg(target_os = "linux")]
#[repr(C)]
struct SnpReportReq {
    /// User data to include in the report (64 bytes).
    user_data: [u8; 64],
    /// VMPL level (0 for most privileged).
    vmpl: u32,
    /// Reserved, must be zero.
    rsvd: [u8; 28],
}

/// snp_report_resp: response structure containing the report
#[cfg(target_os = "linux")]
#[repr(C)]
struct SnpReportResp {
    /// Status code (0 = success).
    status: u32,
    /// Size of the report data.
    report_size: u32,
    /// Reserved.
    rsvd: [u8; 24],
    /// The attestation report.
    report: [u8; SNP_REPORT_SIZE],
}

/// snp_guest_request_ioctl: wrapper for the ioctl call
#[cfg(target_os = "linux")]
#[repr(C)]
struct SnpGuestRequestIoctl {
    /// Message version (must be 1).
    msg_version: u8,
    /// Request data pointer.
    req_data: u64,
    /// Response data pointer.
    resp_data: u64,
    /// Firmware error code (output).
    fw_err: u64,
}

/// snp_ext_report_req: request for extended report (report + certs)
#[cfg(target_os = "linux")]
#[repr(C)]
struct SnpExtReportReq {
    /// The standard report request.
    data: SnpReportReq,
    /// Pointer to certificate buffer.
    certs_address: u64,
    /// Length of certificate buffer.
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

/// Get SNP extended report (report + certificate chain) via SNP_GET_EXT_REPORT.
#[cfg(target_os = "linux")]
fn snp_get_ext_report(
    fd: libc::c_int,
    report_data: &[u8; SNP_USER_DATA_SIZE],
) -> Result<AttestResponse, String> {
    // Certificate buffer (up to 16 KiB for VCEK + ASK + ARK)
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

    // Parse certificate table from certs_buf
    let cert_chain = parse_cert_table(&certs_buf, ext_req.certs_len as usize);

    Ok(AttestResponse {
        report: resp.report.to_vec(),
        cert_chain,
    })
}

/// Parse the SNP certificate table returned by SNP_GET_EXT_REPORT.
///
/// The table is a sequence of `(guid, offset, length)` entries followed
/// by the certificate data. GUIDs identify VCEK, ASK, and ARK.
#[cfg(target_os = "linux")]
fn parse_cert_table(buf: &[u8], len: usize) -> CertChain {
    // AMD SEV-SNP certificate table GUIDs (little-endian UUID format)
    const VCEK_GUID: [u8; 16] = guid_bytes("63da758d-e664-4564-adc5-f4b93be8accd");
    const ASK_GUID: [u8; 16] = guid_bytes("4ab7b379-bbac-4fe4-a02f-05aef327c782");
    const ARK_GUID: [u8; 16] = guid_bytes("c0b406a4-a803-4952-9743-3fb6014cd0ae");

    let mut chain = CertChain::default();
    if len < 24 {
        return chain;
    }

    // Each entry: 16-byte GUID + 4-byte offset + 4-byte length = 24 bytes
    // Table ends with a zero GUID entry
    let mut pos = 0;
    while pos + 24 <= len {
        let guid = &buf[pos..pos + 16];
        if guid.iter().all(|&b| b == 0) {
            break; // End of table
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
/// The first three groups are byte-swapped per RFC 4122.
#[cfg(target_os = "linux")]
const fn guid_bytes(uuid: &str) -> [u8; 16] {
    let b = uuid.as_bytes();
    let mut out = [0u8; 16];

    // Parse hex chars
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

    // Group 1 (4 bytes, reversed): bytes 0-3
    out[0] = hex[6] << 4 | hex[7];
    out[1] = hex[4] << 4 | hex[5];
    out[2] = hex[2] << 4 | hex[3];
    out[3] = hex[0] << 4 | hex[1];
    // Group 2 (2 bytes, reversed): bytes 4-5
    out[4] = hex[10] << 4 | hex[11];
    out[5] = hex[8] << 4 | hex[9];
    // Group 3 (2 bytes, reversed): bytes 6-7
    out[6] = hex[14] << 4 | hex[15];
    out[7] = hex[12] << 4 | hex[13];
    // Groups 4-5 (8 bytes, not reversed): bytes 8-15
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_attest_vsock_port_constant() {
        assert_eq!(ATTEST_VSOCK_PORT, 4091);
    }

    #[test]
    fn test_build_report_data_nonce_only() {
        let nonce = vec![1, 2, 3, 4];
        let data = build_report_data(&nonce, None);
        assert_eq!(&data[..4], &[1, 2, 3, 4]);
        assert_eq!(&data[4..], &[0u8; 60]);
    }

    #[test]
    fn test_build_report_data_with_user_data() {
        let nonce = vec![1, 2, 3, 4];
        let user_data = vec![5, 6, 7, 8];
        let data = build_report_data(&nonce, Some(&user_data));
        assert_eq!(&data[..4], &[1, 2, 3, 4]);
        assert_eq!(&data[4..8], &[5, 6, 7, 8]);
        assert_eq!(&data[8..], &[0u8; 56]);
    }

    #[test]
    fn test_build_report_data_overflow() {
        let nonce = vec![0xAA; 64];
        let data = build_report_data(&nonce, Some(&[0xBB; 10]));
        // Nonce fills all 64 bytes, user_data is ignored
        assert_eq!(data, [0xAA; 64]);
    }

    #[test]
    fn test_build_report_data_exact_64() {
        let nonce = vec![0xFF; 64];
        let data = build_report_data(&nonce, None);
        assert_eq!(data, [0xFF; 64]);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_guid_bytes() {
        // VCEK GUID: 63da758d-e664-4564-adc5-f4b93be8accd
        let guid = guid_bytes("63da758d-e664-4564-adc5-f4b93be8accd");
        // First group reversed: 63da758d -> 8d75da63
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
        assert_eq!(hex_val(b'A'), 10);
        assert_eq!(hex_val(b'F'), 15);
    }
}
