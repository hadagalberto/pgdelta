CREATE SCHEMA sales;
CREATE SCHEMA crm;
CREATE TABLE sales.customers (id bigint PRIMARY KEY, region bigint);
CREATE TABLE sales.regions (id bigint PRIMARY KEY);
CREATE TABLE crm.contacts (id bigint PRIMARY KEY);
CREATE TABLE crm.notes (id bigint PRIMARY KEY, contact_id bigint REFERENCES crm.contacts (id));
CREATE INDEX idx_sales_customers_region ON sales.customers (region);
ALTER TABLE sales.customers ADD CONSTRAINT customers_region_fkey FOREIGN KEY (region) REFERENCES sales.regions (id);
