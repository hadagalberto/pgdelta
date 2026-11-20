CREATE TABLE members (id bigint PRIMARY KEY);
CREATE TABLE orders (id bigint, user_id bigint REFERENCES members (id));
