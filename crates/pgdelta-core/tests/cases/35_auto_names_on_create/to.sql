CREATE TABLE u (id bigint PRIMARY KEY);
CREATE TABLE t (
    a integer PRIMARY KEY REFERENCES u (id),
    b text UNIQUE,
    CONSTRAINT explicit_name CHECK (b <> '')
);
CREATE TABLE t2 (a int CHECK (a > 0), CHECK (a < 10));
