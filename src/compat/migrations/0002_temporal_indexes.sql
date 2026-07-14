DROP INDEX sandbox_records_owner_state_created;
DROP INDEX sandbox_records_expiry;

CREATE INDEX sandbox_records_owner_state_created
    ON sandbox_records(
        owner_id,
        state,
        julianday(created_at),
        sandbox_id
    );

CREATE INDEX sandbox_records_expiry
    ON sandbox_records(state, julianday(expires_at), sandbox_id);

CREATE INDEX sandbox_records_reconcilable
    ON sandbox_records(julianday(created_at), sandbox_id)
    WHERE state IN (
        'creating',
        'running',
        'pausing',
        'paused',
        'resuming',
        'killing'
    );
