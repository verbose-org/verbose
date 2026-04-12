# Verbose IR — Spécification du Format Intermédiaire
## Version 0.1.0 — Draft

---

## 1. Principes de Design

Le Verbose IR est conçu selon trois axiomes :

1. **Rien n'est implicite.** Chaque bloc porte toute l'information nécessaire à sa compréhension et à son optimisation. Aucune analyse inter-blocs n'est requise pour optimiser un bloc individuel.
2. **L'intention survit.** Chaque élément du IR est traçable vers l'intention humaine d'origine. Le chemin inverse (binaire → IR → intention) est toujours possible.
3. **Le compilateur ne devine jamais.** Chaque décision d'optimisation est soutenue par une preuve ou une déclaration explicite dans l'IR.

---

## 2. Structure Globale d'un Programme Verbose

Un programme Verbose IR est un document structuré en sections ordonnées :

```verbose
@verbose 0.1.0

-- Métadonnées du programme --
programme "gestion-commerciale"
  version: "1.0.0"
  auteur_ir: "claude-opus-4-6"
  source_intention: "spec-client-factures.intent"
  hash_intention: sha256:a3f2...

-- Contexte d'exécution --
cible
  architecture: x86_64
  os: linux
  priorité: latence
  mémoire: abondante
  sécurité: standard
  concurrence: multi-coeur(8)

-- Domaines utilisés --
utilise domaine::temps (date, durée, maintenant)
utilise domaine::collection (ensemble, tout, filtre)
utilise domaine::notification (alerte)

-- Concepts --
[... blocs concept ...]

-- Règles --
[... blocs règle ...]

-- Réactions --
[... blocs réaction ...]

-- Contraintes --
[... blocs contrainte ...]
```

---

## 3. Les Blocs — Unité Fondamentale du Verbose IR

Chaque bloc est une unité autonome et auto-documentée. Voici la structure canonique :

### 3.1. Bloc Concept (données)

Définit une entité du domaine.

```verbose
concept Client
  @intention: "Un client est une entreprise ou une personne qui achète nos services"
  @source: ligne 1 de spec-client-factures.intent

  champs:
    nom         : texte, non-nul, immuable-après-création
    email       : texte, non-nul, format(email)
    commercial  : référence(Commercial), non-nul
    factures    : collection(Facture), propriétaire
    bloqué      : booléen, dérivé  -- calculé par une règle, jamais assigné

  accès_mémoire:
    lecture_dominante: vrai        -- lu beaucoup plus souvent qu'écrit
    taille_typique: 50-500         -- nombre d'instances attendues
    localité: regrouper(nom, bloqué, factures)  -- ces champs sont accédés ensemble

  relations:
    Client --possède--> Facture    : 1 vers N, propriétaire
    Client --assigné--> Commercial : N vers 1, référence
```

**Pourquoi c'est verbeux :** En C ou Rust, tu écrirais une struct de 5 lignes. Ici, le compilateur sait que `bloqué` est dérivé (pas besoin de le stocker, on peut le recalculer), que les données sont lues bien plus qu'écrites (optimisation pour la lecture), et quels champs sont accédés ensemble (optimisation du layout mémoire pour le cache).

---

### 3.2. Bloc Règle (logique)

Définit une vérité dérivée du domaine.

```verbose
règle facture_en_retard
  @intention: "Une facture est en retard quand elle a plus de 30 jours"
  @source: ligne 3 de spec-client-factures.intent
  @priorité_évaluation: haute  -- utilisée par d'autres règles

  entrée:
    f: Facture
      accès: lecture(f.date_emission, f.statut)
      pattern: séquentiel  -- on itère sur toutes les factures

  sortie:
    f.en_retard: booléen

  logique:
    f.en_retard = (maintenant() - f.date_emission) > 30.jours

  preuves:
    pureté: oui
      justification: "aucune mutation, aucun effet de bord, seule lecture de f et de l'horloge"
      exception: maintenant() est non-déterministe
      mitigation: évaluation unique par cycle d'exécution, résultat mis en cache

    déterminisme: conditionnel
      condition: "déterministe si maintenant() est fixé pour le cycle"

    terminaison: oui
      justification: "opération arithmétique et comparaison, pas de récursion, pas de boucle"

    complexité: O(1) par facture, O(n) pour l'ensemble

  tests_dérivés:
    - entrée: { f.date_emission: maintenant() - 31.jours } → attendu: vrai
    - entrée: { f.date_emission: maintenant() - 30.jours } → attendu: faux
    - entrée: { f.date_emission: maintenant() - 29.jours } → attendu: faux
    - entrée: { f.date_emission: maintenant() }             → attendu: faux
    - entrée: { f.date_emission: maintenant() - 365.jours } → attendu: vrai

  optimisation_hints:
    vectorisable: oui  -- opération identique sur chaque facture, indépendante
    parallélisable: oui  -- aucune dépendance inter-factures
    cache_résultat: oui, durée: 1.cycle
    instruction_suggérée: comparaison SIMD si > 16 factures
```

