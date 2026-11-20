CREATE TABLE customers (id integer PRIMARY KEY, email text);
COMMENT ON TABLE customers IS 'Customer master data';
COMMENT ON COLUMN customers.email IS 'Contact address';

