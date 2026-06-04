---
name: la3-spec
description: Resolve La3 language semantics from the authoritative reference before implementing them. Use when you're about to implement or change behavior and need the real rule — "how does X work in La3", "what's the spec for casts/ownership/match/operators", or whenever a semantic detail is uncertain. Pulls the exact wording from laila-lang-reference.md and, when the reference is ambiguous, forces an explicit recorded decision instead of a guess.
---

# la3-spec — oráculo da especificação

CLAUDE.md é claro: **não chute** semântica. Esta skill garante que toda regra
implementada está ancorada na fonte autoritativa, ou registrada como decisão.

## Como usar

1. **Ache a regra.** Procure em [laila-lang-reference.md](../../../laila-lang-reference.md):
   ```sh
   grep -ni "<termo>" laila-lang-reference.md
   ```
   Leia a seção inteira em volta, não só a linha. Cite a seção (ex.: "Section 11")
   quando for justificar uma escolha de implementação.

2. **Cruze com o que já existe.** O `typeck`/`borrowck`/`interp` já podem ter
   codificado a regra. Confira se o novo código é consistente com o oráculo
   (interpretador) — divergência silenciosa é bug.

3. **Quando a reference é vaga ou omissa** (o doc avisa: GC vs ownership, `any`,
   lifetimes, etc. são "loosely specified"):
   - Escolha o comportamento mais simples, são e consistente com o resto da v1.
   - **Registre a decisão** no COMPILER_PLAN.md (tabela de decisões ou Progress log),
     com data absoluta e o porquê. Decisão não registrada não existe.
   - Prefira fontes primárias para detalhes de Rust edition-2024 / LLVM IR /
     inkwell / llvm-sys: o Rust Reference, o LLVM Language Reference, os docs do
     inkwell. Não invente API.

4. **Devolva** ao chamador: a regra exata (com a seção citada), como ela mapeia
   pro código, e qualquer decisão nova que você teve que registrar.

## Heurística

Se você se pegar escrevendo "acho que La3 faz assim", pare e rode o grep. Cinco
segundos de leitura evitam um subpart inteiro construído sobre uma suposição errada.
