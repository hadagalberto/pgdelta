CREATE TABLE sales (id integer PRIMARY KEY, amount numeric);
CREATE MATERIALIZED VIEW sales_totals AS SELECT count(*) AS n, sum(amount) AS total FROM sales WITH NO DATA;

