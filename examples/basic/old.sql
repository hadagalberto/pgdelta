CREATE TABLE public.users (
    id integer PRIMARY KEY,
    email text
);

CREATE TABLE public.posts (
    id integer PRIMARY KEY,
    user_id integer,
    title text
);
