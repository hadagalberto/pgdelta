fn main() { let p = pg_query::parse("CREATE TABLE t (id bigint PRIMARY KEY);").unwrap(); println!("{} stmts", p.protobuf.stmts.len()); }
