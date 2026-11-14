# Prove congelate 0.4.0

Tre fonti distinte:

- `sources.json`: identità SHA-256 dei contratti adottati e delle fixture locali;
  il generatore verifica prima i pin di `contracts/upstream/source.json`.
- `transitions.json`, `mixed.json`, `private-inputs.json`: oracoli e input
  dichiarativi separati dal reducer. La provenance cita ERR-003/ERR-014 e le
  regressioni storiche della baseline.
- `observed.json`: risultati pubblici **osservati** sul prodotto 0.4.0, errori,
  metriche, body batch, byte degli artefatti, CLI, runtime, SDK e decisioni retry.
  Contiene commit e digest dei sorgenti dell'acquisizione. Non è l'indice delle
  fixture e non viene aggiornato da `--check`.

`retry.json` è generato da `scripts/generate_frozen_retry.py` usando `Fraction`,
senza importare il prodotto. I fattori scelti sono razionali diadici esatti e gli
otto passi restano nel dominio senza troncamento della parte frazionaria. Il
verificatore Rust chiama la policy privata reale, con `SystemTime` fissato per
HTTP-date e clock Tokio sospeso per le attese brevi; le durate enormi si verificano
come valori senza programmarle. Il clock sospeso è abilitato soltanto nelle
dipendenze di sviluppo. L'unico aggancio nei sorgenti è un include dentro il
modulo `cfg(test)` già esistente di transport.

La matrice di rete verifica separatamente buffer e download contro un server
osservatore: recupero, rifiuto oltre cap, header ignorato esplicitamente, POST,
esaurimento tentativi e status non ripetibile. Questi casi usano attese zero:
non provano il tempo di ogni transizione dei loop reali. Le regressioni di body
timeout, Retry-After e deadline in backoff sono rieseguite.

Il corpus misto usa gli stessi record con concorrenza 1, 2 e 4. Per i percorsi
concorrenti il server non rilascia `a` finché il client non ha letto e chiuso la
risposta completa di `b`. Output, errori ordinati e metriche restano equivalenti.
Il batch ha attesi propri sul wire e sul risultato, inclusa la coda 2+1.

Sono esclusi dagli snapshot soltanto `metrics.elapsed_ms` delle esecuzioni e
`plenora.message.id` generato nelle risposte runtime. Le sentinelle hanno lunghezza
fissa, quindi le coordinate degli errori di parsing restano stabili. Nessuno
scrubber ricorsivo elimina campi sconosciuti, URL o path. CLI exit code e stderr,
trace/causation runtime, errori e tutti gli altri contatori sono conservati.

## Esecuzione

Con Python 3.14 e maturin 1.16.0 installati in `target/sdk-venv`, e modulo nativo
costruito nel worktree, il runner compila solo i target selezionati con
`CARGO_BUILD_JOBS=2`, `--locked --offline`. Scopre i test dagli eseguibili Rust,
li esegue per ID e continua dopo fallimenti e timeout. I vettori runtime adottati
e le regressioni selezionate sono prove correnti solo quando la ricevuta passa.

```powershell
python scripts/generate_frozen_retry.py --check
python scripts/frozen_proofs.py --check --run-python
python scripts/frozen_rust.py --check
```

Per acquisire una baseline osservata, dopo revisione del cambiamento:

```powershell
python scripts/frozen_rust.py --record
```

`--record` esegue prima le asserzioni indipendenti e tutti i test selezionati;
non aggiorna il corpus se una prova fallisce. `--check` confronta anche l'insieme
esatto degli ID osservati: un caso tolto o aggiunto non è ignorato. I file di
cattura rifiutano ID duplicati. Non basta che il processo termini con 0.

Le ricevute in `target/frozen/` includono risultati individuali, commit, digest
dei file locali e del modulo nativo. Skip, discovery vuota, risultati mancanti
e test non eseguiti non qualificano la suite. I report in `campaign/reports`
rimangono storici e non vengono promossi a evidenza corrente.

## Mutazioni

```powershell
python scripts/mutate_frozen_proofs.py
python scripts/mutate_frozen_rust.py
python scripts/mutate_frozen_sdk.py
```

Ogni runner verifica prima la baseline. Il runner Rust richiede un fallimento
di asserzione, ripristina i byte originali in `finally` e ricompila/riesegue la
prova ripristinata. Quello SDK ricostruisce anche il modulo nativo dopo il
ripristino. Non eseguire altri test o build nello stesso worktree durante le
mutazioni. Nessun runner esegue commit, push o operazioni GitHub.

## Confini della qualifica

Le prove coprono i casi elencati, non ogni transizione possibile. Restano fuori
la cancellazione/deadline in ogni ramo OAuth/polling/cache/circuit, i fault
filesystem open/write/flush/rename/cleanup su tutti gli OS, sentinelle in ogni
risposta remota e ricevute individuali per ciascun sottocaso interno alle fixture
runtime. La campagna e il gate completo Linux/Windows non sono sostituiti da
questa selezione. Il resoconto della sessione è in `target/SEMPL-1-REST.md`.
