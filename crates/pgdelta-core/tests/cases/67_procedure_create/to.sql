CREATE TABLE counters (id integer PRIMARY KEY, value integer);
CREATE PROCEDURE reset_counter() LANGUAGE SQL AS $$ UPDATE counters SET value = 0; $$;
