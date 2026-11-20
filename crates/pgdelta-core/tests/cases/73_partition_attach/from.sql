CREATE TABLE measurements (city text, logged_at date, value numeric) PARTITION BY LIST (city);
CREATE TABLE measurements_eu (city text, logged_at date, value numeric);

