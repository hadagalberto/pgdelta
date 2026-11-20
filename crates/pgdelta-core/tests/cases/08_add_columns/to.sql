CREATE TABLE users (id bigint PRIMARY KEY);
ALTER TABLE users ADD COLUMN email text NOT NULL DEFAULT 'unknown@example.com';
ALTER TABLE users ADD COLUMN score numeric(8,2);
