CREATE FUNCTION area(w integer, h integer) RETURNS integer LANGUAGE SQL IMMUTABLE AS $$ SELECT w * h; $$;
CREATE FUNCTION area(w double precision, h double precision) RETURNS double precision LANGUAGE SQL IMMUTABLE AS $$ SELECT w * h; $$;

