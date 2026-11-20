CREATE TABLE t (note text);
CREATE INDEX i ON t (lower(note));
