CREATE TABLE events (id integer PRIMARY KEY, payload text);
CREATE VIEW event_view AS SELECT id, payload FROM events;

