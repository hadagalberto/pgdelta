CREATE TABLE a (id integer PRIMARY KEY, b_id integer);
CREATE TABLE b (id integer PRIMARY KEY, a_id integer);
ALTER TABLE a ADD CONSTRAINT a_b_fkey FOREIGN KEY (b_id) REFERENCES b(id);
ALTER TABLE b ADD CONSTRAINT b_a_fkey FOREIGN KEY (a_id) REFERENCES a(id);
