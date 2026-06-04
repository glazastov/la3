---
name: la3-next
description: Start the next piece of work on the La3 → LLVM compiler following the golden workflow. Use when the user says "continue", "next subpart", "keep going on the compiler", "what's next", or asks to implement the next part of COMPILER_PLAN.md. Picks exactly ONE unchecked subpart, grounds it in the existing code and the language reference, and implements it — no more, no less.
---

# la3-next — pegar e executar o próximo subpart

Você é o motor do golden workflow (CLAUDE.md). Disciplina acima de velocidade.
**Um subpart por vez. Nunca dois.**

## 1. Orientar-se (sempre, mesmo que ache que já sabe)

1. Leia [COMPILER_PLAN.md](../../../COMPILER_PLAN.md) inteiro. Ache a **primeira** caixa
   `[ ]` ou `[~]` em ordem de fase. Esse é o alvo. Ignore tudo depois dele.
2. Leia o **Progress log** no fim do plano: o que foi feito por último, qual decisão
   ficou pendente, o que está "awaiting review".
3. Se a fase anterior está `awaiting review` e o usuário não aprovou, **pergunte**
   antes de avançar de fase. Subparts dentro de uma fase já aprovada podem seguir.

## 2. Aterrissar no código real

Antes de escrever qualquer linha:

- Abra os arquivos que o subpart toca (use a tabela "Source map" do CLAUDE.md).
- Para qualquer semântica da linguagem, **consulte [laila-lang-reference.md](../../../laila-lang-reference.md)**
  — é autoritativo. Se estiver ambíguo, decida pragmaticamente e **registre a decisão
  no COMPILER_PLAN.md** (não deixe na sua cabeça). Considere a skill `la3-spec`.
- Procure o padrão/idioma já usado no repo e copie-o (naming, densidade de comentário,
  submódulos). Código novo deve parecer que sempre esteve ali.

## 3. Implementar — só o subpart

- Faça a menor mudança correta que entrega o subpart. Não refatore de brinde, não
  adiante o próximo subpart, não toque em fase futura.
- Se descobrir que o subpart precisa de algo de outro subpart, **pare e diga ao
  usuário** — não expanda o escopo silenciosamente.
- Mantenha diagnósticos passando por [src/diag.rs](../../../src/diag.rs) (spans).
- O **interpretador é o oráculo** — não o quebre.

## 4. Não feche você mesmo

Quando o código estiver pronto, **não marque a caixa nem escreva no log ainda**.
Invoque a skill `la3-ship` — ela faz build → test → verify → tick → log com rigor.
Depois (opcional, recomendado em mudança grande) rode `la3-fiscal` para auditar.

## Pecados a evitar

- Fazer dois subparts "porque eram pequenos".
- Implementar semântica "de memória" sem abrir a reference.
- Marcar `[x]` antes de `la3-ship`.
- Começar a próxima fase sem o usuário revisar a anterior.
