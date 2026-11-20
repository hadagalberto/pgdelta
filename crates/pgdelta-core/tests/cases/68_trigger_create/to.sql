CREATE TABLE accounts (id integer PRIMARY KEY, balance numeric);
CREATE FUNCTION audit_update() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$;
CREATE TRIGGER check_update BEFORE UPDATE OF balance ON accounts FOR EACH ROW WHEN (OLD.balance IS DISTINCT FROM NEW.balance) EXECUTE FUNCTION audit_update();

