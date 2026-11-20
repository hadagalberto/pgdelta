CREATE SCHEMA old;
CREATE EXTENSION pg_trgm;
CREATE TABLE old.accounts (id bigint, owner text);
CREATE TABLE audit_log (account_id bigint REFERENCES old.accounts (id), note text);
CREATE INDEX idx_audit_note ON audit_log (note);
