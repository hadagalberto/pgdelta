CREATE TABLE events (id bigint, ref text);
CREATE UNIQUE INDEX uq_events_ref ON events (ref) NULLS NOT DISTINCT;
