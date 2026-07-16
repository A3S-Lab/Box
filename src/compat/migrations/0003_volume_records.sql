CREATE TABLE volume_records (
    volume_id TEXT PRIMARY KEY NOT NULL,
    record_json TEXT NOT NULL CHECK (json_valid(record_json)),
    owner_id TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.owner_id')
    ) STORED NOT NULL,
    name TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.name')
    ) STORED NOT NULL,
    runtime_name TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.runtime_name')
    ) STORED NOT NULL,
    state TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.state')
    ) STORED NOT NULL CHECK (state IN ('creating', 'active', 'deleting')),
    created_at TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.created_at')
    ) STORED NOT NULL,
    CHECK (volume_id = json_extract(record_json, '$.volume_id')),
    CHECK (length(trim(owner_id)) > 0),
    CHECK (length(name) > 0),
    CHECK (length(runtime_name) > 0),
    CHECK (length(trim(created_at)) > 0),
    UNIQUE (owner_id, name),
    UNIQUE (runtime_name)
) STRICT;

CREATE INDEX volume_records_owner_created
    ON volume_records(owner_id, julianday(created_at), volume_id)
    WHERE state = 'active';

CREATE INDEX volume_records_reconciliation
    ON volume_records(state, julianday(created_at), volume_id)
    WHERE state IN ('creating', 'deleting');
