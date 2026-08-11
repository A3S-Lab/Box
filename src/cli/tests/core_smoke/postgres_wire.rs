//! Minimal PostgreSQL v3/SCRAM-SHA-256 client for the real HVF regression.
//!
//! Keeping the protocol client here avoids making the physical-host gate depend
//! on Homebrew `psql` or another host package. It intentionally implements only
//! the authentication and one-text-column query shape used by the test.

use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const SSL_REQUEST_CODE: u32 = 80_877_103;
const PROTOCOL_VERSION_3: u32 = 196_608;

pub(crate) fn query_text(
    address: SocketAddr,
    user: &str,
    password: &str,
    database: &str,
    nonce: &str,
) -> Result<String, String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .map_err(|error| format!("connect to PostgreSQL at {address}: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("set PostgreSQL read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("set PostgreSQL write timeout: {error}"))?;
    stream
        .set_nodelay(true)
        .map_err(|error| format!("set PostgreSQL TCP_NODELAY: {error}"))?;

    write_all(&mut stream, &8_u32.to_be_bytes(), "SSLRequest length")?;
    write_all(
        &mut stream,
        &SSL_REQUEST_CODE.to_be_bytes(),
        "SSLRequest code",
    )?;
    let mut ssl_response = [0_u8; 1];
    read_exact(&mut stream, &mut ssl_response, "SSLRequest response")?;
    if ssl_response != *b"N" {
        return Err(format!(
            "PostgreSQL test requires the image's default plaintext endpoint; SSLRequest returned {:?}",
            char::from(ssl_response[0])
        ));
    }

    let mut startup = PROTOCOL_VERSION_3.to_be_bytes().to_vec();
    for (key, value) in [
        ("user", user),
        ("database", database),
        ("application_name", "a3s-box-hvf-regression"),
        ("client_encoding", "UTF8"),
    ] {
        startup.extend_from_slice(key.as_bytes());
        startup.push(0);
        startup.extend_from_slice(value.as_bytes());
        startup.push(0);
    }
    startup.push(0);
    write_startup(&mut stream, &startup)?;

    authenticate(&mut stream, user, password, nonce)?;

    let query = "SELECT 'a3s-box-hvf-' || current_database()";
    let mut query_payload = query.as_bytes().to_vec();
    query_payload.push(0);
    write_message(&mut stream, b'Q', &query_payload)?;

    let mut row = None;
    loop {
        let (tag, payload) = read_message(&mut stream)?;
        match tag {
            b'D' => row = Some(parse_single_text_column(&payload)?),
            b'E' => {
                return Err(format!(
                    "PostgreSQL query failed: {}",
                    error_fields(&payload)
                ))
            }
            b'Z' => break,
            _ => {}
        }
    }

    let _ = write_message(&mut stream, b'X', &[]);
    row.ok_or_else(|| "PostgreSQL query returned no DataRow".to_string())
}

fn authenticate(
    stream: &mut TcpStream,
    user: &str,
    password: &str,
    nonce: &str,
) -> Result<(), String> {
    let escaped_user = user.replace('=', "=3D").replace(',', "=2C");
    let client_first_bare = format!("n={escaped_user},r={nonce}");
    let client_first = format!("n,,{client_first_bare}");
    let mut expected_server_signature = None;
    let mut authenticated = false;

    loop {
        let (tag, payload) = read_message(stream)?;
        match tag {
            b'R' => {
                let (code_bytes, auth_payload) = payload
                    .split_first_chunk::<4>()
                    .ok_or_else(|| "truncated PostgreSQL authentication message".to_string())?;
                let code = u32::from_be_bytes(*code_bytes);
                match code {
                    0 => authenticated = true,
                    10 => {
                        let mechanisms = auth_payload
                            .split(|byte| *byte == 0)
                            .filter(|mechanism| !mechanism.is_empty())
                            .collect::<Vec<_>>();
                        if !mechanisms
                            .iter()
                            .any(|mechanism| *mechanism == b"SCRAM-SHA-256")
                        {
                            return Err("PostgreSQL did not offer SCRAM-SHA-256".to_string());
                        }

                        let mut response = b"SCRAM-SHA-256\0".to_vec();
                        response.extend_from_slice(
                            &u32::try_from(client_first.len())
                                .map_err(|_| "SCRAM initial response is too large".to_string())?
                                .to_be_bytes(),
                        );
                        response.extend_from_slice(client_first.as_bytes());
                        write_message(stream, b'p', &response)?;
                    }
                    11 => {
                        let server_first = std::str::from_utf8(auth_payload).map_err(|error| {
                            format!("invalid SCRAM server-first UTF-8: {error}")
                        })?;
                        let (client_final, server_signature) =
                            scram_client_final(password, nonce, &client_first_bare, server_first)?;
                        expected_server_signature = Some(server_signature);
                        write_message(stream, b'p', client_final.as_bytes())?;
                    }
                    12 => {
                        let server_final = std::str::from_utf8(auth_payload).map_err(|error| {
                            format!("invalid SCRAM server-final UTF-8: {error}")
                        })?;
                        verify_server_final(
                            server_final,
                            expected_server_signature.as_deref().ok_or_else(|| {
                                "SCRAM server-final arrived before server-first".to_string()
                            })?,
                        )?;
                    }
                    other => {
                        return Err(format!(
                            "unsupported PostgreSQL authentication method {other}"
                        ));
                    }
                }
            }
            b'E' => {
                return Err(format!(
                    "PostgreSQL authentication failed: {}",
                    error_fields(&payload)
                ));
            }
            b'Z' if authenticated => return Ok(()),
            b'Z' => return Err("PostgreSQL became ready before AuthenticationOk".to_string()),
            _ => {}
        }
    }
}

