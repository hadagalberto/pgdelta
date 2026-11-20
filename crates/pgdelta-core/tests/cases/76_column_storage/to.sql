CREATE TABLE public.docs (
    id integer PRIMARY KEY,
    body text STORAGE MAIN,
    title text STORAGE EXTENDED,
    raw bytea STORAGE DEFAULT
);
