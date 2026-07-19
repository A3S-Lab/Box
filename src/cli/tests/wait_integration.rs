//! Integration coverage for terminal status retained after auto-removal.

mod support;
use support::CliTest;

fn write_removed_terminal_archive(cli: &CliTest, id: &str, name: &str, exit_code: Option<i32>) {
    let archive_dir = cli.home_path().join("removed-logs").join(id);
    std::fs::create_dir_all(&archive_dir).expect("create removed terminal archive");
    let metadata = serde_json::json!({
        "id": id,
        "short_id": id.replace('-', "").chars().take(12).collect::<String>(),
        "name": name,
        "image": "alpine:latest",
        "removed_at": "2026-07-18T00:00:00Z",
        "created_at": "2026-07-18T00:00:00Z",
        "started_at": "2026-07-18T00:00:01Z",
        "exit_code": exit_code,
        "log_config": {
            "driver": "none",
            "options": {}
        }
    });
    std::fs::write(
        archive_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&metadata).expect("serialize removed terminal archive"),
    )
    .expect("write removed terminal archive");
}

#[test]
fn wait_reads_auto_removed_terminal_archive() {
    let cli = CliTest::new();
    write_removed_terminal_archive(
        &cli,
        "550e8400-e29b-41d4-a716-446655440010",
        "failed-removed-job",
        Some(23),
    );
    write_removed_terminal_archive(
        &cli,
        "550e8400-e29b-41d4-a716-446655440011",
        "successful-removed-job",
        None,
    );

    let output = cli.ok(&[
        "wait",
        "failed-removed-job",
        "successful-removed-job",
        "--no-heartbeat",
    ]);
    assert_eq!(output, "23\n0\n");
}
