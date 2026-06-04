# La3 — The À-la-carte Dynamic Standard Library

> **Status:** design document for **Phase 12** (Pillar 2 of the North Star in
> [COMPILER_PLAN.md](COMPILER_PLAN.md)). No code yet; no rush. This is the
> authoritative specification of *how the dynamic stdlib must work*, written to be
> implemented later "the best way possible". It is intentionally detailed: the
> mechanism has subtle invariants, and getting them wrong silently breaks either
> independence or reproducibility.
>
> **Reading order:** Parts build on each other. Part 1 fixes the requirements and
> vocabulary; Parts 2–4 are the mechanism (modules → capabilities → resolver);
> Part 5 is pipeline/ABI integration; Part 6 is worked examples, edge cases, prior
> art, and open questions.

---

## Part 1 — Motivation, requirements, terminology

### 1.1 The goal in one paragraph

La3 must compile both a Raspberry Pi Pico bootloader/kernel **and** a web/PC
application from the *same language and the same standard library source*. The
standard library is therefore not one monolith and not a `no_std` fork. It is a
**set of many small, fully independent modules**, and a build pulls in **only what
the program actually uses**. When two modules happen to coexist in a build, one
may **opportunistically reuse** the other's implementation to shrink — but a
module used *alone* must depend on **nothing** else. This document specifies the
model that makes those three properties (independence, usage-driven inclusion,
opportunistic sharing) hold simultaneously, and provably.

### 1.2 Formal requirements (the contract this design must satisfy)

These are normative. Every later decision is justified by one of them.

- **R1 — Independence.** For any stdlib module `M`, the program `use M` compiled
  with no other stdlib module present must build, pass `M`'s tests, and link with
  **zero** references to any other stdlib module. *Independence is a per-module
  invariant, checkable in isolation.*
- **R2 — Usage-driven inclusion.** The final artifact contains code for a stdlib
  symbol **iff** that symbol is reachable from `main` (transitively). No "link the
  whole library" baseline.
- **R3 — Opportunistic sharing.** If modules `A` and `B` are both present and `A`
  needs a facility that `B` already provides, `A` **may** be lowered to use `B`'s
  implementation, dropping `A`'s private copy. Sharing only ever **removes**
  duplication that would otherwise be present; it never **adds** a dependency that
  would exist in the alone case (so it can never violate R1 for the alone build).
- **R4 — Two modes.** A build flag selects:
  - **granular** (`--stdlib granular`): independence + opportunistic sharing +
    aggressive size-oriented dead-code elimination. Target: Pico, kernels, the
    smallest possible artifact.
  - **monolithic** (default): the whole stdlib is available and free to
    interdepend, tuned for throughput on PC/Web (cross-module inlining, LTO).
- **R5 — Behavioural identity.** For any program `P`, the granular build and the
  monolithic build of `P` must be **observationally identical** (same output, same
  exit code, same effects). Only size, layout, and performance may differ. *This
  is the headline conformance property and the reason sharing must be sound.*
- **R6 — Determinism / reproducibility.** Given the same source, the same module
  set, and the same flags, resolution must produce the **same** provider choices
  and therefore a bit-reproducible artifact (modulo toolchain). No dependence on
  hash-map iteration order, filesystem order, or timestamps.

### 1.3 Why not the obvious alternatives

- **A single monolith + linker `--gc-sections`.** Gets R2 (dead code stripped)
  but not R3 (no semantic sharing decisions; duplication between independent
  implementations is folded only when the linker's identical-code-folding happens
  to match byte-for-byte) and the source is not authored for R1 (modules
  hard-reference each other). We want sharing as a *deliberate, contract-checked*
  decision, not a linker coincidence.
