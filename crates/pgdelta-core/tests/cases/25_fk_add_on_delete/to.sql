CREATE TABLE users (id bigint PRIMARY KEY);
CREATE TABLE orders (id bigint, user_id bigint);
ALTER TABLE orders ADD CONSTRAINT orders_user_id_fkey FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE;
