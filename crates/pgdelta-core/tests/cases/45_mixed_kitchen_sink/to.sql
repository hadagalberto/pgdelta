CREATE SCHEMA finance;
CREATE EXTENSION pg_trgm WITH SCHEMA finance;
CREATE TABLE finance.accounts (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, owner text NOT NULL, balance numeric(10,2) DEFAULT 0);
CREATE TABLE finance.transactions (id bigint GENERATED ALWAYS AS IDENTITY, account_id bigint REFERENCES finance.accounts (id), amount numeric(10,2), posted_at timestamptz DEFAULT now());
CREATE TABLE audit_log (account_id bigint, note text, severity text);
CREATE INDEX idx_audit_note ON audit_log (note);
CREATE INDEX idx_audit_severity ON audit_log (severity) WHERE severity IS NOT NULL;
ALTER TABLE finance.transactions ADD CONSTRAINT transactions_amount_check CHECK (amount <> 0) NOT VALID;
