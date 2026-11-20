CREATE TABLE accounts (id integer PRIMARY KEY, balance numeric);
CREATE FUNCTION audit_update() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$;

