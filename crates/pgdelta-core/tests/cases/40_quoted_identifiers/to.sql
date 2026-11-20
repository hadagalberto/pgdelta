CREATE TABLE "Users" ("Id" bigint PRIMARY KEY, "Email" text NOT NULL, "CreatedAt" timestamptz DEFAULT now());
