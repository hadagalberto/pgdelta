CREATE TABLE events (id bigint, kind text, body text, created_at timestamptz);
CREATE INDEX idx_events_lower_body ON events (lower(body));
CREATE INDEX idx_events_kind_created ON events (kind text_pattern_ops DESC NULLS LAST, created_at) INCLUDE (body) WITH (fillfactor = 70) WHERE kind IS NOT NULL;
CREATE INDEX idx_events_created_brin ON events USING brin (created_at);
