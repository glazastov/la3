---
name: la3-autopsy
description: Methodically diagnose a La3 compiler bug, failing test, or interpreter/compiler mismatch using the IR dumps and the interpreter oracle. Use when a test fails, output is wrong, the front-end rejects a valid program (or accepts a bad one), or compiled output diverges from the interpreter — "why is this failing", "debug this", "the compiler is wrong here". Bisects the pipeline stage by stage instead of guessing.
---

# la3-autopsy — autópsia de bug no compilador

Bug de compilador não se resolve no olho. Se resolve **isolando o estágio** do
pipeline onde a verdade vira mentira. O interpretador é o oráculo: o que ele faz é,
por definição, o comportamento correto da v1.

## Pipeline e as janelas de inspeção

```
fonte → tokens → AST → resolve(nomes) → types → layout → borrowck → HIR → (MIR) → LLVM
         la3 tokens  la3 ast  la3 resolve  la3 types  la3 layout            la3 hir
```

Cada estágio tem um dump. Use-os para localizar onde o estado fica errado.

## Procedimento

1. **Reproduza minimamente.** Reduza o programa `.la3` ao menor caso que ainda
   falha. Um bug em 5 linhas vale dez em 50.

2. **Pergunte ao oráculo.** `cargo run -- run <caso>.la3` — o que o interpretador
   faz? Esse é o resultado-alvo. Se o próprio interpretador está errado, o bug é
   na semântica/oráculo, não no backend.

3. **Bisect pelos dumps**, do mais cedo ao mais tardio. No primeiro dump que mostra
   estado errado, você achou o estágio culpado:
   - `la3 tokens` — lexer comeu/cuspiu token errado?
   - `la3 ast` — parser montou a árvore errada?
   - `la3 resolve` — uso ligado ao binding errado? shadowing?
   - `la3 types` — tipo inferido errado / `Unknown` indevido?
   - `la3 layout` — tamanho/align/`drop=` errado?
   - `la3 hir` — tipo embutido ou `BindingId` errado, açúcar não rebaixado?

4. **Confirme a hipótese** lendo o código daquele estágio (Source map do CLAUDE.md),
   não o de baixo. Resista a "consertar" um sintoma três estágios abaixo da causa.

5. **Differential, quando há binário.** Compare stdout + exit code do binário contra
   o interpretador no mesmo input. A primeira divergência é a pista.

6. **Conserte na causa, adicione o teste de regressão** que captura exatamente este
   caso mínimo, e rode `la3-ship`.

## Armadilhas

- "Consertar" no interpretador um bug que é do checker (ou vice-versa) — você
  mascara, não resolve.
- Tratar `Unknown` como erro: o checker é deliberadamente **lenient**; `Unknown` é
  compatível com tudo. O bug pode ser um tipo que deveria ser concreto e não é.
- Assumir que o último estágio é o culpado só porque foi o último a mudar.
