CREATE TABLE employees (id integer PRIMARY KEY, name text, dept text);
CREATE VIEW engineering AS SELECT id, name, dept FROM employees WHERE dept = 'eng';
