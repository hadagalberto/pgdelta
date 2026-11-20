CREATE TABLE public.docs (
    id integer PRIMARY KEY,
    body text,
    title text STORAGE PLAIN,
    raw bytea STORAGE EXTERNAL
);
