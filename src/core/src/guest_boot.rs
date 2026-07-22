//! Host/guest bootstrap readiness contract.

/// Environment variable carrying the per-boot readiness token into guest-init.
pub const GUEST_READY_TOKEN_ENV: &str = "A3S_GUEST_READY_TOKEN";

/// Guest-visible readiness marker written after filesystem and network setup.
pub const GUEST_READY_PATH: &str = "/.a3s_guest_ready";

/// Maximum accepted readiness-token length.
pub const MAX_GUEST_READY_TOKEN_BYTES: usize = 128;

/// Validate a host-generated readiness token before it is persisted or compared.
pub fn validate_guest_ready_token(token: &str) -> Result<(), &'static str> {
    if token.is_empty() {
        return Err("guest readiness token is empty");
    }
    if token.len() > MAX_GUEST_READY_TOKEN_BYTES {
        return Err("guest readiness token exceeds the size limit");
    }
    if !token
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err("guest readiness token contains unsupported characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_tokens_are_bounded_and_path_safe() {
        assert!(validate_guest_ready_token("0123456789abcdef").is_ok());
        assert!(validate_guest_ready_token("boot-token_1").is_ok());
        assert!(validate_guest_ready_token("").is_err());
        assert!(validate_guest_ready_token("bad/token").is_err());
        assert!(validate_guest_ready_token(&"x".repeat(MAX_GUEST_READY_TOKEN_BYTES + 1)).is_err());
    }
}
