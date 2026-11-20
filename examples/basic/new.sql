CREATE TABLE public.people (
    id integer PRIMARY KEY,
    contact text
);

CREATE TABLE public.posts (
    id integer PRIMARY KEY,
    user_id integer NOT NULL,
    title text
);

ALTER TABLE public.posts ADD CONSTRAINT posts_user_id_fkey
    FOREIGN KEY (user_id) REFERENCES public.people(id);

CREATE INDEX posts_title_idx ON public.posts (title);
