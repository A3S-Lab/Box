CREATE TABLE snapshot_records (
    snapshot_id TEXT PRIMARY KEY NOT NULL,
    record_json TEXT NOT NULL CHECK (json_valid(record_json)),
    content_id TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.content_id')
    ) STORED NOT NULL,
    owner_id TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.owner_id')
    ) STORED NOT NULL,
    source_sandbox_id TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.source_sandbox_id')
    ) STORED NOT NULL,
    source_execution_id TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.source_execution_id')
    ) STORED NOT NULL,
    source_execution_generation INTEGER GENERATED ALWAYS AS (
        json_extract(record_json, '$.source_execution_generation')
    ) STORED NOT NULL,
    source_state TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.source_state')
    ) STORED NOT NULL CHECK (source_state IN ('running', 'paused')),
    reference TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.reference')
    ) STORED NOT NULL,
    state TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.state')
    ) STORED NOT NULL CHECK (state IN ('creating', 'active', 'deleting')),
    created_at TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.created_at')
    ) STORED NOT NULL,
    CHECK (snapshot_id = json_extract(record_json, '$.snapshot_id')),
    CHECK (length(trim(content_id)) > 0),
    CHECK (length(trim(owner_id)) > 0),
    CHECK (length(trim(source_sandbox_id)) > 0),
    CHECK (length(trim(source_execution_id)) > 0),
    CHECK (source_execution_generation > 0),
    CHECK (length(trim(reference)) > 0),
    CHECK (length(trim(created_at)) > 0),
    UNIQUE (content_id),
    UNIQUE (owner_id, reference)
) STRICT;

CREATE INDEX snapshot_records_owner_created
    ON snapshot_records(owner_id, julianday(created_at), snapshot_id)
    WHERE state = 'active';

CREATE INDEX snapshot_records_reconciliation
    ON snapshot_records(state, julianday(created_at), snapshot_id)
    WHERE state IN ('creating', 'deleting');
