# Verbose — Architecture Complète

Ce document explique **tout** ce que le compilateur fait, comment, et pourquoi.
Écrit pour quelqu'un qui découvre le projet (y compris le créateur après un doliprane).

---

## La Vue d'Ensemble

```
                    CE QUE L'HUMAIN ÉCRIT
                    ─────────────────────
                    factures.intent
                    "Une facture est importante
                     quand son montant dépasse 10000"
                              │
                              ▼
                    CE QUE L'IA GÉNÈRE
                    ──────────────────
                    factures.verbose
                    concept + rule + proofs + hints
                              │
                              ▼
              ┌───────────────────────────────┐
              │        VERBOSEC (le compilo)  │
              │                               │
              │  1. LEXER     texte → tokens  │
              │  2. PARSER    tokens → AST    │
              │  3. RESOLVER  imports fusionnés│
              │  4. VERIFIER  preuves checkées│
              │  5. OPTIMIZER AST simplifié   │
              │  6. BACKEND   code final      │
              │                               │
              └───────┬───┬───┬───┬───────────┘
                      │   │   │   │
                      ▼   ▼   ▼   ▼
                   Interp Rust x86  WASM
                   JSON  441KB 570B  60B
```

---

## Les 6 Étapes du Compilateur

### Étape 1 : LEXER (src/lexer.rs)

**Rôle :** Transformer du texte en "tokens" (mots reconnus).

```
Entrée:  "rule important\n  logic:\n    x = i.amount > 10000\n"
Sortie:  [Ident("rule"), Ident("important"), NEWLINE, INDENT,
          Ident("logic"), Colon, NEWLINE, INDENT,
          Ident("x"), Equal, Ident("i"), Dot, Ident("amount"),
          Gt, Number(10000), NEWLINE, DEDENT, DEDENT, EOF]
```

**Point clé :** L'indentation est significative (comme Python). Le lexer émet
des tokens `INDENT` et `DEDENT` pour marquer les blocs. Pas de `{` `}`.

### Étape 2 : PARSER (src/parser.rs)

**Rôle :** Transformer les tokens en un arbre (AST = Abstract Syntax Tree).

```
Tokens:  Ident("i") Dot Ident("amount") Gt Number(10000)
AST:     Binary(Gt,
           Field(Ident("i"), "amount"),
           Number(10000))
```

L'AST est un arbre typé : chaque nœud sait ce qu'il est (nombre, champ,
comparaison, appel de règle, if/else, quantificateur...).

**Priorité des opérateurs (du plus faible au plus fort) :**
```
or → and → comparaisons (> < >= <= == !=) → add/sub (+, -) → mul/div/mod (*, /, %) → unary (not, -) → primary (nombre, champ, appel, parenthèses)
```

### Étape 3 : RESOLVER (dans src/main.rs)

**Rôle :** Charger les fichiers importés (`use "stdlib/finance.verbose"`)
et fusionner tous les concepts/règles en un seul programme.

```
app.verbose:               stdlib/finance.verbose:
  use "stdlib/finance"       concept Invoice { ... }
  rule my_rule { ... }       rule standard_tax { ... }
         │                            │
         └────────────┬───────────────┘
                      ▼
              Programme unifié:
                concept Invoice
                rule standard_tax
                rule my_rule
```

Gestion des imports circulaires : un fichier déjà chargé est ignoré.

### Étape 4 : VERIFIER (src/verifier.rs)

**Rôle :** Vérifier que les preuves de l'IA sont vraies. **ZERO TRUST.**

```
L'IA déclare:          Le vérificateur vérifie:
  reads: [i.amount]  →  parcourt l'AST, liste tous les accès → [i.amount] ✓
  writes: []         →  aucune mutation dans le code → [] ✓
  calls: []          →  aucun appel de fonction → [] ✓
  verdict: pure      →  writes=[] ET calls=[] → pure ✓
  bound: 1           →  compte les opérations: 1 (Gt) → 1 ≤ 1 ✓
  overflow: [0, 100] →  interval arithmetic: [0, 100] ⊆ [0, 100] ✓
```

**Les 10+ vérifications :**

