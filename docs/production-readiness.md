# Production readiness

Questo documento definisce il perimetro di produzione iniziale di Plenora REST.
La matrice e volutamente stretta: nuove piattaforme entrano nel supporto solo
dopo avere ottenuto artifact, smoke test e gate di release equivalenti.

## Matrice supportata

| Superficie | Supporto iniziale |
| --- | --- |
| Sistema operativo | GNU/Linux compatibile manylinux2014, glibc 2.17+ |
| Architettura | x86_64 |
| Python | CPython 3.10, 3.11, 3.12, 3.13 e 3.14; ABI3 py310; API sincrona |
| Rust | MSRV 1.85.1; target x86_64-unknown-linux-gnu |
| Distribuzione | crate e wheel allegati alla GitHub Release |
| Integrita | SHA-256, SBOM SPDX 2.3 e attestazioni Sigstore/SLSA |

Non sono ancora superfici supportate Windows, macOS, Linux ARM, musl, PyPy,
CPython 3.15 e l'API Python asincrona. Questo non implica necessariamente
incompatibilita tecnica: indica che non sono ancora coperte dal contratto di
supporto e dai gate.

## Gate ordinario

Il comando seguente deve restare abbastanza rapido per ogni pull request:

~~~powershell
./scripts/verify.ps1
~~~

Il gate esegue:

1. validazione JSON Schema Draft 2020-12 e risoluzione dei riferimenti locali;
2. confronto con il baseline immutabile delle superfici pubbliche v1;
3. cargo check dell'intero workspace con Rust 1.85.1 e lockfile;
4. rustfmt, Clippy con warning negati e tutti i test Rust;
5. baseline breve di concorrenza, fault transitori e streaming;
6. build della wheel e test black-box da ambiente pulito;
7. installazione della stessa wheel ABI3 su CPython 3.10-3.14.

Le release aggiungono doppia build byte-identica, controllo dei digest,
generazione dell'SBOM e attestazioni di provenienza.

## Baseline prestazionale breve

Il test production_baseline non e una campagna di benchmark. Usa esclusivamente
server locali deterministici e verifica:

- 32 enrichment con concorrenza configurata a 8, ordine stabile e nessun
  superamento del limite;
- recupero esatto da 503 e 429 con tre richieste complessive e due retry;
- download streaming da 2 MiB con limite della risposta in memoria pari a
  1 KiB, checksum e assenza di file parziali.

Questi valori rilevano regressioni macroscopiche senza allungare la CI con un
soak test.

## Campagna finale separata

La campagna finale viene eseguita in staging dopo il consolidamento del gate
breve. Non deve essere inserita nella CI ordinaria. Comprendera almeno:

- carico prolungato con concorrenza e rate limit rappresentativi di Plenora;
- fault injection su DNS, connect, TLS, risposta interrotta, 429 e 5xx;
- upload, download e resume con artifact delle dimensioni operative reali;
- osservazione di memoria, file descriptor, disco temporaneo e latenza;
- verifica di cancellazione, deadline, recovery e arresto del worker.

I risultati della campagna dovranno registrare versione, commit, configurazione,
durata, percentili, errori e limiti approvati prima del go-live.
