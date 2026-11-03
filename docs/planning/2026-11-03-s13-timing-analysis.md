# S1.3 — Analisi dei tempi R3.3 (dati reali run owner 03/11)

**Data:** 2026-11-03 · **Fonte:** `%LOCALAPPDATA%\ibis\plugin\plugin.log`
(timestamp UTC; local = UTC+1) — righe runtime `embed_text`,
`memory_search:*`, boot database, ciclo di vita sessioni. Evidenza lato
VSIX (canale Output) **non persistita** dal run: vedi §Limiti.
**Nota (03/11, post-analisi):** la strumentazione VSIX/ide-host che produceva
le righe del canale Output è stata rimossa in 1.0.1 a fine sessione debug
(decisione owner); i numeri raccolti con le build 1.1.0–1.1.2 restano validi
e documentati qui.

---

## 1. Le sessioni di oggi (chi ha pagato cosa)

| Sessione (UTC) | Attore | Boot spawn→ready | Composizione boot | Note |
|---|---|---|---|---|
| 06:26:35–06:27 | owner (1° tentativo UI, pre-1.1.0) | **~17.1s** | check ast-grep+surra ~1.2s · **SurrealDB boot 4.07s** · **Ollama DOWN → daemon auto-start 8.72s** · schema 0.32s | il caso "R3.3 timeout >15s" ricostruito: Ollama giù incassa +8.7s di boot |
| 06:57:55–06:58:05 | agente (dry-run seed) | ~3.4s | SurrealDB boot **3.05s** + schema | readiness poll: DB pronto a 3.35s dallo spawn |
| 06:58:30–06:58:36 | agente (seed riuscito) | ~2.4s | SurrealDB boot ~2.4s | 3 `memory_add` con embed 2.84s→19ms→18ms |
| 07:05:47–07:07+ | **owner UI (VSIX 1.1.0)** | **~4.9s** | ast-grep 0.31s · check surreal 0.86s · **SurrealDB boot 3.17s** · connect 0.02s · schema 0.30s · AI probe 0.25s | prima espansione = boot pagato qui |
| 07:10:00–07:10:09 | **owner UI (VSIX 1.1.1)** | **~1.4–2.4s** | **SurrealDB boot 1.04s** (page cache calda) | singole `memory_list` a ritmo umano (0.3–1.7s = click) |

Varianza chiave: **il boot del processo SurrealDB va da 1.04s a 4.07s**
(page cache fredda/calda) e **Ollama giù costa +8.7s** (daemon auto-start).
Tutto il resto del boot è rumore (<1.5s).

## 2. Decomposizione del cold start (sessione owner 07:05, Ollama su)

```
spawn child ──► ast-grep check        0.31s
            ──► surreal check         0.86s   (version probe ×2)
            ──► SurrealDB boot        3.17s   ◄── 65% del boot
            ──► connect + schema      0.32s
            ──► AI probes (Ollama×2)  0.25s
            ──► STDIO ready           ──────
            TOTALE                    ≈ 4.9s  ◄── pagato dalla PRIMA chiamata
```

- La **prima `memory_list` arriva 2ms dopo l'handshake** (51.983 → 51.984):
  VS Code ripristina l'espansione della tree view e il fetch parte subito —
  quindi la prima espansione percepita ≈ boot intero (~4.9s).
- Con Ollama giù, lo stesso percorso ≈ **17s** (06:26) → il vecchio
  `timeout waiting for MCP response id=1` >15s è esattamente questo.

## 3. Decomposizione per-chiamata (runtime)

| Operazione | Cold (per processo) | Warm | Fonte |
|---|---|---|---|
| `memory_list` (frase B) | ~1–10ms (niente riga di timing runtime — vedi §6) | ~1–10ms | inferenza da pattern di arrivo (5 call in 10ms nei burst) |
| `memory_search` embed | **2.84s** (load modello Ollama, primo colpo per processo) · 30–120ms (modello caldo, query nova) | **23–34µs** (cache in-process) | `embed_text: dt=… ok=true` |
| `memory_search` db query | 19–105ms (prime query post-schema) | **1.0–1.4ms** | `db query returned in …` |
| `memory_search` totale | 121ms (07:06:12) · 20–36ms (seed) | **1.5–1.8ms** | `done in …` |
| `memory_expand` (frase C) | n/d | n/d | nessuna riga runtime (§6) |
| embed `memory_add` | 2.84s → 19ms → 18ms (stesso processo) | — | seed 06:58 |

Storico 30/10 (pre-fix, per confronto): `memory_search: embedded in 3.0–5.4s`
— l'embed freddo dominava ogni search; oggi il cold embed per-processo resta
2.84s **ma è amortizzato dalla cache** (le query ripetute cadono a µs).

## 4. Il bottone refresh — conferma dai dati

Le raffiche `5× memory_list + 1× memory_search` a 07:07:23 / 25 / 27 / 28 /
30 (≈2s di distanza) sono **5 click di refresh** su una tree con 5 kind
espansi + il nodo search: ogni click **rifetchava davvero tutto** (con
everything warm: embed µs, db 1ms, totale 1.5–1.8ms).

