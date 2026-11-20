CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email character varying(255) NOT NULL UNIQUE,
    display_name text COLLATE "C",
    is_active boolean DEFAULT true,
    score numeric(8,2),
    CONSTRAINT users_score_nonneg CHECK (score >= 0)
);
