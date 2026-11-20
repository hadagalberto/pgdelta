CREATE TABLE measurements (city text, logged_at date, value numeric) PARTITION BY RANGE (logged_at);
CREATE TABLE measurements_2024 PARTITION OF measurements FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');

