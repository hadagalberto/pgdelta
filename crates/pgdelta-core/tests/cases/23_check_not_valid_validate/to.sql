CREATE TABLE orders (id bigint, amount numeric);
ALTER TABLE orders ADD CONSTRAINT orders_amount_check CHECK (amount >= 0);
