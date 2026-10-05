# Report delle esecuzioni

Soltanto esecuzioni realmente avvenute, così come le ha scritte
`plenora-rest-campaign`: ogni JSON contiene commit, ambiente, toolchain,
profilo, soglie con il loro stato di approvazione, campioni delle risorse ed
esito per criterio. Le soglie usate sono i default proposti, non ancora
approvati.

| report | commit | ambiente | carico | esito |
| --- | --- | --- | --- | --- |
| [2026-10-05-smoke](2026-10-05-smoke.md) | e78602e | VM Ubuntu, kernel 5.15, 8 CPU, 15 GB, rustc 1.98.0 release | passaggio funzionale + 15 min, 4 worker, 4 op/s | fallita: difetti 1-3 della roadmap |
| [2026-10-05-load](2026-10-05-load.md) | e78602e | come sopra | 15 min, 32 worker, 40 op/s, 200 req/s | fallita: difetti 1-3 e un esaurimento temporaneo del disco della VM (vedi sotto) |
| [2026-10-05-soak](2026-10-05-soak.md) | e78602e | come sopra | 75 min, 16 worker, 20 op/s, 200 req/s | fallita: difetti 1-3; risorse entro le soglie |
| [2026-10-05-load-2](2026-10-05-load-2.md) | ffbf34d | come sopra | 10 min, 32 worker, 40 op/s, 200 req/s | fallita: difetti 1-3 |

Note:

- [2026-10-05-load](2026-10-05-load.md): tra il secondo 120 e il 170 del
  carico il filesystem della VM, condiviso con altre sessioni, si è riempito.
  Il motore ha risposto con FILE_IO senza pubblicare file né lasciare
  parziali; l'harness invece lasciava vuoti i sorgenti degli upload e
  segnalava come incompleto un artifact assente dopo un download fallito.
  Gli esiti inattesi, gli errori dell'harness, i file residui e i file
  incompleti di quel report vengono da lì e non dal motore; l'harness è stato
  corretto in ffbf34d e il load è stato ripetuto
  ([2026-10-05-load-2](2026-10-05-load-2.md)) senza quelle violazioni.
- [2026-10-05-soak](2026-10-05-soak.md): 75 minuti, non le 4-6 ore della
  roadmap. L'RSS cresce a gradini (circa 50, 52, 57, 61 e 74 MiB) e resta
  piatto nell'ultimo quarto d'ora; crescita 21 MiB e pendenza 27 MiB/h dopo
  il warm-up, sotto le soglie proposte (64 MiB e 32 MiB/h) ma vicina alla
  pendenza massima. Descriptor e thread tornano ai valori iniziali a riposo.
  Va confermato con un soak lungo.
- Il picco di richieste contemporanee visto dal server è 6 in tutte le
  esecuzioni (limite 16 nello smoke, 32 negli altri): nello smoke per i pochi
  worker, nel load e nel soak perché domina il limitatore di rate (200
  richieste al secondo, rispettato: massimo 200 in un secondo). Il criterio di
  concorrenza è quindi superato senza essere stato messo alla prova.
