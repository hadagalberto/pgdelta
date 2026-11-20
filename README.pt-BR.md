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

Marco 5: além da cobertura de objetos, introspecção ao vivo e análise de
segurança dos marcos 1–4, o pgdelta agora modela também **modos `STORAGE` de
coluna** e **restrições de exclusão**, e foi auditado requisito por requisito
(veja [docs/SPEC-CHECKLIST.md](docs/SPEC-CHECKLIST.md)). Entradas hostis têm
testes de regressão: SQL malformado, bytes NUL, BOM/CRLF, caminhos do Windows,
fontes vazias e com 5.000 colunas, e expressões profundamente aninhadas (que
agora falham limpadamente em vez de estourar a pilha). Os 77 casos de
snapshot/convergência, paridade com banco ao vivo, testes de propriedade e
vetores válidos/inválidos continuam passando.

Veja [SPEC.md](SPEC.md) para o projeto,
[docs/SPEC-CHECKLIST.md](docs/SPEC-CHECKLIST.md) para a verificação de cada
requisito, [docs/parser.md](docs/parser.md) para a decisão sobre o parser e
[docs/safety.md](docs/safety.md) para as regras de segurança e suas citações
oficiais do PostgreSQL.

## Uso

Todos os exemplos com arquivos SQL abaixo usam as fixtures executáveis em
`examples/basic/`. A saída de cada comando foi verificada exatamente como
mostrada; os exemplos com `postgres://` também exigem um servidor acessível.

```sh
# Gerar um script de migração (escreve migration.sql)
cargo run -p pgdelta -- diff --from examples/basic/old.sql \
  --to examples/basic/new.sql --output migration.sql

# Plano em JSON (seções, comandos, perigos, resumo)
cargo run -p pgdelta -- diff --from examples/basic/old.sql \
  --to examples/basic/new.sql --json

# Plano seguro para produção: NOT VALID → VALIDATE → SET NOT NULL,
# FK NOT VALID → VALIDATE, CREATE INDEX CONCURRENTLY fora de transação
cargo run -p pgdelta -- diff --from examples/basic/old.sql \
  --to examples/basic/new.sql --online

# Dicas de renomeação explícitas (nunca inferidas silenciosamente)
cargo run -p pgdelta -- diff --from examples/basic/old.sql \
  --to examples/basic/new.sql \
  --rename table.users=people --rename column.people.email=contact

# Falhar no CI quando o plano contém perigos (código de saída 2)
cargo run -p pgdelta -- diff --from examples/basic/old.sql \
  --to examples/basic/new.sql --fail-on-hazard

# Analisar uma migração escrita à mão com as mesmas regras
cargo run -p pgdelta -- lint migration.sql --json

# Despejar o modelo normalizado de uma fonte
cargo run -p pgdelta -- inspect examples/basic/old.sql

# Comparar um banco ao vivo com um esquema declarativo
cargo run -p pgdelta -- diff --from postgres://user:pass@host/db \
  --to examples/basic/new.sql

# Deixar o PostgreSQL validar e normalizar fontes SQL em um banco shadow
cargo run -p pgdelta -- diff --from examples/basic/old.sql \
  --to examples/basic/new.sql --shadow postgres://user:pass@host/postgres
```

O script tem um bloco transacional explícito `BEGIN; ... COMMIT;` com
renomeações, remoções e alterações comuns. Etapas não transacionais
(`CREATE INDEX CONCURRENTLY`) vêm depois, em seção identificada. O resumo
humano e os marcadores de perigo vão para **stderr**; SQL e JSON vão para
**stdout**, para permitir pipes. `--color auto|always|never` controla ANSI
(`auto` respeita `NO_COLOR`).

### Códigos de saída

| Código | Significado |
| ------ | ----------- |
| 0 | Sucesso (inclui "sem alterações"). |
| 1 | Erro operacional (fonte inválida, SQL inválido, dica de renomeação inválida). |
| 2 | Perigos encontrados com `--fail-on-hazard`. |

### Análise de perigos

Cada comando é classificado como **destrutivo** (`DROP TABLE/SCHEMA/COLUMN`,
`DROP CONSTRAINT`, estreitamento de tipo) ou de **bloqueio** (`ALTER COLUMN
TYPE` com reescrita, `SET NOT NULL` sem `CHECK` validado, `ADD
CHECK/FOREIGN KEY` sem `NOT VALID`, `CREATE INDEX` comum,
`CREATE INDEX CONCURRENTLY` dentro de transação). `--online` elimina os
perigos de bloqueio que podem ser tornados seguros sem alterar o esquema
final; etapas destrutivas continuam sendo informadas porque descartam dados.

## Desenvolvimento

```sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

Os testes de integração ao vivo precisam de `PGDELTA_TEST_DATABASE_URL`
apontando para PostgreSQL 13–18. Sem a variável, eles imprimem o motivo da
ignoração e retornam; a CI roda a matriz `postgres:13` … `postgres:18`.

Fim de linha LF (`.gitattributes`). O repositório não é publicado no
crates.io.
