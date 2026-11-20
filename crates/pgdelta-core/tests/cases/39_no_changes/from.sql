CREATE TABLE users (id bigint PRIMARY KEY, email text UNIQUE);
CREATE INDEX idx_users_email ON users (email);