- **Cargo-style feature flags.** Manual, coarse, and combinatorial: every optional
  dependency is a flag the user must set, and the matrix of valid combinations
  explodes. Our resolution is **automatic** (driven by what's reachable), and
  independence is a *checked invariant*, not a flag the author might forget.
- **A `no_std` / `std` split.** A hard two-world fork doubles the surface and
  still bundles a fixed subset. La3 rejects this (user decision): one source, one
  vocabulary, sized by use.

### 1.4 Terminology (used precisely throughout)

| Term | Meaning |
| --- | --- |
| **Module** | The unit of *independence*, *versioning*, and *capability declaration*. A named collection of public exports plus private internals. The smallest thing that can be "present" or "absent" in a build. |
| **Public export** | A symbol (function, type, method, const) a module offers to *user code* as its API. |
| **Internal** | A symbol private to a module's implementation; never referenced by name from outside. |
| **Capability** | A named, versioned, semantically-specified facility that more than one module might need (e.g. `growable_buffer`, `sort`, `utf8_decode`, `format_int`). Defined by a *signature* + a *semantic contract* + a *conformance suite*. The unit of *sharing*. |
| **Wanter** | A module that needs a capability `C`. It declares `wants C` and ships a private **fallback** implementation of `C`. |
| **Fallback** | A wanter's own private implementation of a capability, used when no external provider is present (this is what keeps R1 true). |
| **Provider** | A module that offers an implementation of `C` for others to use. A **canonical** provider exists to provide `C` (high quality, preferred); a **fallback provider** is a wanter's fallback promoted to serve others. |
| **Present-set** | The set of modules considered available during resolution. *This single input is what distinguishes the two build modes* (granular = only reachable; monolithic = all). |
| **Resolution** | The whole-program pass that, given the present-set, binds every `wants C` to exactly one provider (or to the wanter's own fallback), to a fixpoint. |
| **Binding** | The result of resolution for one `(wanter, C)` pair: the concrete provider symbol its `wants C` call sites are rewritten to. |
| **Conformance suite** | The executable contract of a capability: a battery every provider/fallback must pass. Sharing is sound *because* both sides pass the same suite (this is how R5 is guaranteed across a swap). |

### 1.5 Document conventions

- "**MUST / MUST NOT / MAY**" are normative (RFC 2119 sense).
- Pseudo-code is illustrative La3-ish / Rust-ish; the real annotations syntax is
  pinned in Part 2.
- Cross-references like *(see §4.3)* point within this document; *(Phase N)*
  points at [COMPILER_PLAN.md](COMPILER_PLAN.md).

---

## Part 2 — The module model

### 2.1 What a module is

A **module** is a directory of La3 source plus an optional `extern "C"` runtime
shim (Phase 4), compiled as a unit, that exposes a stable **public API** and
declares its **capability surface** (`provides` / `wants`). Modules are the atoms
of the dynamic stdlib: independence (R1), inclusion (R2), and sharing (R3) are all
defined at module granularity, while dead-code elimination additionally prunes at
*symbol* granularity inside the modules that survive.

A module has four kinds of symbols:

1. **Public exports** — the API user code calls. Stable, versioned.
2. **Capability providers** — implementations a module offers to *other* modules
   (canonical providers). May or may not also be public exports.
3. **Fallbacks** — private implementations of capabilities the module `wants`,
   used only when no external provider is bound (§3, §4).
4. **Internals** — everything else; never referenced by name from outside.

### 2.2 The independence invariant, made mechanical

R1 says a module used alone references nothing else. We make this *checkable*, not
aspirational, with a single rule the compiler enforces:

> **Rule I (no foreign hard references).** A module's code MAY reference, by name,
> only (a) its own symbols and (b) **capabilities** (never another module's
> symbols directly). All cross-module use goes through the capability layer.

Because the only outward edges a module can express are `wants C` edges, and every
`wants C` has a private fallback (§3.2), a module always has a self-contained
lowering: resolve every `wants` to its own fallback and nothing external is
referenced. That self-contained lowering is exactly the **alone build**.

**Enforcement — the *compile-alone* check (CI gate).** For every module `M`:

```
build M with present-set = {M}        # only M; every `wants` binds to M's fallback
assert: builds, links with no external symbols, M's test suite passes
```

If `M` fails to build alone, it violated Rule I (it reached for a foreign symbol
instead of declaring a capability). This check is cheap, fully parallel (one build
per module), and is the operational definition of R1. It MUST be part of CI.

### 2.3 The module manifest

Every module carries a manifest. To keep a **single source of truth**, the
manifest is *derived from in-source annotations* (so the declaration lives next to
the code it describes) and emitted as a machine-readable file for tooling. The
in-source form (syntax provisional, to be pinned when Phase 12 starts):

```la3
// module: text            v1.3.0
// summary: UTF-8 text, splitting, formatting

@export
fn split(s: str, sep: str) -> List<str> { ... }

// "I need a growable byte buffer. Here is my private fallback if nobody better
//  is in the build." The fallback MUST pass capability `growable_buffer`'s
//  conformance suite (§3.4).
@wants(growable_buffer ^1)         // semver-range requirement
@fallback(growable_buffer)
fn _buf_new() -> RawBuf { ... }    // a small, correct, unoptimized implementation

// "I can provide capability `utf8_decode` to others (canonical)."
@provides(utf8_decode v1, priority = 10)
fn decode(bytes: &[u8]) -> Result<str> { ... }
```

The derived manifest (one per module, illustrative JSON):

```jsonc
{
  "module": "text", "version": "1.3.0",
  "exports": ["split", "decode", "..."],
  "provides": [
    { "capability": "utf8_decode", "version": "1.0.0", "symbol": "text::decode",
      "priority": 10 }
  ],
  "wants": [
    { "capability": "growable_buffer", "range": "^1",
      "fallback": "text::_buf_new" }
  ]
}
```

Notes:

- `provides.priority` is an integer used only to break ties between two **canonical**
  providers of the same capability (§4.4); higher wins. It is *not* a quality
  score the resolver optimizes — resolution is not an optimizer (§4.6).
- A module MAY both `wants` and `provides` the *same* capability is **forbidden**
  (it would mean "I need X and I am the authority on X" — express it as a plain
  internal instead). A module that provides `C` and also uses `C` simply calls its
  own provider directly.
- A fallback symbol MAY be promoted to serve *other* wanters (becoming a fallback
  provider) — see §4.4. This is how two modules that each ship a fallback can still
  deduplicate onto one of them when no canonical provider exists.

### 2.4 Versioning

- **Modules** use semver. A public export's signature/semantics change ⇒ major bump.
- **Capabilities** are versioned independently of the modules that implement them
  (§3.3): the *contract* evolves on its own timeline. A `wants` states a range
  (`^1`, `>=1.2 <2`); a `provides` states the exact contract version it satisfies.
  The resolver only binds a provider whose version satisfies the wanter's range
  (§4.3). This lets a capability gain a v2 without forcing every wanter to move.

### 2.5 Module granularity guidance (authoring)

Independence has a cost: a too-fine module split multiplies fallbacks and
capability seams; a too-coarse one drags unused code into every build (hurting
R2). Guidance for stdlib authors:

- A module should be a **coherent vocabulary** a user reaches for as a unit
  (`text`, `time`, `json`, `collections`, `io`, `math`, `crypto.sha`).
- A **capability** should be a **natural reuse seam** *between* such vocabularies —
  a data structure (`growable_buffer`, `hashmap`), an algorithm (`sort`,
  `binary_search`), an encoding (`utf8_decode`, `hex`), or a primitive
  (`format_int`, `memcpy`). If only one module would ever need it, it is an
  *internal*, not a capability.
- Prefer **few, well-specified capabilities** over many ad-hoc ones: each
  capability is a contract you must keep stable and conformance-test forever.

---

## Part 3 — Capabilities: the sharing currency

Capabilities are the only mechanism by which modules share. Everything that makes
opportunistic sharing **sound** (R5) and **independence-preserving** (R1, R3) lives
in how a capability is specified and how its implementations are checked.

### 3.1 Anatomy of a capability

A capability `C` is a four-part object, declared once, centrally (not inside any
single module — it is a contract *between* modules):

1. **Identity** — a unique name and a semver version line (`growable_buffer`, `v1`).
2. **Signature** — the exact types of the operations it exposes. A capability is
   usually a small *interface* (a handful of related operations), not a single
   function. Example:
   ```la3
   capability growable_buffer v1 {
       type Buf                          // opaque to wanters
       fn new() -> Buf
       fn push(b: &mut Buf, x: u8)
       fn as_slice(b: &Buf) -> &[u8]
       fn len(b: &Buf) -> usize
       fn drop(b: Buf)                   // ownership: providers own the storage
   }
   ```
3. **Semantic contract** — the observable behaviour, in prose precise enough to
   test: e.g. "`as_slice` returns exactly the bytes pushed, in order; `push` is
   amortized O(1); `len` equals the number of `push`es minus none (no element is
   dropped or reordered); `drop` frees all storage and is idempotent w.r.t. R1's
   deterministic-drop model." Any behaviour a wanter could *observe* is fixed here.
4. **Conformance suite** — the executable encoding of the contract (§3.4).

The opaque `type Buf` matters: wanters program against the capability's *interface*
and never against a provider's representation. That is what lets the resolver swap
provider A for provider B without the wanter noticing (R5).

### 3.2 Wanters and fallbacks

A wanter declares `@wants(C ^range)` and MUST supply an `@fallback(C)`
implementation that satisfies `C` entirely on its own. The fallback:

- is **private** (an internal symbol) until/unless promoted (§4.4);
- SHOULD be **small and obviously correct**, not fast — its job is to keep the
  alone build working and small, not to win benchmarks. The fast path is what a
  *canonical* provider brings when present;
- MUST pass `C`'s conformance suite (enforced at module test time, §3.4). A
  fallback that fails conformance is a build error for the module, because it would
  make the alone build behave differently from a shared build, violating R5.

Why fallbacks instead of "hard error if no provider"? Because R1 demands the alone
build *work*, not *fail with a helpful message*. The fallback is the price of
independence, paid in a few bytes that vanish the moment a better provider shows up.

### 3.3 Providers

A **canonical provider** declares `@provides(C version, priority = p)`. It exists
to offer `C` well (the optimized hashmap, the SIMD `utf8_decode`). It too MUST pass
`C`'s conformance suite. A provider's `version` states which contract version it
satisfies; the resolver matches it against wanters' ranges (§4.3).

A **fallback provider** is a wanter's fallback that the resolver *promotes* to serve
other wanters when no canonical provider is present (§4.4). The promotion is purely
a resolver decision; the author writes a fallback, not a "maybe-provider".

**Provider obligations (normative):**

- P-1: passes `C`'s conformance suite for the declared version.
- P-2: is **pure with respect to the contract** — no observable behaviour outside
  what `C` specifies (no global state a wanter could detect, no ordering surprises).
  This is what guarantees substitutability.
- P-3: respects La3 ownership (Phase 1.6 / MIR 3.5): the capability signature fixes
  who owns what; a provider MUST honour the declared drop responsibility so that
  swapping providers cannot change when memory is freed.

### 3.4 The conformance suite — why sharing is sound

R5 (granular ≡ monolithic) reduces to a single local property:

> **Substitutability.** For every capability `C`, **all** implementations of `C`
> (every fallback and every provider) are observationally interchangeable.

We do not *prove* this; we **test** it, exhaustively and identically, against every
implementation. For each capability `C` there is one conformance suite
`conform(C)`. The build system runs `conform(C)` against:

- every `@fallback(C)` in every wanter, and
- every `@provides(C)` in every provider.

An implementation that fails `conform(C)` cannot be shipped (module build error)
and cannot be bound by the resolver. Therefore *any* binding the resolver chooses
selects an implementation that passed the same suite as the one it replaced ⇒ the
swap is behaviourally invisible **to the strength of the suite** ⇒ R5 holds for
every program without per-program verification, *as strongly as `conform(C)`
captures C's observable behaviour*.

This is an honest reduction, not a proof: testing cannot *prove* equivalence. But it
moves the obligation to exactly one place — the suite — and the **differential-
against-reference** technique makes that obligation as strong as a single trusted
reference implementation (the same discipline as "the interpreter is the oracle").
A capability whose behaviour is too rich to pin with a suite is a sign the seam is
wrong: shrink the capability until its contract is fully testable (§2.5).

Conformance suites SHOULD combine:

- **example tests** (fixed inputs → fixed outputs, including edge cases: empty,
  max-size, boundary, error paths);
- **property tests** (randomized: e.g. "for any byte sequence, `as_slice(build(seq))
  == seq`"), with a **fixed seed** so the gate is deterministic (R6);
- **differential tests against a reference implementation** of `C` (often the
  simplest fallback is the reference): every other implementation must match the
  reference output on every input. This is the strongest and cheapest way to pin
  substitutability, and it mirrors the project-wide rule that the *interpreter is
  the oracle* (see memory `la3-oracle-over-reference`).

**Contract evolution.** A change that alters any observable behaviour ⇒ a new
capability *version* with its own suite. Old providers keep satisfying the old
version; wanters move ranges deliberately. The suite is the contract; the contract
is versioned; therefore behaviour is versioned (§2.4).

### 3.5 Capabilities and generics

Many capabilities are generic (`sort<T: Ord>`, `hashmap<K: Hash, V>`). Sharing a
generic capability is still subject to **monomorphization** (Phase 7.1): the bound
provider is specialized per concrete type used. Consequences to keep in mind:

- Sharing deduplicates the *provider source*, but each `(provider, concrete type)`
  pair is still a distinct monomorphic instance in the binary. Two modules sharing
  `hashmap` save duplication only on the type instantiations they have **in common**.
- The conformance suite for a generic capability MUST be instantiated at enough
  representative types (a small scalar, an owned heap type, a struct) to exercise
  the ownership/drop contract (P-3) across the type shapes the back-end lays out
  differently.
- Resolution (binding) happens **before** monomorphization: the resolver rewrites
  `wants C` call sites to the provider symbol while still generic; 7.1 then
  specializes the now-direct calls. Order is fixed in §5.2.

### 3.6 Implementation keystone — a capability is an *implicit interface parameter*

The single most important framing for making this buildable rather than a new
mechanism invented from scratch:

> **A `wants C` is an implicit generic/interface bound, and binding a provider is
> instance selection.** A module that `wants C` is, semantically, generic over
> "some implementation of the interface `C`" — exactly like a Phase-7 generic
> function bounded by an interface (`fn f<B: GrowableBuffer>(...)`). The resolver is
> a *deterministic, build-set-relative instance selector*: instead of the user
> writing the type argument, the resolver picks it from the present-set.

This reframing buys us almost everything for free:

- **Reuse, don't reinvent.** Binding + specialization are the **monomorphization
  machinery of Phase 7.1**. A capability is an interface (Phase 7.1 already does
  interfaces + static dispatch via bounds); a wanter is generic over it; the
  resolver supplies the instance; mono specializes. The genuinely *new* work in
  Phase 12 is the **selection policy and the fixpoint** (§4), not a new
  code-generation path.
- **By-value opaque types just work.** Because the wanter is **monomorphized against
  the chosen provider**, the concrete layout of `Buf` is known when the wanter is
  specialized — so a wanter MAY hold `Buf` by value, on its stack or inside its own
  structs, with zero indirection. This is the *zero-cost* property; we do not force
  capability types behind a pointer (that easy cop-out is rejected — §5.5).
- **Coherence, but relative to the build.** Rust forbids two `impl`s of a trait for a
  type (global coherence) so selection is unambiguous. We do not need global
  coherence; we need **per-build determinism**: within one build the resolver picks
  exactly one provider per `wants C` (§4.4), so the instance is unique *for that
  build*. Different builds (granular vs monolithic, or different module sets) MAY
  select different providers — fine precisely because the conformance contract
  (§3.4) makes the choice behaviourally invisible (R5).

The cost of this keystone is the cost of monomorphization: a wanter specialized
against its provider would be duplicated only if the *same* wanter were bound to
*different* providers **in the same build** — which never happens (the resolver
binds each `wants C` to exactly one provider per build). So within a build there is
no duplication from this, and across builds there is no shared artifact to
duplicate. The keystone is therefore zero-overhead in the dimension that matters,
at the cost of leaning hard on the monomorphizer — a hard-but-solved problem, not an
impossible one.

---

## Part 4 — The capability resolver

The resolver is the brain of the dynamic stdlib. Input: the program plus the
**present-set** (which differs by mode, §5.1). Output: for every `wants C` edge, a
**binding** to exactly one implementation symbol, such that R1–R6 hold. It runs as
a whole-program MIR→MIR pass (§5.2).

### 4.1 Why it is a fixpoint, not a single pass

Naively: "for each wanted capability, if a provider is present, bind to it." But
**binding changes which *symbols* are reachable**, and that can expose new `wants`
edges to resolve:

- Binding `A.wants(C)` to provider `B::c` makes `B::c` (and everything `B::c`
  transitively calls within already-present modules, including `B`'s own `wants`)
  **reachable** — a new `wants` edge to bind.
- Resolving that edge may make yet more provider code reachable, and so on.

So resolution and symbol-reachability are mutually recursive and MUST be solved to a
**least fixpoint**: start from `main`, grow the reachable symbol set and the bindings
together until nothing changes. Two things keep this tame:

- **The module present-set is (by the §4.4 presence gate) the ordinary-reachability
  closure** — capabilities fold *within* it and do not, by default, pull new modules
  in. So the fixpoint grows *symbols*, not the module universe (the expensive
  dimension is bounded up front). An explicit pin (§6.5) is the only thing that adds
  a module, and it does so once, before the loop.
- **Monolithic mode fixes the present-set to "all"** so the gate is vacuous; the
  binding pass is otherwise identical — the elegance of §5.1 (one resolver, two
  present-sets).

### 4.2 The algorithm

```
INPUT:  program P (already lowered to MIR, still generic),
        all_modules,                       # every stdlib module known to the build
        mode ∈ {granular, monolithic}
OUTPUT: bindings: Map<(wanter, C) → provider_symbol>,
        reachable: Set<symbol>             # the DCE keep-set (R2)

# present_universe is the only mode-dependent input (§5.1):
present_universe = (mode == monolithic) ? all_modules
                                        : modules_directly_used_by(P)   # seeds

reachable = reachable_symbols_from(main_of(P))   # ignoring `wants` edges for now
present   = modules_touched_by(reachable) ∩ present_universe
bindings  = {}

repeat until fixpoint (neither `bindings` nor `reachable` changes):
    # (a) resolve every wanted capability given the modules now present
    for each `wants(C, range)` edge w that is reachable:
        provider = choose_provider(C, range, present, present_universe)   # §4.3/4.4
        bindings[w] = provider                # may be w's own fallback
    # (b) recompute reachability THROUGH the chosen bindings
    reachable = reachable_symbols_from(main_of(P), follow = bindings)
    present   = modules_touched_by(reachable) ∩ present_universe
    # in granular mode `present` may have grown (a) -> recompute (a); loop.

assert no `wants` edge in `reachable` is unbound        # totality (4.5)
return bindings, reachable
```

Monotonicity guarantees termination: `reachable` and `present` only ever **grow**
(a binding is never retracted once an edge is reachable — see 4.4 stability), and
both are bounded by the finite universe of symbols/modules. Each iteration is
O(edges); iterations ≤ number of modules. The whole thing is a classic worklist
least-fixpoint; an efficient implementation uses a queue of "newly reachable wants"
and "newly present modules" instead of re-scanning.

### 4.3 Candidate selection — who *can* serve a `wants(C, range)`

`choose_provider` first computes the **candidate set** for `C` under the current
`present` modules:

```
candidates(C, range, present) =
    { canonical providers of C in `present` whose version ∈ range }      # tier 1
  ∪ { fallback providers of C in `present` whose version ∈ range }       # tier 2
  ∪ { the wanter's OWN fallback }                                        # tier 3 (always)
```

- A candidate MUST have passed `conform(C)` (§3.4) — guaranteed at module build
  time, so by here every candidate is substitutable.
- Version range matching uses the capability's own semver line (§2.4), never the
  module's.
- Tier 3 (the wanter's own fallback) is **always** a candidate; this is what makes
  resolution *total* (4.5) and independence-preserving: there is always at least
  one legal choice that adds no external edge.

### 4.4 Provider precedence — making the choice deterministic (R6)

Among candidates the resolver applies a **total order** and takes the maximum. It is
purely ordinal — no size/cost model, no search (that would break R6) — in this
priority sequence:

1. **Presence gate (the mode, made local).** A candidate whose module is in the
   current **present-set** outranks one whose module is not. The present-set is the
   mode (§5.1): in **granular** it is "already reachable" (the growing set); in
   **monolithic** it is "all modules" (so the gate is vacuous). *This single rule is
   what makes R3 literal:* in granular, the resolver **deduplicates only onto modules
   that are already in the binary for an ordinary reason** — it never drags a new
   module in just to fold a fallback. Pulling a new provider in is **opt-in** (an
   explicit `--stdlib-pin`, §6.5), never a silent size regression.
2. **Tier**: canonical (1) > fallback-provider (2) > own-fallback (3). *Among equally
   present candidates, prefer the purpose-built implementation, then sharing over a
   private copy.* (The wanter's own-fallback is always present, so the gate never
   strands a `wants` — totality, §4.5.)
3. **Declared `priority`** (higher wins) — author's tie-break between two equally
   present, equally canonical providers.
4. **Stable identity** — lexicographic order of `(module_name, version, symbol)` as
   the final, total tie-break. This makes the outcome independent of map iteration
   order, discovery order, or parallelism (R6).

The practical upshot: **in granular default mode the set of modules in the binary is
exactly the set reachable by ordinary `use`/calls; capabilities only fold duplication
*within* that set.** Monolithic flips the gate (all present) so canonical providers
always win — the "free interdependence" of that mode — at the cost of a larger
candidate universe (still DCE-stripped to what is reached).

**Binding stability (needed for termination, 4.2).** Once an edge is bound and the
chosen provider is reachable, re-resolving in a later iteration MUST yield the same
choice *or a strictly higher one in the order* (e.g. a canonical provider became
present). The order is designed so additions to `present` can only **upgrade** a
binding (own-fallback → fallback-provider → canonical), never oscillate. Upgrades
are monotone and bounded, so the fixpoint converges. An implementation MAY simplify
by resolving in two settled phases: first grow `present` to its fixpoint using
tier-3-permissive reachability, then assign final bindings once — equivalent result,
no oscillation.

### 4.5 Totality and failure modes

- **Totality.** Because tier 3 always exists, every reachable `wants` is bindable;
  resolution never "fails to find an implementation". This is a direct consequence
  of R1's fallback requirement and is asserted at the end of 4.2.
- **Version gap.** If a wanter requires `C ^2` but only `C v1` providers (and its
  own `v?` fallback) are present, candidates are filtered by range; if the fallback
  itself is `v1`, the wanter is mis-declared (it shipped a `^2` want with a `v1`
  fallback) — a **module build error**, caught before resolution (the fallback's
  declared version must satisfy the want's own range; §2.3 invariant).
- **Ambiguous canonical providers.** Two canonical providers of the same `C`,
  same tier, same `priority`, both in range: the §4.4 step-4 stable-identity
  tie-break makes it deterministic, **but** the build SHOULD emit a *warning*
  ("capability C has 2 equal canonical providers; bound X by name order; set
  `priority` to disambiguate") because silent name-order dependence is a
  maintainability trap.
- **Capability cycles.** `A.wants(C)`→`B::c`, and `B.wants(D)`→`A::d`: the
  fixpoint handles cycles fine (it grows a set; cycles just mean both become
  present together). A *pathological* cycle where binding choices depend on each
  other is broken by tier-3: the resolver can always fall back to private
  implementations to cut a cycle, then upgrade monotonically. No deadlock.

### 4.6 What the resolver is **not**

The resolver is a **deterministic constraint solver**, not a size/speed optimizer.
It does not search for the globally smallest binary (that is NP-hard in general and
non-reproducible in practice). It applies a fixed, total preference order and the
mode's size policy (§5.4). "Best result" comes from good capability design and good
provider precedence, not from an optimizer that might pick differently across runs
(which would violate R6). If finer size control is wanted, it is exposed as explicit
knobs (pin a provider, exclude a module), never as opaque search.

---

## Part 5 — Build modes, pipeline integration, ABI

### 5.1 The two modes are one resolver with two present-sets

The cleanest insight of this design: **granular and monolithic are not two code
paths.** They are the *same* resolver (§4) run with a different `present_universe`:

| | `present_universe` | Effect |
| --- | --- | --- |
| **monolithic** (default) | **all** stdlib modules | Every canonical provider is present ⇒ every `wants` binds to a canonical provider (tier 1) ⇒ no fallback is ever compiled ⇒ modules "freely interdepend". Optimization policy favors cross-module inlining/LTO. Target: PC/Web. |
| **granular** (`--stdlib granular`) | only modules **reachable** from `main` (grown to fixpoint) | Fewer providers present ⇒ fallbacks fill the gaps, and co-present modules deduplicate via fallback-promotion ⇒ smallest artifact. Optimization policy favors out-of-line shared symbols + aggressive DCE. Target: Pico/kernels. |

This unification is worth defending: it means **one source of truth** (the modular
stdlib with capabilities) and **one mechanism** (the resolver), so the two targets
can never silently diverge — which is precisely what R5 demands. Maintaining a
separate "big stdlib" for desktop would reintroduce the `std`/`no_std` fork we
rejected.

Consequences:

- A program that uses few modules gets a tiny binary in **both** modes (R2 holds
  in monolithic too — DCE still strips unreached symbols). The difference is that
  monolithic *allows* a module to reach for any other directly (because all
  providers are present), while granular *forces* the independence discipline to
  pay off as size.
- Because both modes pass the same conformance gate (§3.4) and the resolver is
  deterministic (§4.4), R5 (granular ≡ monolithic) is structural, not tested
  per-program. (We still add a CI check that builds key programs both ways and
  diff-tests them — defense in depth, see §6.3.)

### 5.2 Where it sits in the compiler pipeline

Resolution is a **whole-program MIR→MIR pass**. The ordering constraints are firm:

```
HIR → MIR lowering (3.2/3.3/3.4)             # per-function CFGs
   → ownership lowering: drops (3.5)         # per-function; capability sig fixes drop duties
   → [PHASE 12] reachability + capability resolution (fixpoint, §4)
        · seed from main; grow present-set; bind every reachable `wants C`
        · rewrite each `wants C` MIR Call's callee → the bound provider symbol
        · produce the DCE keep-set
   → monomorphization (7.1)                   # specialize now-direct generic calls
   → DCE: drop every MIR fn not in the keep-set
   → LLVM codegen (Phase 5) → link (Phase 11)
```

Why this order:

- **Resolution before monomorphization.** The resolver rewrites *generic* `wants C`
  call sites to a generic provider symbol; 7.1 then specializes the direct calls.
  Resolving after mono would mean rewriting N specialized call sites instead of one
  generic edge, and would entangle two whole-program passes.
- **Resolution after drop insertion (3.5) in principle, but drop duties come from
  the capability *signature*, not the provider.** Because the capability fixes who
  owns/drops what (P-3), drop insertion can run against the *signature* before the
  provider is known; the bound provider must honour it. (If a future provider needed
  different drop placement, that would be an observable difference — forbidden by
  P-2/R5. So drop placement is provider-independent by construction.)
- **DCE consumes the resolver's keep-set**, so dead fallbacks and unreached modules
  never reach codegen.

The `validate` pass on MIR (added in 3.1) MUST run after resolution rewrites call
sites (no dangling callee symbols) and after DCE (no references into deleted fns).

### 5.3 Two implementation strategies for binding (and why we pick the compiler one)

**Strategy A — compiler-level call-site rewrite (chosen).** The resolver rewrites
the MIR `Call.func` of each `wants C` site to the bound provider's symbol, then DCE
deletes unbound fallbacks. Pros: total control, deterministic, enables the bound
call to be **inlined** (granular can still inline a shared tiny accessor; monolithic
inlines aggressively), no reliance on linker features. Cons: we own the whole
mechanism. This is the primary design.

**Strategy B — linker-level weak symbols / COMDAT folding (complement, not core).**
Emit each fallback as a `weak` symbol named by capability; emit canonical providers
as `strong`; let the linker prefer strong and `--gc-sections` drop the rest; let
identical-code-folding (ICF) merge byte-identical fallbacks. Pros: cheap, reuses
mature linker machinery. Cons: coarser (symbol granularity only), behaviour depends
on linker flags/versions (R6 risk), no inlining across the weak boundary, and ICF
only folds *byte-identical* code (misses semantically-equal-but-different
implementations that the capability contract says are interchangeable). We MAY use
B as a **second-line** size win *under* A (e.g. fold leftover identical drop glue),
but A is the source of truth so that R5/R6 never depend on linker behaviour.

### 5.4 The mode's size/speed policy

Resolution decides *which* implementation; a separate **policy** decides
inline-vs-outline and how hard to chase size:

- **granular policy:** prefer one out-of-line copy of a shared provider over
  inlining it into many call sites (size); enable `-Oz`-style choices, section GC,
  and Strategy-B ICF as a finishing pass; never duplicate a provider to inline it
  unless it is below a tiny size threshold.
- **monolithic policy:** prefer inlining and cross-module LTO (speed); duplication
  for inlining is acceptable; DCE still runs but size is not the objective.

Crucially, **policy never changes observable behaviour** — only size/speed. So R5
is preserved across policies just as it is across modes.

### 5.5 ABI of a capability

For Strategy A, a capability call lowers to an ordinary direct call once bound, so
there is no special runtime ABI — the provider's symbol is called with the
capability signature's calling convention. The layout question is answered by the
§3.6 keystone (capability = implicit interface parameter, wanter monomorphized
against the provider):

- **Opaque capability types are concrete after mono.** `type Buf` has no fixed
  layout at *source* time; it gets the chosen provider's concrete layout when the
  wanter is **monomorphized against that provider** (binding precedes mono, §5.2).
  So a wanter MAY hold `Buf` by value (stack, struct fields) at zero cost — its own
  layout simply *depends on the bound provider*, exactly as a Rust generic's layout
  depends on its type argument.
- **Provider choice may change the wanter's layout across builds — and that is
  fine.** Within a single build there is exactly one bound provider (§4.4), so the
  wanter is monomorphized once; there is no in-build ambiguity. Across builds
  (granular vs monolithic, different module sets) the wanter may be specialized to a
  different `Buf` and thus laid out differently — but **R5 constrains behaviour, not
  layout**, and substitutability (§3.4) guarantees the behaviour is identical. (If a
  wanter must *not* be re-laid-out across builds — e.g. it crosses a stable binary
  ABI boundary — it MUST take the capability type behind a reference in its own
  signature; that is an explicit author choice, not the default.)
- **Ownership crosses the boundary exactly as the signature declares (P-3):**
  `new() -> Buf` transfers ownership to the wanter; `drop(b: Buf)` consumes it. The
  borrow checker (1.6) type-checks the wanter against the capability *signature*, so
  a provider swap cannot introduce a use-after-move regardless of the provider's
  internal representation.

### 5.6 The build report (observability)

Granular builds are size-sensitive; the developer must be able to *see* what was
pulled in and folded. The driver (Phase 11.1) emits, on request (`--stdlib-report`):

```
module set (present):   text v1.3.0, collections v2.1.0          [2 of 41 known]
capabilities resolved:
  growable_buffer ^1   → collections::Buf            (canonical, folded text's fallback)
  utf8_decode v1       → text::decode                (own provider)
  sort ^1              → text::_sort_fallback         (own fallback; no provider present)
dead-code eliminated:   312 fns, 18 KiB
fallbacks dropped:      1   (text::_buf_new, folded onto collections)
final .text size:       11.4 KiB
```

This report is also the **audit trail** for R3 ("did sharing only ever remove
duplication?") and for debugging size regressions on the embedded target.

---

## Part 6 — Worked example, hard cases, prior art, open questions

### 6.1 End-to-end worked example

Two modules:

- **`text`** (v1) — provides `utf8_decode`; **wants** `growable_buffer` (ships a
  tiny fallback `_buf_new`, a plain `[u8]`-backed buffer).
- **`collections`** (v2) — **provides** `growable_buffer` (canonical, its core
  `Buf`); wants nothing external.

Program:

```la3
use text
fn main() {
    let parts = text.split(read_line(), ",")   // text.split internally uses growable_buffer
    io.println(parts.len())
}
```

**Granular build** (`present_universe` = reachable from `main`):

1. Seed: `main` uses `text` ⇒ present = {text}. `text.split` is reachable; it has a
   `wants(growable_buffer ^1)` edge.
2. Resolve round 1: present = {text}. Candidates for `growable_buffer`: only
   tier-3 (text's own `_buf_new`). Bind `text.wants(growable_buffer)` → `text::_buf_new`.
3. Reachability through that binding pulls in nothing new. **Fixpoint.** `collections`
   is never present.
4. Result: `text` alone, with its fallback buffer. Tiny. Independent (R1 holds —
   exactly the alone build).

Now the program also `use collections` (say `main` builds a `Map`):

1. Seed: present = {text, collections}.
2. Resolve: candidates for `growable_buffer` now include `collections`'s **canonical**
   provider (tier 1) and text's fallback (tier 3). Precedence (§4.4): canonical wins.
   Bind `text.wants(growable_buffer)` → `collections::Buf`.
3. DCE drops `text::_buf_new` (now unbound). `text` got **smaller** and shares
   `collections`'s buffer — duplication removed, no new alone-dependency created
   (R3 ✓).
4. Behaviour identical to the alone case because both `Buf` implementations passed
   `conform(growable_buffer)` (R5 ✓).

**Monolithic build** of the *first* program (`use text` only): `present_universe` =
all 41 modules, so `collections` is present even though `main` doesn't use a `Map`.
`text.wants(growable_buffer)` binds to `collections::Buf` (canonical). DCE still
strips everything in `collections` except the `Buf` provider actually reached. Same
behaviour; slightly different size/shape than granular — both correct (R5 ✓).

### 6.2 Hard cases, faced head-on (nothing-but-the-impossible-is-feared)

- **Capability that is itself heavy.** If the canonical `hashmap` provider is large,
  granular's §4.4 **presence gate** prevents pulling it in *just* to dedupe a tiny
  fallback: a canonical provider only wins if its module is *already present* for an
  ordinary reason. Otherwise the wanter keeps its small fallback. So R3's "only
  remove duplication already present" is enforced by construction — pulling the big
  provider in is an explicit opt-in (`--stdlib-pin`), never a silent regression.
- **Diamond providers.** Two canonical providers of `sort` (a `collections` quicksort
  and a `dsp` radix sort), both present, both in range. Resolution is deterministic
  via priority then stable identity (§4.4), and the build *warns* so the author sets
  an explicit `priority` or pins one. No silent nondeterminism.
- **Capability cycle.** `crypto` wants `bignum`; `bignum` wants `rng` (for blinding);
  `rng` wants `crypto` (for a CSPRNG). The fixpoint converges because each has a
  fallback (tier 3) that breaks the cycle; once all three are present they upgrade to
  each other's canonical providers monotonically. Worst case: three fallbacks (still
  correct, just bigger) — never a deadlock or a build failure.
- **Provider that is *almost* conformant.** Forbidden to exist: it fails `conform(C)`
  and cannot ship. There is no "90% compatible" provider — substitutability is binary.
  This is strict on purpose; it is the load-bearing wall under R5.
- **Inlining duplicating a "shared" provider.** Allowed only under the monolithic
  size policy (where duplication-for-speed is fine) or below a tiny threshold in
  granular. Never changes behaviour.
- **A module that wants to *observe* which provider it got.** Forbidden by P-2 (no
  observable behaviour outside the contract). If a module needs provider-specific
  behaviour, that behaviour belongs *in the capability contract* (and its suite), or
  it is not a capability at all.

### 6.3 Verification strategy (how we keep ourselves honest)

- **Per-module compile-alone gate** (§2.2) ⇒ R1.
- **Per-capability conformance suite** run against every fallback and provider
  (§3.4) ⇒ substitutability ⇒ R5 by construction.
- **Resolver determinism tests**: same inputs ⇒ identical bindings + identical
  artifact hash (R6); fuzz the present-set and assert the total order never cycles.
- **Cross-mode differential**: build a corpus of programs both granular and
  monolithic; run both; assert identical observable output (defense-in-depth for R5).
- **Reachability/DCE soundness**: every symbol kept is reachable; every symbol
  dropped is unreachable (no false drop). Tie this to MIR `validate` (§5.2).

### 6.4 Prior art (and where La3 goes further)

- **Zig** lazily compiles only referenced declarations — the closest existing thing
  to R2. La3 adopts that *and* adds the capability/fallback layer for R1+R3 (Zig has
  no independence invariant or opportunistic cross-library sharing contract).
- **Linker `--gc-sections` + ICF / COMDAT folding** — our Strategy B (§5.3), used as
  a finishing pass only; we do the semantic decision earlier (Strategy A).
- **Rust `no_std`/`alloc`/feature flags** — the manual, coarse, combinatorial model
  we explicitly reject (§1.3); capabilities replace feature flags with *automatic,
  contract-checked* resolution.
- **Weak symbols (ELF) / `__attribute__((weak))`** — the mechanism behind Strategy B;
  insufficient alone (byte-identity only, linker-version-dependent).
- **Swift embedded / SwiftPM traits, Nix overlays** — partial analogues of
  configurable surfaces; none provide the alone-build guarantee + substitutability
  contract together.

The genuinely novel combination is: **(independence as a checked per-module
invariant) + (usage-driven inclusion) + (opportunistic sharing made sound by a
per-capability conformance contract) + (two modes unified as one resolver over two
present-sets).** Each piece exists somewhere; the set, with R5/R6 guaranteed, is new.

### 6.5 Open design questions (to settle when Phase 12 starts)

1. **Annotation syntax** (`@wants/@provides/@fallback`, `capability …` blocks) —
   pinned against the real grammar; this doc's syntax is provisional.
2. **Capability registry** — central file vs distributed declaration; how the build
   discovers all capabilities and their suites deterministically.
3. **The opt-in "pull" knob** — the default (§4.4) never pulls a new module just to
   dedupe. If a build *wants* to (trade size for sharing/quality), `--stdlib-pin C=mod`
   forces a provider present. Open: whether to also offer a deterministic
   *auto-pull* policy (e.g. "pull if provider size < summed dropped-fallback size")
   as an explicit, reproducible mode — useful, but must stay a fixed rule, never a
   search (§4.6).
4. **Granularity of the present-set in granular mode** — module-level (simpler) vs
   allowing a *single* provider symbol from an otherwise-absent module (smaller, but
   complicates "module present"). Lean module-level first.
5. **User overrides** — `--stdlib-pin C=collections`, `--stdlib-exclude dsp`; how
   they interact with totality (4.5) and the report (5.6).
6. **Interaction with the interpreter oracle** — the interpreter must expose the
   same capability *behaviour* (it already has one implementation of each facility);
   conformance suites should run against the interpreter too, so the oracle and every
   compiled provider agree (ties into `la3-oracle-over-reference`).
7. **Cross-language providers** — capabilities backed by `extern "C"` runtime
   (Phase 4): same contract, conformance suite runs against the C implementation.

### 6.6 Glossary cross-reference

See §1.4 for the canonical definitions of *module, capability, wanter, fallback,
provider (canonical / fallback), present-set, resolution, binding, conformance
suite*. Key invariants by number: **R1** independence, **R2** usage-driven inclusion,
**R3** opportunistic-only sharing, **R4** two modes, **R5** behavioural identity,
**R6** determinism.

---

*End of design. This document is the Phase 12 specification; implementation is
deferred (no rush) and will pin the provisional syntax and the open questions in
§6.5 when the phase begins.*