| Check | Ce qu'il vérifie |
|-------|-----------------|
| reads match | Les champs déclarés = les champs réellement lus dans l'AST |
| writes match | Les mutations déclarées = les mutations réelles (doit être vide pour pure) |
| calls match | Les appels déclarés = les appels réels dans l'AST |
| verdict coherent | pure ↔ writes=[] et calls=[] |
| termination bound | Le nombre déclaré ≥ le nombre réel d'opérations |
| determinism | total ↔ pas d'appels non-déterministes |
| source exists | @source: file:line → le fichier et la ligne existent |
| field exists | Chaque champ accédé (i.amount) existe sur le concept |
| target matches | Le target de logic = le nom déclaré dans output |
| called rules exist | Chaque règle appelée existe dans le programme |
| hint valid | vectorizable ↔ pure et pas d'appels |
| overflow bounds | Interval arithmetic prouve que les bornes sont respectées |

**Interval arithmetic (pour overflow et dead code) :**
```
Expression: i.amount * i.tax_rate / 100
Field ranges: amount ∈ [0, 10000000], tax_rate ∈ [0, 100]

Calcul:
  amount * tax_rate → [0*0, 10000000*100] = [0, 1000000000]
  / 100             → [0/100, 1000000000/100] = [0, 10000000]

Résultat: [0, 10000000]
Si l'IA déclare overflow: [0, 10000000] → ✓ accepté
Si l'IA déclare overflow: [0, 1000]     → ✗ rejeté (computed > declared)
```

### Étape 5 : OPTIMIZER (src/optimizer.rs)

**Rôle :** Simplifier l'AST. **Indépendant de la plateforme** — bénéficie
à TOUS les backends (x86, WASM, Rust, futur ARM).

```
Avant:   Binary(Add, Number(100), Number(20))     → Après: Number(120)
Avant:   Binary(Mul, Field(i, x), Number(0))       → Après: Number(0)
Avant:   Binary(Mul, Field(i, x), Number(1))       → Après: Field(i, x)
Avant:   Not(Not(expr))                             → Après: expr
Avant:   If(always_false_cond, then, else)          → Après: else
```

**Optimisations universelles :**
- Constant folding : 100 + 20 → 120
- Identités algébriques : x*0→0, x*1→x, x+0→x
- Double négation : not not x → x
- Dead code : if(impossible) then A else B → B

### Étape 6 : BACKENDS (4 options)

```
                        AST optimisé
                             │
              ┌──────┬───────┼────────┬─────────┐
              ▼      ▼       ▼        ▼         ▼
          Interpréteur  Rust    x86-64    WASM    (futur ARM)
          src/         src/    src/      src/
          interpreter  codegen native    wasm
          .rs          .rs     .rs       .rs
```

#### Backend Interpréteur (src/interpreter.rs)
- Lit du JSON, évalue les expressions directement
- Le plus simple, le plus flexible
- Supporte TOUT (collections, quantificateurs, réactions)

#### Backend Rust (src/codegen.rs)
- Génère du code source Rust, appelle `rustc`
- Binaire ~441 KB (inclut la libc Rust)
- Supporte tout sauf les quantificateurs

#### Backend Natif x86-64 (src/native.rs)
- Émet des octets machine DIRECTEMENT dans un ELF
- Binaire ~400-700 bytes, zéro dépendances
- Optimisations platform-specific :

```
Optimisation          │ Instruction émise      │ Gain
──────────────────────┼────────────────────────┼──────────────
SIMD (vectorizable)   │ pcmpgtq (SSE4.2)      │ 2 valeurs/cycle
Fork (parallel)       │ syscall 57 (fork)      │ 2 cœurs CPU
Magic division (/ N)  │ mul + shr              │ 4 cycles vs 40
Shift (* 2^n)         │ shl                    │ 1 cycle vs 3
Dead branch           │ (supprimé)             │ 0 instruction
Constant              │ mov rax, valeur        │ 0 calcul
```

#### Backend WASM (src/wasm.rs)
- Émet un module WebAssembly binaire
- ~60 bytes, tourne dans les navigateurs
- Machine à pile (pas de registres, plus simple que x86)

---

## Les Deux Types de Blocs

```
┌─────────────────────────────────────┐
│  RULE (pur)                         │
│                                     │
│  Entrée → Calcul → Sortie          │
│  Pas d'effets de bord              │
│  Le compilateur optimise            │
│  SIMD, fork, dead code, tout       │
│                                     │
│  Exemple:                           │
│    important = i.amount > 10000     │
└─────────────────────────────────────┘
            │
            │ trigger
            ▼
┌─────────────────────────────────────┐
│  REACTION (effets déclarés)         │
│                                     │
│  Écoute un trigger (rule)           │
│  Si vrai → exécute les effets       │
│  Effets listés explicitement        │
│  Le compilateur vérifie, pas        │
│  d'effet caché                      │
│                                     │
│  Exemple:                           │
│    trigger: important_invoice       │
│    effects:                         │
│      print "ALERT: Important!"      │
└─────────────────────────────────────┘
```