fn scram_client_final(
    password: &str,
    client_nonce: &str,
    client_first_bare: &str,
    server_first: &str,
) -> Result<(String, Vec<u8>), String> {
    let server_nonce = scram_attribute(server_first, 'r')?;
    if !server_nonce.starts_with(client_nonce) || server_nonce.len() == client_nonce.len() {
        return Err("SCRAM server nonce does not extend the client nonce".to_string());
    }
    let salt = base64::engine::general_purpose::STANDARD
        .decode(scram_attribute(server_first, 's')?)
        .map_err(|error| format!("invalid SCRAM salt: {error}"))?;
    let iterations = scram_attribute(server_first, 'i')?
        .parse::<u32>()
        .map_err(|error| format!("invalid SCRAM iteration count: {error}"))?;
    if !(1..=1_000_000).contains(&iterations) {
        return Err(format!(
            "SCRAM iteration count {iterations} is outside the test safety bound"
        ));
    }

    let salted_password = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
    let client_key = hmac_sha256(&salted_password, b"Client Key");
    let stored_key = Sha256::digest(client_key);
    let client_final_without_proof = format!("c=biws,r={server_nonce}");
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");
    let client_signature = hmac_sha256(&stored_key, auth_message.as_bytes());
    let mut client_proof = [0_u8; 32];
    for (output, (key, signature)) in client_proof
        .iter_mut()
        .zip(client_key.iter().zip(client_signature.iter()))
    {
        *output = key ^ signature;
    }

    let server_key = hmac_sha256(&salted_password, b"Server Key");
    let server_signature = hmac_sha256(&server_key, auth_message.as_bytes()).to_vec();
    let proof = base64::engine::general_purpose::STANDARD.encode(client_proof);
    Ok((
        format!("{client_final_without_proof},p={proof}"),
        server_signature,
    ))
}

fn verify_server_final(server_final: &str, expected_signature: &[u8]) -> Result<(), String> {
    if let Ok(error) = scram_attribute(server_final, 'e') {
        return Err(format!("SCRAM server rejected authentication: {error}"));
    }
    let actual = base64::engine::general_purpose::STANDARD
        .decode(scram_attribute(server_final, 'v')?)
        .map_err(|error| format!("invalid SCRAM server signature: {error}"))?;
    if !constant_time_eq(&actual, expected_signature) {
        return Err("SCRAM server signature mismatch".to_string());
    }
    Ok(())
}

fn scram_attribute(input: &str, wanted: char) -> Result<&str, String> {
    let mut found = None;
    for attribute in input.split(',') {
        let Some((name, value)) = attribute.split_once('=') else {
            return Err(format!("malformed SCRAM attribute {attribute:?}"));
        };
        if name.len() == 1 && name.as_bytes()[0] == wanted as u8 && found.replace(value).is_some() {
            return Err(format!("duplicate SCRAM attribute {wanted}"));
        }
    }
    found.ok_or_else(|| format!("missing SCRAM attribute {wanted}"))
}

fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut first_input = Vec::with_capacity(salt.len() + 4);
    first_input.extend_from_slice(salt);
    first_input.extend_from_slice(&1_u32.to_be_bytes());
    let mut previous = hmac_sha256(password, &first_input);
    let mut output = previous;
    for _ in 1..iterations {
        previous = hmac_sha256(password, &previous);
        for (accumulator, byte) in output.iter_mut().zip(previous) {
            *accumulator ^= byte;
        }
    }
    output
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_BYTES: usize = 64;
    let mut normalized_key = [0_u8; BLOCK_BYTES];
    if key.len() > BLOCK_BYTES {
        normalized_key[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized_key[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36_u8; BLOCK_BYTES];
    let mut outer_pad = [0x5c_u8; BLOCK_BYTES];
    for index in 0..BLOCK_BYTES {
        inner_pad[index] ^= normalized_key[index];
        outer_pad[index] ^= normalized_key[index];
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn write_startup(stream: &mut TcpStream, payload: &[u8]) -> Result<(), String> {
    let length = u32::try_from(payload.len() + 4)
        .map_err(|_| "PostgreSQL startup message is too large".to_string())?;
    write_all(stream, &length.to_be_bytes(), "startup length")?;
    write_all(stream, payload, "startup payload")
}

fn write_message(stream: &mut TcpStream, tag: u8, payload: &[u8]) -> Result<(), String> {
    let length = u32::try_from(payload.len() + 4)
        .map_err(|_| "PostgreSQL message is too large".to_string())?;
    write_all(stream, &[tag], "message tag")?;
    write_all(stream, &length.to_be_bytes(), "message length")?;
    write_all(stream, payload, "message payload")?;
    stream
        .flush()
        .map_err(|error| format!("flush PostgreSQL message: {error}"))
}

fn read_message(stream: &mut TcpStream) -> Result<(u8, Vec<u8>), String> {
    let mut tag = [0_u8; 1];
    read_exact(stream, &mut tag, "message tag")?;
    let mut length = [0_u8; 4];
    read_exact(stream, &mut length, "message length")?;
    let length = u32::from_be_bytes(length) as usize;
    if !(4..=MAX_MESSAGE_BYTES).contains(&length) {
        return Err(format!("invalid PostgreSQL message length {length}"));
    }
    let mut payload = vec![0_u8; length - 4];
    read_exact(stream, &mut payload, "message payload")?;
    Ok((tag[0], payload))
}

fn write_all(stream: &mut TcpStream, bytes: &[u8], label: &str) -> Result<(), String> {
    stream
        .write_all(bytes)
        .map_err(|error| format!("write PostgreSQL {label}: {error}"))
}

fn read_exact(stream: &mut TcpStream, bytes: &mut [u8], label: &str) -> Result<(), String> {
    stream
        .read_exact(bytes)
        .map_err(|error| format!("read PostgreSQL {label}: {error}"))
}

fn parse_single_text_column(payload: &[u8]) -> Result<String, String> {
    if payload.len() < 6 {
        return Err("truncated PostgreSQL DataRow".to_string());
    }
    let columns = u16::from_be_bytes([payload[0], payload[1]]);
    if columns != 1 {
        return Err(format!(
            "expected one PostgreSQL DataRow column, got {columns}"
        ));
    }
    let length = i32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]);
    if length < 0 {
        return Err("PostgreSQL query unexpectedly returned NULL".to_string());
    }
    let length = usize::try_from(length).map_err(|_| "negative DataRow length".to_string())?;
    let value = payload
        .get(6..6 + length)
        .ok_or_else(|| "truncated PostgreSQL DataRow value".to_string())?;
    if payload.len() != 6 + length {
        return Err("PostgreSQL DataRow has trailing bytes".to_string());
    }
    std::str::from_utf8(value)
        .map(str::to_string)
        .map_err(|error| format!("PostgreSQL DataRow is not UTF-8: {error}"))
}

fn error_fields(payload: &[u8]) -> String {
    let mut fields = Vec::new();
    for field in payload.split(|byte| *byte == 0) {
        if field.len() > 1 {
            fields.push(String::from_utf8_lossy(&field[1..]).into_owned());
        }
    }
    if fields.is_empty() {
        "unstructured server error".to_string()
    } else {
        fields.join(": ")
    }
}

#[cfg(test)]
#[path = "postgres_wire_tests.rs"]
mod tests;
