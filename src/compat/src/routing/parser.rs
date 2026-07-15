use std::fmt;
use std::num::NonZeroU16;
use std::str::FromStr;

use axum::http::header::HOST;
use axum::http::uri::Authority;
use axum::http::{HeaderMap, HeaderName};
use thiserror::Error;

use crate::control::SandboxId;

pub const SANDBOX_ID_HEADER: &str = "e2b-sandbox-id";
pub const SANDBOX_PORT_HEADER: &str = "e2b-sandbox-port";

#[derive(Clone, PartialEq, Eq)]
pub struct SandboxDomain(String);

impl SandboxDomain {
    pub fn new(value: impl Into<String>) -> RouteParseResult<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 253
            || value.ends_with('.')
            || value.bytes().any(|byte| byte.is_ascii_uppercase())
            || !value.split('.').all(valid_dns_label)
        {
            return Err(RouteParseError::InvalidDomain);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn shared_hostname(&self) -> String {
        format!("sandbox.{}", self.0)
    }
}

impl fmt::Debug for SandboxDomain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SandboxDomain")
            .field(&self.0)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteForm {
    Direct,
    SharedHeaders,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSandboxRoute {
    pub sandbox_id: SandboxId,
    pub port: NonZeroU16,
    pub form: RouteForm,
}

#[derive(Debug, Clone)]
pub struct SandboxRouteParser {
    domain: SandboxDomain,
}

impl SandboxRouteParser {
    pub fn new(domain: SandboxDomain) -> Self {
        Self { domain }
    }

    pub fn parse(&self, headers: &HeaderMap) -> RouteParseResult<ParsedSandboxRoute> {
        let host = single_header(headers, &HOST)?.ok_or(RouteParseError::MissingHost)?;
        self.parse_host(host, headers)
    }

    pub fn parse_host(
        &self,
        authority: &str,
        headers: &HeaderMap,
    ) -> RouteParseResult<ParsedSandboxRoute> {
        let authority = Authority::from_str(authority).map_err(|_| RouteParseError::InvalidHost)?;
        let host = authority.host().to_ascii_lowercase();
        let header_route = route_headers(headers)?;

        if host == self.domain.shared_hostname() {
            let (sandbox_id, port) = header_route.ok_or(RouteParseError::MissingRouteHeaders)?;
            return Ok(ParsedSandboxRoute {
                sandbox_id,
                port,
                form: RouteForm::SharedHeaders,
            });
        }

        let suffix = format!(".{}", self.domain.as_str());
        let label = host
            .strip_suffix(&suffix)
            .filter(|label| !label.is_empty() && !label.contains('.'))
            .ok_or(RouteParseError::UnsupportedHost)?;
        let (port, sandbox_id) = parse_direct_label(label)?;
        if let Some((header_id, header_port)) = header_route {
            if header_id != sandbox_id || header_port != port {
                return Err(RouteParseError::ConflictingRouteHeaders);
            }
        }
        Ok(ParsedSandboxRoute {
            sandbox_id,
            port,
            form: RouteForm::Direct,
        })
    }
}

fn route_headers(headers: &HeaderMap) -> RouteParseResult<Option<(SandboxId, NonZeroU16)>> {
    let sandbox_id = single_named_header(headers, SANDBOX_ID_HEADER)?;
    let port = single_named_header(headers, SANDBOX_PORT_HEADER)?;
    match (sandbox_id, port) {
        (None, None) => Ok(None),
        (Some(sandbox_id), Some(port)) => Ok(Some((
            SandboxId::new(sandbox_id).map_err(|_| RouteParseError::InvalidRouteHeaders)?,
            parse_port(port).map_err(|_| RouteParseError::InvalidRouteHeaders)?,
        ))),
        _ => Err(RouteParseError::MissingRouteHeaders),
    }
}

fn parse_direct_label(label: &str) -> RouteParseResult<(NonZeroU16, SandboxId)> {
    let (port, sandbox_id) = label.split_once('-').ok_or(RouteParseError::InvalidHost)?;
    Ok((
        parse_port(port).map_err(|_| RouteParseError::InvalidHost)?,
        SandboxId::new(sandbox_id).map_err(|_| RouteParseError::InvalidHost)?,
    ))
}

fn parse_port(value: &str) -> Result<NonZeroU16, ()> {
    if value.starts_with('0') {
        return Err(());
    }
    let port = value.parse::<u16>().map_err(|_| ())?;
    NonZeroU16::new(port).ok_or(())
}

fn single_named_header<'a>(
    headers: &'a HeaderMap,
    name: &'static str,
) -> RouteParseResult<Option<&'a str>> {
    single_header(headers, &HeaderName::from_static(name))
}

