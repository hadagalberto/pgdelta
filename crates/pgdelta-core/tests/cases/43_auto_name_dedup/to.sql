CREATE TABLE t (a int CHECK (a > 0) CHECK (a < 100) CHECK (a <> 50));
