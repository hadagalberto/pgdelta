CREATE FUNCTION area(w integer, h integer) RETURNS integer LANGUAGE SQL IMMUTABLE AS $$ SELECT w * h; $$;

