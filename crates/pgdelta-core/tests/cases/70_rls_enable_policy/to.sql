CREATE TABLE documents (id integer PRIMARY KEY, owner text);
ALTER TABLE documents ENABLE ROW LEVEL SECURITY;
CREATE POLICY owner_read ON documents FOR SELECT TO public USING (owner = current_user);

