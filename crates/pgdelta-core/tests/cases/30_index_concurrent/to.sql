CREATE TABLE events (id bigint, kind text);
CREATE INDEX CONCURRENTLY idx_events_kind ON events (kind);
