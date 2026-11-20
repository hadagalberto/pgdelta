CREATE TABLE accounts (id integer PRIMARY KEY, balance numeric);
CREATE FUNCTION audit_update() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$;
CREATE TRIGGER check_update AFTER UPDATE ON accounts FOR EACH ROW EXECUTE FUNCTION audit_update();

