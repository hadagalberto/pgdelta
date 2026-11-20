# pgdelta

Um CLI + biblioteca Rust de binário único que compara dois esquemas
PostgreSQL e gera um script de migração seguro e ordenado por dependências.
As fontes são arquivos `.sql` ou diretórios de arquivos `.sql` ("esquema
declarativo"); fontes de banco ao vivo chegam no marco 2.

Licença MIT. [Read in English](README.md).

## Estado

Marco 1: scaffold do workspace, CLI com `clap`, modelo normalizado de
catálogo, fontes de arquivo/diretório SQL, diff + planejamento de schemas,
extensões, tabelas, colunas (defaults, geradas, identidade, collation),
constraints e índices, com saída ordenada por dependências e cobertura por
snapshots insta.

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
