# pgdelta

Um CLI + biblioteca Rust de binário único que compara dois esquemas
PostgreSQL e gera um script de migração seguro e ordenado por dependências.
As fontes podem ser arquivos `.sql`, diretórios de arquivos `.sql` ("esquema
declarativo") ou bancos ao vivo `postgres://`, introspectados via `pg_catalog`
(PostgreSQL 13–18). A opção `--shadow` aplica o SQL em um banco descartável e
introspecta o resultado — o modo mais preciso, pois o próprio PostgreSQL
valida e normaliza o DDL.

Licença MIT. [Read in English](README.md).

## Estado

Marco 3: modelo normalizado e diff/planejamento para todos os tipos de objeto
do SPEC, com introspecção ao vivo de `pg_catalog` via `tokio-postgres`.
Expressões e tipos retornados pelo catálogo passam pelos mesmos normalizadores
do parser SQL, garantindo paridade entre uma fonte SQL e um banco ao vivo.
Testes de convergência ao vivo, paridade e modo shadow cobrem os 75 casos de
snapshot; os testes são ignorados com motivo impresso quando
`PGDELTA_TEST_DATABASE_URL` não está definida.

Veja [SPEC.md](SPEC.md) para o projeto completo e
[docs/parser.md](docs/parser.md) para a decisão sobre o parser e as regras de
normalização.

## Uso

```sh
# Gerar um script de migração
cargo run -p pgdelta -- diff --from schema.sql --to wanted/ --out migration.sql

# Plano em JSON em vez de SQL
cargo run -p pgdelta -- diff --from antigo.sql --to novo.sql --json

# Despejar o modelo normalizado de uma fonte
cargo run -p pgdelta -- inspect schema/
```

O script tem um bloco transacional explícito `BEGIN; ... COMMIT;`; eventuais
`CREATE INDEX CONCURRENTLY` vêm depois, em uma seção separada e identificada.

## Desenvolvimento

```sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

Fim de linha LF (`.gitattributes`). O repositório não é publicado no
crates.io.
