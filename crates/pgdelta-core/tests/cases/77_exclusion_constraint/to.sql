CREATE TABLE public.bookings (
    id integer PRIMARY KEY,
    during tsrange,
    CONSTRAINT bookings_during_excl EXCLUDE USING gist (during WITH &&)
);
