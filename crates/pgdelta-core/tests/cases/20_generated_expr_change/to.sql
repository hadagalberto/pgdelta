CREATE TABLE invoices (subtotal numeric(10,2), total numeric(10,2) GENERATED ALWAYS AS (subtotal * 3) STORED);
