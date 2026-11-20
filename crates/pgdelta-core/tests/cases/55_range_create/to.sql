CREATE FUNCTION int4range_diff(a integer, b integer) RETURNS double precision LANGUAGE SQL IMMUTABLE AS $$ SELECT a::double precision - b::double precision $$;
CREATE TYPE int4range_custom AS RANGE (SUBTYPE = integer, SUBTYPE_DIFF = int4range_diff);
