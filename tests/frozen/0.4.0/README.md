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
dipendenze di sviluppo. Gli agganci test-only sono descritti nella sezione rami e guasti sotto.

La matrice di rete verifica separatamente buffer e download contro un server
osservatore: recupero, rifiuto oltre cap, header ignorato esplicitamente, POST,
esaurimento tentativi e status non ripetibile. Questi casi usano attese zero:
non provano il tempo di ogni transizione dei loop reali. Le regressioni di body
timeout, Retry-After e deadline in backoff sono rieseguite.

Il corpus misto usa gli stessi record con concorrenza 1, 2 e 4. Per i percorsi
concorrenti il server non rilascia `a` finché il client non ha letto e chiuso la
risposta completa di `b`. Output, errori ordinati e metriche restano equivalenti.
Il batch ha attesi propri sul wire e sul risultato, inclusa la coda 2+1.

Sono esclusi dagli snapshot `metrics.elapsed_ms` delle esecuzioni e
`plenora.message.id` generato nelle risposte runtime. L'origine loopback delle
fixture remote viene verificata e sostituita con un valore fisso. Le sentinelle hanno lunghezza
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

## Rami, clock e guasti

`io-branches.json` enumera i punti semantici e I/O del banco `cfg(test)`:
prepare, OAuth iniziale/letto/cached, polling, cache miss/hit/revalidate,
circuit admission/reject/probe, cookie, open/write/flush/finalize/publication/
rename/cleanup. Ogni punto viene raggiunto sia con cancellazione sia con
scadenza del clock Tokio. Non si aggiungono yield alle regioni sincrone del
prodotto: una regione terminale può ancora concludersi prima che il controllo
sia ripollato. Gli snapshot conservano anche questi successi osservati.

Il confronto tra enum e tabella rifiuta un punto dichiarato nuovo non censito;
le prove verificano che ogni punto censito sia raggiunto. **Non** scopre un
nuovo ramo Rust privo di strumentazione. La mutazione dell'inventario qualifica
il verificatore, non il prodotto. Non è una guardia testuale sui sorgenti.

Il clock delle attese interne del trasporto è quello Tokio fermato: i server
osservano l'assenza di un tentativo prima di 250/500 ms, sia buffered sia download,
per status transitorio, connessione interrotta e body troncato. Il wrapper di
test registra le attese effettive; la produzione importa direttamente Tokio.

I guasti PermissionDenied con sentinella sono iniettati nei sette punti I/O;
si verificano codice pubblico, richieste inviate, pubblicazione e assenza di
staging residuo. Non sono equivalenti a guasti hardware/OS. La prova Linux
`scripts/frozen_linux_io.py` prepara rifiuto POSIX e limite reale di dimensione
file; restituisce skip/77 fuori da Linux o come root. Lo script locale
`target/SEMPL-1-REST-VM.sh` è il piano di esecuzione VM, non una ricevuta verde.

`remote-private.json` è la fonte condivisa per i sei formati remoti, error_path,
status 200/400 e sette header sensibili: core, runtime, CLI e SDK sync/async.
Le risposte dati valide non sono segreti per definizione: la prova vieta
l'esposizione nelle diagnostiche di errore e negli header sensibili selezionati.

`scripts/frozen_signals.py` invia segnali OS reali in un processo/console isolato.
Su Windows usa Ctrl-Break/Ctrl-Break e Ctrl-C/Ctrl-Break; su Unix SIGINT/SIGTERM e SIGTERM/SIGINT. Il server
trattiene la risposta di cancellazione per verificare che il secondo segnale
non tronchi la chiusura; poi verifica exit 130 e una sola cancellazione remota.

Mutazioni aggiuntive, sempre da eseguire senza altre build nello stesso worktree:

```powershell
python scripts/mutate_frozen_rust.py --prefix branches-
python scripts/mutate_frozen_signals.py
python scripts/mutate_frozen_sdk.py --headers --surface sdk
python scripts/mutate_frozen_sdk.py --headers --surface cli
```

## Confini della qualifica

### Artefatti cancellati: regola portabile

ERR-005 (`contracts/upstream/specs/errors/ERRORS-1.0.md`) vieta di dedurre
rollback da timeout/cancellazione. `docs/architecture.md`, Artifact e streaming,
richiede staging rimosso, vieta risultati incompleti e non annulla una promozione
già completata. Finalize precede `persist_download` su tutti gli OS: senza
overwrite la pubblicazione usa hard_link, con overwrite rename. Un segnale
cooperativo a Finalize può essere osservato dopo l'avvio della syscall.

I 14 casi `control-artifacts-*` relativi ai download congelano quindi la regola
in `io-branches.json`, non la vittoria di uno scheduler: assente oppure completo,
mai staging/byte parziali; completo obbligatorio dopo pubblicazione confermata
o esito di successo. `contract_checked_cases` distingue questi oracoli dagli
esiti esatti in `observed.json`. I risultati grezzi restano sotto
`target/frozen/control-artifacts/<os>/`, senza entrare nella decisione attesa.
La divergenza Linux/Windows a Finalize-cancel era un difetto dell'atteso, non
evidenza sufficiente di un difetto del prodotto.

Il piano VM delega a `scripts/frozen_vm.py`: ogni coppia punto/controllo viene
eseguita in un processo separato, così un assert non nasconde le coppie successive.
Anche dopo errori/skip il runner prosegue con le altre prove e mutazioni,
scrive ricevute sotto `target/frozen/linux/<run>/` e restituisce exit 1 se la
raccolta non è interamente verde. Non riclassifica fallimenti come attesi-rossi.

Le prove coprono i punti elencati, non ogni condizione interna di ogni funzione.
Restano fuori il censimento automatico dei rami non strumentati, tutte le
combinazioni di stato/errore e qualifica Linux corrente.
I fault sintetici non dimostrano l'identico comportamento di ogni filesystem.
T5 campagne/artefatti di release e T6 release riproducibile sono rinviati per
istruzione del titolare. Il gate completo non è sostituito da questa selezione.
Il resoconto della sessione è in `target/SEMPL-1-REST.md`.