---

## Les Fichiers du Projet

```
src/
  main.rs          Point d'entrée CLI, résolution des imports, dispatch
  lexer.rs         Texte → Tokens (avec INDENT/DEDENT)
  parser.rs        Tokens → AST (recursive descent)
  ast.rs           Tous les types de l'AST (Expr, Rule, Concept, Reaction...)
  verifier.rs      Vérification zero-trust + interval arithmetic
  optimizer.rs     Optimisations universelles (constant fold, dead code...)
  interpreter.rs   Évaluation directe sur JSON
  codegen.rs       Génération de code Rust
  native.rs        Émission de code machine x86-64 + ELF builder
  wasm.rs          Émission de modules WebAssembly
  validate_x86.rs  Auto-vérification du code machine émis

examples/
  invoices.*       Exemple minimal (1 concept, 1 rule)
  business.*       Arithmétique + composition (4 rules, let bindings)
  clients.*        Type text + comparaison de chaînes
  collections.*    Quantificateurs all/any avec lambdas
  pricing.*        If/else imbriqués + let bindings
  deadcode.*       Démonstration d'élimination de branches mortes
  showcase.*       TOUTES les features en un scénario cohérent
  reactions.*      Première réaction (effets de bord déclarés)
  app.* + stdlib/  Système de modules (use + import)
  demo.html        Démo navigateur (WASM)
```

---

## Le Pipeline Complet en Un Schéma

```
HUMAIN                    IA                      COMPILATEUR
──────                    ──                      ───────────

"Une facture est    →   concept Invoice         →  LEXER
 importante quand       rule important_invoice      ↓
 son montant            proofs: pure, bound=1    PARSER
 dépasse 10000"         hints: vectorizable         ↓
                                                 RESOLVER (imports)
 factures.intent        factures.verbose            ↓
                                                 VERIFIER
                                                   reads ✓
                                                   purity ✓
                                                   bound ✓
                                                   overflow ✓
                                                    ↓
                                                 OPTIMIZER
                                                   constant fold
                                                   dead code
                                                    ↓
                                                 BACKEND
                                               ┌────┼────┐
                                               ▼    ▼    ▼
                                             x86  WASM  Rust
                                             570B  60B  441KB
```

---

## Glossaire

| Terme | Signification |
|-------|---------------|
| AST | Abstract Syntax Tree — l'arbre qui représente le code en mémoire |
| Token | Un mot reconnu par le lexer (nombre, identifiant, opérateur) |
| INDENT/DEDENT | Tokens émis quand l'indentation augmente/diminue |
| Recursive descent | Technique de parsing : chaque règle de grammaire = une fonction |
| Zero trust | Le compilateur ne fait jamais confiance — il vérifie tout |
| Interval arithmetic | Calcul de bornes : [min, max] pour chaque sous-expression |
| Constant folding | Calculer 100+20=120 à la compilation, pas à l'exécution |
| Dead code | Code qui ne sera jamais exécuté (branche impossible) |
| Strength reduction | Remplacer une opération lente par une rapide (×4 → shift) |
| Magic division | Remplacer ÷100 par ×magic_number>>shift (4 cycles vs 40) |
| SIMD | Single Instruction Multiple Data — traiter 2+ valeurs en un coup |
| ELF | Format de binaire Linux (Executable and Linkable Format) |
| WASM | WebAssembly — bytecode qui tourne dans les navigateurs |
| Peephole | Optimisation locale : scanner le code émis pour des patterns inutiles |
| CSE | Common Subexpression Elimination — calculer une fois, réutiliser |
| Pureté | Pas d'effets de bord — le résultat dépend uniquement des entrées |
| Réaction | Bloc avec effets de bord déclarés (print, write, send) |
| Trigger | La règle pure qui déclenche une réaction |
| LEB128 | Encodage d'entiers à taille variable (utilisé dans WASM) |

---

*Ce document est généré pour le projet Verbose v0.1.0 — 42 commits, ~7000 lignes, 72 tests, 4 backends.*
