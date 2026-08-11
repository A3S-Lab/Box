use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn unused_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn spawn_server(address: SocketAddr, state: &Path) -> Server {
    Server(
        Command::new(env!("CARGO_BIN_EXE_a3s-box"))
            .args([
                "scale-api",
                "--address",
                &address.to_string(),
                "--state",
                state.to_str().unwrap(),
                "--max-instances",
                "10",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn wait_ready(address: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(address).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("scale-api did not become ready at {address}");
}

fn request_json() -> &'static str {
    r#"{"schema_version":1,"operation_id":"lost-response-v1","service":"api","expected_revision":"0","direction":"Up","current_replicas":0,"desired_replicas":2,"reason":"process recovery test"}"#
}

fn send_post(address: SocketAddr) -> TcpStream {
    let body = request_json();
    let request = format!(
        "POST /v1/scale/api HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(address).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream
}

fn post(address: SocketAddr) -> String {
    let mut stream = send_post(address);
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn wait_revision(state: &Path, revision: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(state) {
            let journal: serde_json::Value = serde_json::from_str(&contents).unwrap();
            if journal["revisions"]["api"]
                .as_u64()
                .map(|value| value.to_string())
                == Some(revision.to_string())
            {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("revision {revision} was not durably recorded");
}

#[test]
fn lost_response_is_exactly_replayable_after_process_restart() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("scale-authority.json");
    let address = unused_address();
    let mut first = spawn_server(address, &state);
    wait_ready(address);

    let abandoned_response = send_post(address);
    wait_revision(&state, "1");
    drop(abandoned_response);
    first.0.kill().unwrap();
    first.0.wait().unwrap();

    let restarted = spawn_server(address, &state);
    wait_ready(address);
    let replay = post(address);
    assert!(replay.starts_with("HTTP/1.1 200 OK"), "{replay}");
    assert!(replay.contains(r#""actual_replicas":2"#), "{replay}");
    assert!(replay.contains(r#""revision":"1""#), "{replay}");
    drop(restarted);
}