→ Il "non fa niente" era un difetto di **feedback**, non di funzione:
refetch identico, nessuno spinner, nessuna riga di log. Chiuso in 1.1.2
(spinner nel title + righe `refresh requested`/`refresh cycle complete`).

## 5. Verdetto S1.3 contro i target

| Target | Esito | Prova |
|---|---|---|
| warm < 500ms | **PASS, 2 ordini di grandezza sotto** | memory_list ~1–10ms, memory_search warm 1.5–1.8ms |
| cold < 2s | **FALLITO — ma non è il path di query** | boot spawn→ready 1.4–4.9s (Ollama su) / ~17s (giù); la prima espansione paga il boot intero |

Il cold target è violato dalla **boot tax del child** (SurrealDB 1–4s +
check 1.2s + probe), non dalla latenza delle chiamate. Tre opzioni:

1. **Pre-warm all'attivazione** (consigliata): spawn child + `initialize` +
   una chiamata nula in `activate()` — il boot si paga durante lo startup di
   VS Code e la prima espansione diventa warm. Implementabile in ide-host
   (~10 righe) — dimmi e la faccio.
2. Ridefinire il target: "cold = prima espansione **con sessione già viva**"
   (<2s, già PASS) e tenere il boot fuori dal target, tracciandolo a parte.
3. Ridurre il boot di SurrealDB (surrealkv journal replay) — guadagno
   1–3s, costo alto, non raccomandato adesso.

## 6. Anomalie e gap di strumentazione (da registrare)

1. **`ERROR Failed to send query results to channel: SendError(..)`** a ogni
   boot (06:26, 07:05, 07:10) — benigno in apparenza (startup), ma va
   attribuito: probabile listener che chiude il canale prima del primo result.
2. **`embed_text … ok=false dt=2.0s` a raffica** quando Ollama sta ancora
   partendo: il provider ottiene timeout di connessione da 2s ×3 prima di
   riuscire. Con il daemon auto-start (06:26) la finestra è peggiore.
   Candidato: retry esplicito o gate "ollama not ready" durante il boot.
3. **Nessuna riga di timing runtime per `memory_list` e `memory_expand`** —
   le due operazioni della frase B/C non lasciano numeri nel plugin.log; i
   numeri S1.3 per B/C esistono solo nel canale Output (VSIX 1.1.x). Per la
   parità di evidenza: aggiungere le righe (`memory_list: done in …`,
   `memory_expand: done in …`) — azione agente, S1.4-adiacente.
4. **Evidenza VSIX non persistita**: il runbook chiede di salvare
   `output-channel.txt` ma la cartella `artifacts/ui-tests/*-r33*` non esiste
   — i numeri `tools/call … in Xms` e `session cold init` di oggi sono
   persi. Per il prossimo run: `code > Output > Ibis Assistant → copia` o
   attivare un sink su file. **Chiusura R3.3 (03/11) registrata comunque in
   checklist** su plugin.log + numeri build strumentate (gap qui descritto).
5. **`BatchManager: Failed to fetch pending commits` (ripetuto, ~10s)** —
   deserializzazione `CommitRow.date`: `Expected string, got datetime`
   (`SELECT … FROM commit WHERE batch_status = 'pending'`) — righe con
   `date` datetime non decodificano nel tipo `none | string`. Error lane
   background (ingest), non tocca il path tree-view di R3.3; da fixare a
   parte (mapping datetime → string nel deserializer o nella query).
6. **`BatchManager: Failed to embed batch: batch_embed_text is not
   supported by provider 'ibis-ai-client'`** — il provider adapter lato
   server (client AI dell'httpserver) non implementa `batch_embed_text`
   mentre il BatchManager lo chiama in fallback sincrono. Anche questa è
   lane background: i chunk restano in coda (`Found 100 chunks`) senza
   vettorizzare. Da attribuire a implementazione missing dell'adapter, non
   a R3.3.

## 7. Cosa resta per chiudere S1.3

- [x] Decisione owner sul cold target (opzione 1/2/3 del §5) — **opzione 1,
      pre-warm all'attivazione (03/11)**: atterrata in VSIX 1.0.2 (`bf824b1`).
- [ ] Se opzione 1: pre-warm + nuovo run owner → la prima espansione misurata
      da canale Output (questa volta salvato). Pre-warm atterrato; **run
      owner da fare**: attesa riga `pre-warm: runtime session ready in Nms`
      sul canale e prima espansione warm-like (cronometro + screenshot).
- [x] Evidenze del run 1.1.2 (numerino + refresh con feedback) registrate
      nella checklist §Esito (03/11: R3.3 → PASS, con nota sul gap §6.4).
- [x] S1.4 (demote info→debug) dopo il verdetto — fatto 03/11 (`e9b85b8`):
      timing `memory_search` a debug + righe nuove a info
      `memory_list: done in …` / `memory_expand: done in …` (§6.3, parità
      di evidenza per il prossimo run).