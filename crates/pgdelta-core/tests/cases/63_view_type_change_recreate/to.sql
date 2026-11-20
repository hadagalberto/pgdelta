CREATE TABLE events (id integer PRIMARY KEY, payload varchar(80));
CREATE VIEW event_view AS SELECT id, payload FROM events;