**Pourquoi c'est verbeux :** Le compilateur classique verrait `(now - date) > 30` et devrait deviner si c'est parallélisable. Ici, c'est prouvé : pure, sans dépendances, vectorisable. Le compilateur peut directement émettre des instructions SIMD sans analyse.

---

### 3.3. Bloc Règle Composée

Quand une règle dépend d'autres règles.

```verbose
règle client_bloqué
  @intention: "Un client est bloqué si toutes ses factures sont en retard"
  @source: ligne 4 de spec-client-factures.intent
  @dépendances: [facture_en_retard]

  entrée:
    c: Client
      accès: lecture(c.factures)
      traverse: c.factures → chaque f → f.en_retard

  sortie:
    c.bloqué: booléen

  logique:
    c.bloqué = tout(c.factures, f => f.en_retard)

  preuves:
    pureté: oui
      justification: "agrégation en lecture seule sur des valeurs déjà calculées"

    déterminisme: oui
      justification: "dépend uniquement de facture_en_retard qui est mis en cache par cycle"

    terminaison: oui
      justification: "itération bornée sur c.factures, collection finie"
      borne: |c.factures|

    complexité: O(k) où k = |c.factures|

  cas_limites:
    - si c.factures est vide: c.bloqué = vrai
      @avertissement: "tout() sur ensemble vide est vrai par convention logique"
      @décision_intention: "À CONFIRMER avec l'utilisateur — un client sans facture est-il bloqué ?"

  tests_dérivés:
    - entrée: { c.factures: [retard, retard, retard] } → attendu: vrai
    - entrée: { c.factures: [retard, retard, ok] }     → attendu: faux
    - entrée: { c.factures: [ok] }                      → attendu: faux
    - entrée: { c.factures: [] }                        → attendu: vrai (⚠ à confirmer)

  graphe_dépendances:
    facture_en_retard → client_bloqué
    évaluation: facture_en_retard DOIT être résolu avant client_bloqué
    parallélisme: facture_en_retard peut être évalué en parallèle sur toutes les factures
                  PUIS client_bloqué est évalué séquentiellement par client
```

**Point notable :** Le bloc a détecté un cas limite (ensemble vide) et a **flaggé une ambiguïté** à remonter à l'humain. L'IA ne tranche pas, elle signale. C'est le genre de bug silencieux qui existe dans 90% des codebases.

---

### 3.4. Bloc Réaction (effet de bord)

```verbose
réaction sur_client_bloqué
  @intention: "Quand un client devient bloqué, refuser ses nouvelles commandes"
  @source: ligne 5 de spec-client-factures.intent
  @déclencheur: client_bloqué passe de faux à vrai

  entrée:
    c: Client
      accès: lecture(c.nom, c.commercial)
    événement: transition(c.bloqué, faux → vrai)

  effets:
    - type: mutation
      cible: système_commandes
      action: refuser_commandes(c)
      réversible: oui (si c.bloqué repasse à faux)

    - type: notification
      cible: c.commercial
      domaine: domaine::notification
      message: "Le client {c.nom} est bloqué"
      priorité: normale
      canal: email

  preuves:
    pureté: NON — cette réaction a des effets de bord
    effets_déclarés: [mutation(système_commandes), notification(email)]
    effets_non_déclarés: aucun  -- PREUVE : seuls les effets listés sont émis
    idempotence: oui  -- appeler deux fois produit le même résultat
    ordre_critique: non  -- les deux effets sont indépendants

  tests_dérivés:
    - scénario: "client Dupont passe à bloqué"
      vérifie: commandes_refusées(Dupont) = vrai
      vérifie: notification_envoyée(Dupont.commercial, "Le client Dupont est bloqué")

  rollback:
    quand: c.bloqué repasse à faux
    actions:
      - réautoriser_commandes(c)
      - notifier c.commercial "Le client {c.nom} est débloqué"
```

