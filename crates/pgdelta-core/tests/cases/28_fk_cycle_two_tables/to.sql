CREATE TABLE a (id bigint PRIMARY KEY, b_id bigint);
CREATE TABLE b (id bigint PRIMARY KEY, a_id bigint);
ALTER TABLE a ADD CONSTRAINT a_b_id_fkey FOREIGN KEY (b_id) REFERENCES b (id);
ALTER TABLE b ADD CONSTRAINT b_a_id_fkey FOREIGN KEY (a_id) REFERENCES a (id);
