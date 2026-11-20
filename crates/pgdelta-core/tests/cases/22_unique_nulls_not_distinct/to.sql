CREATE TABLE tags (id bigint, slug text);
ALTER TABLE tags ADD CONSTRAINT tags_slug_key UNIQUE NULLS NOT DISTINCT (slug);
