---
name: la3-ship
description: Close out a La3 compiler subpart with the full build → test → verify → tick → log discipline. Use after implementing a subpart, when the user says "ship it", "finish this subpart", "tick the box", "wrap up", or "update the plan". Refuses to mark anything done until it has actually run the build and tests and verified behavior.
---

# la3-ship — fechar um subpart com rigor

Você é o "closer". A regra de ouro: **nada é dado como pronto sem você ter rodado
o comando e visto a saída.** Sem "deve passar", sem "provavelmente está ok".

## Sequência obrigatória (na ordem)

### 0. Formatar (sempre)
```sh
cargo fmt --all
```
- Rode **sempre**, antes do build, para manter o código organizado e o diff limpo.
- Depois confirme que não sobrou nada fora de padrão: `cargo fmt --all -- --check`
  tem que sair limpo (exit 0).

### 1. Build limpo
```sh
cargo build --workspace
```
- Tem que terminar sem erro **e sem warning**. Warning é falha. Conserte antes de seguir.

### 2. Testes
```sh
cargo test --workspace
```
- Todos verdes. Anote o **número total** que passou (vai pro log).
- **Todo subpart precisa de uma bateria dedicada** para o que ele adicionou
  (`tests/*.rs` ou `#[cfg(test)]`). Se não existe, ela ainda não terminou — escreva
  os testes agora. Testes que não exercitam o código novo não contam.
- Se havia testes `#[ignore]`d marcando comportamento futuro, **não os apague nem
  desligue o ignore** a menos que o subpart seja exatamente o que os destrava;
  nesse caso, rode `cargo test -- --ignored` e confirme que ficaram verdes.

### 3. Verify (comportamento real, não só compilação)
- Rode o(s) exemplo(s) relevantes: `cargo run -- run examples/<x>.la3` e confira a saída.
- Quando o backend já emitir binário, **differential test**: a saída compilada
  (stdout + exit code) tem que bater com o interpretador. O interpretador é o oráculo.
- Confirme que o interpretador **não regrediu** num exemplo que já funcionava.

### 4. Tick + log + docs
Só depois dos passos 1–3 verdes:
- Marque a caixa do subpart `[x]` no COMPILER_PLAN.md, com uma frase do que entrou.
- Adicione **uma linha** no Progress log: data **absoluta** (hoje é resoluível pelo
  contexto), o que mudou, o nº de testes que passou, "0 warnings", e o que está
  pendente de review. Seja honesto sobre limitações/gaps deferidos.
- Se mudou comportamento, comando ou feature visível ao usuário, **atualize o
  [README.md](../../../README.md)**.

### 5. Parar no fim de fase
Se este subpart fecha **todos** os da fase: atualize o `STATUS` da fase para
`[x] done (awaiting review)` e **pare para o usuário revisar**. Não comece a
próxima fase. Diga claramente qual é o próximo subpart e que está aguardando.

## Saída que você dá ao usuário
Um resumo curto e factual: comandos rodados, contagem de testes (antes → depois),
o que foi verificado, o que ficou deferido, e qual o próximo passo. Sem floreio.
