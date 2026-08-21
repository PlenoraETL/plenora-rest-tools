# Stato e strada verso la produzione

Questo documento è l'unico posto in cui viene mantenuto il lavoro futuro del
progetto. README, architettura e contratti descrivono soltanto ciò che esiste
oggi.

## Valutazione attuale

Plenora REST Tools è una candidata pre-produzione con base funzionale
consolidata. Il core, le superfici pubbliche e la supply chain di release sono
implementati e verificati. Non è ancora dichiarata pronta al go-live perché
mancano validazione operativa in staging, release candidata e adozione canary
in Plenora.

## Completato

| Area | Stato | Evidenza |
| --- | --- | --- |
| Core provider-neutral | completato | nessun adapter specifico; configurazione tramite contratti |
| Operazioni v1 | completato | test, generate, enrich, download e upload |
| Rust | completato | crate pubblico e runtime binding |
| Python | completato | SDK sincrono e wheel ABI3 py310 |
| Runtime Plenora | completato | envelope, capability, lifecycle e risorse autorizzate |
| Contratti | completato | sei schemi Draft 2020-12 e baseline v1 |
| Sicurezza | completato | default fail-closed e redazione pubblica |
| Resilienza | completato | retry, rate limit, cache, cookie e circuit breaker |
| Job REST | completato | polling, resume, recovery e cancellazione remota |
| Artifact | completato | upload/download streaming, resume e SHA-256 |
| Gate CI | completato | MSRV, format, Clippy, test, wheel e matrice Python |
| Release | completato | build riproducibile, checksum, SBOM e attestazioni |
| Support matrix | definita | Linux manylinux2014 x86_64, CPython 3.10-3.14 |

Il gate breve include già test deterministici per concorrenza limitata, retry
su fault transitori e streaming multi-megabyte. Questi test impediscono
regressioni macroscopiche, ma non simulano un carico operativo prolungato.

## Prossimo gate: campagna finale in staging

La campagna deve essere esterna alla CI ordinaria e deve usare configurazioni,
dimensioni e limiti rappresentativi di Plenora. È il prossimo lavoro
prioritario.

### Fase 1: smoke operativo

Durata prevista: 15-30 minuti.

- avvio e arresto ripetuto dei worker;
- test delle cinque operazioni;
- credential_ref e artifact source/sink reali;
- deadline, cancellazione e recovery;
- verifica immediata di log, metriche e cleanup.

La fase successiva parte soltanto se non emergono errori funzionali.

### Fase 2: load e fault injection

Durata prevista: 1-2 ore.

- concorrenza e rate limit rappresentativi;
- 429 e 5xx con Retry-After;
- DNS failure, connect timeout e TLS failure;
- risposta interrotta prima e dopo un possibile effetto remoto;
- errori durante paginazione e polling;
- upload, download e resume con artifact realistici;
- verifica dell'assenza di amplificazione incontrollata delle richieste.

### Fase 3: soak

Durata prevista: 4-6 ore automatiche.

Durante il test devono essere osservati:

- memoria residente;
- handle o file descriptor;
- connessioni e socket;
- disco e file temporanei;
- latenza e throughput;
- errori per categoria e fase;
- retry, circuit breaker e code interne del runtime.

Il carico deve alternare richieste brevi, enrichment concorrente, polling e
trasferimenti, evitando un benchmark artificiale su una sola operazione.

### Fase 4: report

Durata prevista: circa un'ora, esclusa la correzione di problemi.

Il report deve registrare:

- commit e versione esaminati;
- ambiente e configurazione;
- durata e profilo del carico;
- percentili di latenza;
- throughput e tasso di errore;
- picchi e trend delle risorse;
- fault iniettati e comportamento osservato;
- limiti accettati;
- anomalie, severità e decisione finale.

## Criteri di accettazione della campagna

Prima dell'esecuzione devono essere fissati i limiti numerici adatti
all'ambiente Plenora. In ogni caso il gate fallisce se si osserva:

- crash, panic, deadlock o corruzione dei dati;
- crescita non stabilizzata di memoria, handle o file temporanei;
- superamento persistente dei limiti di concorrenza o rate;
- retry oltre max_attempts;
- duplicazione di submit durante resume;
- pubblicazione di file incompleti;
- perdita di ordine nell'enrichment;
- esposizione di segreti o path;
- impossibilità di cancellare, chiudere o recuperare il worker;
- remote_effect o retry advice incoerenti con il fault.

I problemi bloccanti vengono corretti e verificati con uno scenario breve
mirato prima di ripetere soltanto la fase della campagna interessata.

## Gate successivo: release candidata

Dopo una campagna verde:

1. scegliere la nuova versione;
2. sincronizzare i cinque manifest di versione;
3. rigenerare e approvare i digest degli artefatti;
4. rieseguire verifica e release riproducibile;
5. unire su main;
6. creare il tag annotato;
7. verificare GitHub Release, checksum, SBOM e attestazioni.

La release non deve essere pubblicata automaticamente su crates.io o PyPI
finché non viene definita una politica esplicita per quei registri.

## Gate finale: integrazione Plenora

L'adozione deve trattare la libreria come black-box:

1. integrare il runtime binding o la wheel senza duplicare logica HTTP;
2. tradurre la configurazione Plenora nei contratti v1;
3. sostituire il percorso REST precedente dietro un controllo di rollout;
4. eseguire test end-to-end su servizi rappresentativi;
5. attivare un canary limitato;
6. osservare errori, latenza e risorse;
7. aumentare gradualmente il traffico;
8. rimuovere il vecchio percorso soltanto dopo una finestra stabile.

Il rollback deve poter ripristinare il percorso precedente senza modificare
contratti o dati persistenti.

## Definizione di produzione

La libreria può essere dichiarata pronta per la produzione quando:

- la campagna staging è verde e il report è approvato;
- non esistono problemi aperti di severità bloccante;
- la release candidata è riproducibile e attestata;
- l'integrazione end-to-end in Plenora è verde;
- il canary rispetta i limiti approvati;
- esistono procedura di rollback e ownership operativa.

## Evoluzioni non bloccanti

Dopo il go-live iniziale possono essere valutati:

- supporto Linux ARM;
- wheel per Windows e macOS;
- target musl;
- SDK Python asincrono;
- ulteriori formati o strategie di paginazione generiche;
- pubblicazione su crates.io e PyPI.

Ogni nuova piattaforma entra nella matrice soltanto con artifact, smoke test e
gate equivalenti. Nessuna evoluzione deve introdurre adapter specifici nel
core.

## Fuori roadmap

Non è previsto trasformare la libreria in:

- client dedicato a un singolo servizio;
- amministratore di broker o code;
- orchestratore ETL;
- archivio persistente dei job remoti;
- sostituto del runtime Plenora.

## Riferimenti

- [Panoramica](../README.md)
- [Architettura](architecture.md)
- [Contratti](../contracts/README.md)
- [Sviluppo e release](development.md)
