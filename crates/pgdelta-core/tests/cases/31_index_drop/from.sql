CREATE TABLE events (id bigint, kind text);
CREATE INDEX idx_events_kind ON events (kind);
CREATE INDEX idx_events_id ON events (id);
