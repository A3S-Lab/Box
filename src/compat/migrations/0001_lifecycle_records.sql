CREATE TABLE sandbox_records (
    sandbox_id TEXT PRIMARY KEY NOT NULL,
    record_json TEXT NOT NULL CHECK (json_valid(record_json)),
    owner_id TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.owner_id')
    ) STORED NOT NULL,
    operation_id TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.operation_id')
    ) STORED NOT NULL,
    generation INTEGER GENERATED ALWAYS AS (
        json_extract(record_json, '$.generation')
    ) STORED NOT NULL CHECK (generation > 0),
    state TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.state')
    ) STORED NOT NULL CHECK (
        state IN (
            'creating',
            'running',
            'pausing',
            'paused',
            'resuming',
            'killing',
            'killed',
            'failed'
        )
    ),
    created_at TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.created_at')
    ) STORED NOT NULL,
    expires_at TEXT GENERATED ALWAYS AS (
        json_extract(record_json, '$.expires_at')
    ) STORED NOT NULL,
    CHECK (sandbox_id = json_extract(record_json, '$.sandbox_id')),
    CHECK (length(trim(owner_id)) > 0),
    CHECK (length(trim(operation_id)) > 0),
    CHECK (length(trim(created_at)) > 0),
    CHECK (length(trim(expires_at)) > 0)
) STRICT;

CREATE UNIQUE INDEX sandbox_records_operation_id
    ON sandbox_records(operation_id);

CREATE INDEX sandbox_records_owner_state_created
    ON sandbox_records(owner_id, state, created_at, sandbox_id);

CREATE INDEX sandbox_records_expiry
    ON sandbox_records(state, expires_at, sandbox_id);
