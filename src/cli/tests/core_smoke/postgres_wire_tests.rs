use super::*;
use std::net::TcpListener;
use std::thread;

#[test]
fn hmac_and_pbkdf2_match_published_vectors() {
    assert_eq!(
        hex::encode(hmac_sha256(&[0x0b; 20], b"Hi There")),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    assert_eq!(
        hex::encode(pbkdf2_sha256(b"password", b"salt", 1)),
        "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
    );
    assert_eq!(
        hex::encode(pbkdf2_sha256(b"password", b"salt", 4096)),
        "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
    );
}

#[test]
fn client_completes_scram_and_reads_one_text_row() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || serve_test_connection(listener));

    let row = query_text(
        address,
        "a3s_cloud",
        "a3s_cloud",
        "a3s_cloud",
        "a3sbox-client-nonce",
    )
    .unwrap();
    assert_eq!(row, "a3s-box-hvf-a3s_cloud");
    server.join().unwrap();
}

fn serve_test_connection(listener: TcpListener) {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let mut ssl_request = [0_u8; 8];
    stream.read_exact(&mut ssl_request).unwrap();
    assert_eq!(&ssl_request[..4], &8_u32.to_be_bytes());
    assert_eq!(&ssl_request[4..], &SSL_REQUEST_CODE.to_be_bytes());
    stream.write_all(b"N").unwrap();

    let startup = read_startup_for_test(&mut stream);
    assert!(startup
        .windows(b"user\0a3s_cloud\0".len())
        .any(|window| { window == b"user\0a3s_cloud\0" }));

    let mut sasl = 10_u32.to_be_bytes().to_vec();
    sasl.extend_from_slice(b"SCRAM-SHA-256\0\0");
    write_server_message(&mut stream, b'R', &sasl);

    let (tag, initial) = read_message(&mut stream).unwrap();
    assert_eq!(tag, b'p');
    let mechanism_end = initial.iter().position(|byte| *byte == 0).unwrap();
    assert_eq!(&initial[..mechanism_end], b"SCRAM-SHA-256");
    let response_length_offset = mechanism_end + 1;
    let response_length = u32::from_be_bytes(
        initial[response_length_offset..response_length_offset + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let client_first = std::str::from_utf8(
        &initial[response_length_offset + 4..response_length_offset + 4 + response_length],
    )
    .unwrap();
    let client_first_bare = client_first.strip_prefix("n,,").unwrap();
    let client_nonce = scram_attribute(client_first_bare, 'r').unwrap();
    let salt = base64::engine::general_purpose::STANDARD.encode(b"fixed-test-salt");
    let server_first = format!("r={client_nonce}-server,s={salt},i=4096");
    let mut continuation = 11_u32.to_be_bytes().to_vec();
    continuation.extend_from_slice(server_first.as_bytes());
    write_server_message(&mut stream, b'R', &continuation);

    let (tag, client_final) = read_message(&mut stream).unwrap();
    assert_eq!(tag, b'p');
    let (expected_final, server_signature) =
        scram_client_final("a3s_cloud", client_nonce, client_first_bare, &server_first).unwrap();
    assert_eq!(client_final, expected_final.as_bytes());

    let mut final_message = 12_u32.to_be_bytes().to_vec();
    final_message.extend_from_slice(b"v=");
    final_message.extend_from_slice(
        base64::engine::general_purpose::STANDARD
            .encode(server_signature)
            .as_bytes(),
    );
    write_server_message(&mut stream, b'R', &final_message);
    write_server_message(&mut stream, b'R', &0_u32.to_be_bytes());
    write_server_message(&mut stream, b'Z', b"I");

    let (tag, query) = read_message(&mut stream).unwrap();
    assert_eq!(tag, b'Q');
    assert_eq!(
        query.strip_suffix(&[0]).unwrap(),
        b"SELECT 'a3s-box-hvf-' || current_database()"
    );
    let value = b"a3s-box-hvf-a3s_cloud";
    let mut data_row = 1_u16.to_be_bytes().to_vec();
    data_row.extend_from_slice(&(value.len() as u32).to_be_bytes());
    data_row.extend_from_slice(value);
    write_server_message(&mut stream, b'D', &data_row);
    write_server_message(&mut stream, b'C', b"SELECT 1\0");
    write_server_message(&mut stream, b'Z', b"I");

    let (tag, payload) = read_message(&mut stream).unwrap();
    assert_eq!(tag, b'X');
    assert!(payload.is_empty());
}

fn read_startup_for_test(stream: &mut TcpStream) -> Vec<u8> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).unwrap();
    let length = u32::from_be_bytes(length) as usize;
    let mut payload = vec![0_u8; length - 4];
    stream.read_exact(&mut payload).unwrap();
    assert_eq!(&payload[..4], &PROTOCOL_VERSION_3.to_be_bytes());
    payload
}

fn write_server_message(stream: &mut TcpStream, tag: u8, payload: &[u8]) {
    stream.write_all(&[tag]).unwrap();
    stream
        .write_all(&((payload.len() + 4) as u32).to_be_bytes())
        .unwrap();
    stream.write_all(payload).unwrap();
    stream.flush().unwrap();
}