**Pourquoi c'est verbeux :** Le compilateur sait exactement quels effets de bord existent, qu'ils sont indépendants (parallélisables), idempotents (retry safe), et réversibles. En code classique, tout ça serait enfoui dans la logique métier.

---

## 4. Bloc Contrainte (invariants globaux)

```verbose
contrainte intégrité_facture
  @intention: "Le montant d'une facture est toujours strictement positif"
  @source: ligne 7 de spec-client-factures.intent
  @niveau: critique  -- violation = erreur fatale, pas un avertissement

  porte_sur: Facture.montant
  invariant: Facture.montant > 0

  vérification:
    moment: à_la_création, à_la_modification
    coût: O(1)
    peut_être_inlinée: oui  -- pas besoin d'un appel de fonction, juste un check

  en_cas_de_violation:
    action: rejeter_opération
    message: "Tentative de créer une facture avec un montant de {montant}"
    log: niveau erreur
    propagation: aucune  -- on rejette, on ne propage pas un état invalide
```

---

## 5. Métadonnées Globales de Compilation

À la fin du programme, un bloc résume les propriétés globales :

```verbose
résumé_compilation
  blocs_total: 6
  blocs_purs: 3 (facture_en_retard, client_bloqué, intégrité_facture)
  blocs_avec_effets: 1 (sur_client_bloqué)
  effets_déclarés: [mutation(système_commandes), notification(email)]

  graphe_exécution:
    facture_en_retard  →  client_bloqué  →  sur_client_bloqué
    intégrité_facture  →  (garde sur toute mutation de Facture)

  parallélisme_possible:
    - facture_en_retard : parallélisable sur toutes les factures
    - client_bloqué : parallélisable sur tous les clients (après résolution factures)
    - sur_client_bloqué : effets parallélisables entre eux

  estimation_mémoire:
    pour 100 clients, 1000 factures:
      données: ~120 KB
      IR en mémoire: ~2 MB
      pic d'exécution: ~4 MB

  ambiguïtés_non_résolues:
    - client_bloqué / cas_limites[0]: "client sans facture — bloqué ou non ?"
```

---

## 6. Comparaison de Volume

Pour exprimer la même logique :

| Format | Lignes approximatives |
|--------|----------------------|
| Python | ~25 lignes |
| Rust | ~40 lignes |
| Go | ~50 lignes |
| Verbose IR | ~200 lignes |

Le rapport est de 5x à 8x. Mais ces 200 lignes portent :
- Les preuves de pureté et de terminaison
- Les patterns d'accès mémoire
- Les déclarations de parallélisme
- Les tests dérivés
- Les cas limites détectés
- Les hints d'optimisation CPU
- La traçabilité vers l'intention source

**Un humain ne les écrirait jamais. Une IA les génère en secondes. Un compilateur les exploite pour produire un binaire optimal.**

---

## 7. Questions Ouvertes pour le POC

1. **Format de sérialisation** : Le Verbose IR est-il du texte lisible (comme ci-dessus), du JSON structuré, du protobuf, ou un format binaire custom ? Pour le POC : texte lisible. Pour la production : probablement un format binaire avec un pretty-printer.

2. **Versioning de l'IR** : Comment gérer les évolutions du schéma ? Compatibilité ascendante obligatoire ?

3. **Validation croisée** : Le compilateur doit-il re-vérifier les preuves de l'IA ou les accepter sur confiance ? Recommandation : re-vérifier systématiquement (zero trust).

4. **Granularité des blocs** : À quel niveau de détail découpe-t-on ? Une règle = un bloc ? Ou une expression = un bloc ?

5. **Backend** : LLVM, WASM, ou binaire custom pour le POC ?