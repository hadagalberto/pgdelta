CREATE TABLE events (id bigint, kind text, payload jsonb);
CREATE INDEX idx_events_kind ON events (kind);
ALTER TABLE events ADD CONSTRAINT events_kind_check CHECK (kind <> '');