fn single_header<'a>(
    headers: &'a HeaderMap,
    name: &HeaderName,
) -> RouteParseResult<Option<&'a str>> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(RouteParseError::DuplicateHeader);
    }
    let value = value
        .to_str()
        .map_err(|_| RouteParseError::InvalidRouteHeaders)?;
    if value.is_empty() {
        return Err(RouteParseError::InvalidRouteHeaders);
    }
    Ok(Some(value))
}

fn valid_dns_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && (bytes[bytes.len() - 1].is_ascii_lowercase() || bytes[bytes.len() - 1].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RouteParseError {
    #[error("sandbox domain is invalid")]
    InvalidDomain,
    #[error("Host header is missing")]
    MissingHost,
    #[error("Host header is invalid")]
    InvalidHost,
    #[error("host is outside the configured sandbox domain")]
    UnsupportedHost,
    #[error("sandbox route headers are incomplete")]
    MissingRouteHeaders,
    #[error("sandbox route headers are invalid")]
    InvalidRouteHeaders,
    #[error("sandbox route headers conflict with the direct hostname")]
    ConflictingRouteHeaders,
    #[error("routing header is duplicated")]
    DuplicateHeader,
}

pub type RouteParseResult<T> = std::result::Result<T, RouteParseError>;

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, Request};

    use super::*;

    fn parser() -> SandboxRouteParser {
        SandboxRouteParser::new(SandboxDomain::new("box.example.com").unwrap())
    }

    fn headers(host: &str) -> HeaderMap {
        Request::builder()
            .header(HOST, host)
            .body(())
            .unwrap()
            .into_parts()
            .0
            .headers
    }

    #[test]
    fn parses_direct_and_shared_routes_without_string_split_ambiguity() {
        let direct = parser()
            .parse(&headers("49983-sandbox-abc.box.example.com:443"))
            .unwrap();
        assert_eq!(direct.sandbox_id.as_str(), "sandbox-abc");
        assert_eq!(direct.port.get(), 49_983);
        assert_eq!(direct.form, RouteForm::Direct);

        let mut shared = headers("sandbox.box.example.com");
        shared.insert(SANDBOX_ID_HEADER, HeaderValue::from_static("sandbox-abc"));
        shared.insert(SANDBOX_PORT_HEADER, HeaderValue::from_static("49999"));
        let shared = parser().parse(&shared).unwrap();
        assert_eq!(shared.sandbox_id.as_str(), "sandbox-abc");
        assert_eq!(shared.port.get(), 49_999);
        assert_eq!(shared.form, RouteForm::SharedHeaders);
    }

    #[test]
    fn direct_route_headers_must_match_the_hostname() {
        let mut matching = headers("49983-sandbox-abc.box.example.com");
        matching.insert(SANDBOX_ID_HEADER, HeaderValue::from_static("sandbox-abc"));
        matching.insert(SANDBOX_PORT_HEADER, HeaderValue::from_static("49983"));
        assert!(parser().parse(&matching).is_ok());

        matching.insert(SANDBOX_PORT_HEADER, HeaderValue::from_static("49999"));
        assert_eq!(
            parser().parse(&matching).unwrap_err(),
            RouteParseError::ConflictingRouteHeaders
        );
    }

    #[test]
    fn rejects_domain_confusion_invalid_ports_and_hostile_identities() {
        for host in [
            "49983-sandbox-abc.box.example.com.evil.invalid",
            "49983-sandbox-abc.evil.box.example.com",
            "0-sandbox-abc.box.example.com",
            "65536-sandbox-abc.box.example.com",
            "049983-sandbox-abc.box.example.com",
            "49983-../box.example.com",
            "49983--leading.box.example.com",
        ] {
            assert!(parser().parse(&headers(host)).is_err(), "accepted {host}");
        }
    }

    #[test]
    fn shared_routes_require_one_complete_header_pair() {
        let mut missing = headers("sandbox.box.example.com");
        missing.insert(SANDBOX_ID_HEADER, HeaderValue::from_static("sandbox-abc"));
        assert_eq!(
            parser().parse(&missing).unwrap_err(),
            RouteParseError::MissingRouteHeaders
        );

        let mut duplicated = headers("sandbox.box.example.com");
        duplicated.append(SANDBOX_ID_HEADER, HeaderValue::from_static("sandbox-abc"));
        duplicated.append(SANDBOX_ID_HEADER, HeaderValue::from_static("sandbox-other"));
        duplicated.insert(SANDBOX_PORT_HEADER, HeaderValue::from_static("49983"));
        assert_eq!(
            parser().parse(&duplicated).unwrap_err(),
            RouteParseError::DuplicateHeader
        );
    }

    #[test]
    fn validates_canonical_acl_domains() {
        assert_eq!(
            SandboxDomain::new("Box.Example.com").unwrap_err(),
            RouteParseError::InvalidDomain
        );
        for valid in ["localhost", "box.example", "sandbox-1.box.example"] {
            assert!(SandboxDomain::new(valid).is_ok(), "rejected {valid}");
        }
    }
}
