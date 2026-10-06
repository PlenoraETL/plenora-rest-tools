# AGENTS.md

Il documento che si legge per primo. Non descrive il repository — quello lo fa
il repository — ma dice **che cosa non è negoziabile** e **dove sta il resto**.

## Le regole che non cambiano

Sono policy, non fatti del codice: non si generano da nessuna parte, e questo è
l'unico posto in cui sono scritte. Sono le stesse delle altre librerie Plenora.

1. **Niente failure silenziose.** Un risultato sbagliato è peggio di un
   errore. Un caso che il motore non sa trattare si rifiuta con un errore
   tipizzato, mai con un valore plausibile: un null al posto di un dato
   mancante, un troncamento non dichiarato, un tentativo ripetuto prima del
   tempo chiesto dal server. Ogni limite ha un comportamento esplicito oltre
   la soglia, registrato in [`docs/limiti.md`](docs/limiti.md); i pochi
   limiti applicati senza errore sono elencati lì con il loro motivo.
2. **Niente `unsafe`** (`unsafe_code = "forbid"` per tutto il workspace).
   Niente panic nel codice delle librerie: il gate anti-panic nega unwrap,
   expect, panic, unreachable, todo, unimplemented e accessi con indice che
   possano uscire dai limiti.
3. **Dipendenze**: ogni dipendenza diretta ha versione esatta (`=x.y.z`) e una
   motivazione scritta accanto al pin in `Cargo.toml`; una dipendenza nuova
   entra solo così. Gli strumenti dei gate (maturin, jsonschema,
   cargo-llvm-cov, le azioni dei workflow) sono fissati allo stesso modo. Un
   advisory si accetta solo con un'esenzione motivata in `deny.toml`, che
   `scripts/check_advisory_exemptions.py` rifiuta quando la motivazione scade.
4. **Ogni correzione ha una prova che fallisce senza la correzione**, e ogni
   ottimizzazione o scorciatoia ha un oracolo che la confronta con il percorso
   generico. Trovato un difetto, si cerca la stessa classe altrove.
5. **Determinismo**: stesso input, stesso output. Nessun ordine che dipenda da
   hash, thread o tempo; l'enrichment concorrente restituisce i record
   nell'ordine dell'input.
6. **Errori senza dati**: i messaggi d'errore sono testo statico del motore.
   Mai valori di righe o colonne, body remoti, header, credenziali o path
   locali, nemmeno in diagnostica (`ErrorDetail` si costruisce solo da
   stringhe statiche).
7. **Contratti**: le superfici v1 (schemi, export Rust e Python, binding)
   sono congelate da `contracts/compatibility-v1.json`; una modifica
   incompatibile richiede un nuovo contratto versionato o una decisione
   esplicita registrata. Una regola dei contratti comuni non rispettata è una
   deviazione dichiarata in `adoption-manifest.json` con regola, ambito,
   rischio e rientro, mai un silenzio.
8. **Seconda revisione** per la logica critica: confronti, conversioni
   numeriche, aggregazioni, serializzazione, validazione al confine runtime.
9. **Prima del commit**: rustfmt, Clippy con `-D warnings`, Clippy
   anti-panic sulle librerie, test completi del workspace, controllo MSRV,
   validatore dei contratti. Tutti verdi. Un gate non eseguito si dichiara
   non eseguito. Prima del merge in `main` la CI è verde su Linux **e**
   Windows.

## Dove sta il resto

| serve | sta in |
| --- | --- |
| che cosa fa il motore e dove sono i confini | [`docs/architecture.md`](docs/architecture.md) |
| dove le garanzie si fermano, limiti e deviazioni | [`docs/limiti.md`](docs/limiti.md) |
| i contratti pubblici e la loro adozione | [`contracts/README.md`](contracts/README.md), [`adoption-manifest.json`](adoption-manifest.json) |
| come si costruisce, che cosa si esegue, come si rilascia | [`docs/development.md`](docs/development.md) |
| il lavoro futuro | [`docs/roadmap.md`](docs/roadmap.md) |
| che cosa è cambiato | [`CHANGELOG.md`](CHANGELOG.md) |
| perché una decisione è stata presa | `git log` |

## Prima di dire «fatto»

I comandi stanno in [`docs/development.md`](docs/development.md) e nei
workflow sotto `.github/workflows/`: sono la fonte, e ricopiarli qui li farebbe
divergere al primo cambiamento. Il gate completo è `scripts/verify.ps1` (lo
stesso della CI Linux); il job Windows del workflow Verify è parte del gate.
