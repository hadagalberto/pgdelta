CREATE FUNCTION double_it(a integer) RETURNS integer LANGUAGE SQL IMMUTABLE AS $$ SELECT a * 2; $$;

