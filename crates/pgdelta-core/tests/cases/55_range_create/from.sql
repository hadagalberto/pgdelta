CREATE FUNCTION int4range_diff(a integer, b integer) RETURNS double precision LANGUAGE SQL IMMUTABLE AS $$ SELECT a::double precision - b::double precision $$;
