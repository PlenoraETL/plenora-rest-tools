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

### Campagna come codice

Le fasi 1-4 esistono come codice versionato e si possono ripetere su ogni
commit; manca l'esecuzione in staging con le configurazioni Plenora (vedi
[Cosa resta per chiudere il gate](#cosa-resta-per-chiudere-il-gate)).

| parte | dove |
| --- | --- |
| harness e verificatore | crates/rest-campaign, binario plenora-rest-campaign |
| profili di carico per fase | campaign/profiles.json |
| soglie con motivazione e stato di approvazione | campaign/limits.json |
| esecuzione locale o su VM | scripts/campaign.sh |
| esecuzione su runner GitHub | .github/workflows/campaign.yml |
| report delle esecuzioni realmente avvenute | campaign/reports |

L'harness usa soltanto l'API pubblica (Engine, RuntimeBinding, tipi del
contratto) contro un server HTTP/1.1 locale in-process, scritto a mano come
nei test black-box, che inietta i guasti a un punto preciso dello scambio e
conta per ogni operazione richieste, corpi ricevuti, submit, polling,
cancellazioni e richieste Range. Ogni scenario dichiara l'esito atteso,
compreso l'errore atteso per un guasto, e gli invarianti da controllare:

| area | scenari |
| --- | --- |
| richieste brevi | risposta JSON, latenza variabile |
| 429 e 5xx | Retry-After rispettato, 5xx transitori recuperati, 5xx persistenti con esattamente max_attempts tentativi |
| risposta interrotta | connessione chiusa prima della risposta, corpo POST letto e connessione chiusa (effetto remoto possibile, nessun retry non idempotente), risposta troncata, risposta assente con timeout |
| guasti di rete | DNS inesistente (.invalid), porta chiusa, TLS verso la porta in chiaro, connect timeout verso un indirizzo che non risponde |
| controllo | deadline (Engine::execute, Engine::execute_with_control, payload del RuntimeBinding), cancellazione locale |
| paginazione | offset e link con errore transitorio a metà, cursor con errore persistente a metà (nessun output parziale) |
| enrichment concorrente | ordine e contenuto conservati con completamenti fuori ordine e 503 transitori |
| job asincroni | polling, errore transitorio di polling, polling timeout con recovery e resume senza nuovo submit, cancellazione e deadline con cancellazione remota |
| trasferimenti | download in streaming con SHA-256, ripresa con Range e If-Range, download corrotto e interrotto senza file pubblicato, upload in streaming |
| sessioni ed Engine | sessione cookie aperta, usata, chiusa e rifiutata prima della rete; Engine aperti e chiusi in sequenza che dopo close rifiutano il lavoro; credential_ref e artifact_sink risolti dal RuntimeBinding |

Ogni fase esegue prima un passaggio funzionale (ogni scenario una volta, più
Engine aperti e chiusi in sequenza), poi il carico misto per la durata scelta:
un generatore emette operazioni al rate obiettivo con scenario e parametri
derivati dal seed, un numero fissato di worker le esegue, e un'operazione che
trova tutti i worker occupati è contata come persa. Durante il carico vengono
campionati RSS, file descriptor e thread da /proc/self (solo su Linux: altrove
il report li dichiara non misurati), file temporanei e parziali nella
directory di lavoro, latenze (istogramma a memoria costante, p50, p95 e p99
arrotondati per eccesso), throughput, errori per categoria, fase e codice,
retry, concorrenza e richieste al secondo viste dal server per ogni Engine.
Alla fine il driver attende a riposo, misura i residui, chiude l'Engine e
verifica che rifiuti il lavoro.

Comandi:

~~~bash
scripts/campaign.sh smoke                     # passaggio funzionale + 15 min
scripts/campaign.sh load --duration-min 15    # carico con guasti, durata scelta
scripts/campaign.sh soak                      # 4 h (profilo), solo Linux
scripts/campaign.sh smoke --quick             # prova dell'harness, ~40 s
~~~

Il workflow Campaign esegue la fase scelta su avvio manuale, smoke da 5 minuti
e load da 15 ogni settimana, e le due fasi in modalità quick sulle pull
request che toccano la campagna; carica i report come artefatto. Un job su
runner GitHub dura al più 6 ore: il workflow rifiuta un soak con più di 330
minuti di carico, che va eseguito su una macchina dedicata.

Il report (JSON, schema plenora-rest-campaign-report-v1, e riassunto Markdown)
registra commit, versione, ambiente, toolchain, profilo, soglie e loro stato
di approvazione, durate, percentili, throughput, errori, campioni e andamento
delle risorse, guasti iniettati con remote_effect e retry advice osservati, ed
esito per criterio. L'exit code è 0 per una campagna superata, 1 per criteri
falliti, 2 per uso o configurazione non validi, 3 per un errore dell'harness.
`plenora-rest-campaign --verify-only REPORT.json` riapplica il verificatore a
un report salvato, per esempio con soglie approvate in seguito.

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

### Come la campagna verifica i criteri

Il verificatore è una funzione pura del report e delle soglie: un criterio
che non si può valutare non risulta mai superato, e nelle fasi load e soak
(non quick) un andamento delle risorse senza campioni sufficienti, o non
misurato perché la piattaforma non è Linux, fa fallire il gate. I test
dell'harness, eseguiti da cargo test, costruiscono report con una violazione
per ogni criterio e controllano che il gate fallisca.

| criterio | controllo |
| --- | --- |
| crash, panic, deadlock | panic contati da un gancio globale (anche nei task del motore); watchdog per operazione; drenaggio con limite di tempo |
| corruzione dei dati | valore della risposta, righe paginate, record arricchiti, SHA-256 e dimensione dei file confrontati con il contenuto deterministico del server |
| crescita non stabilizzata | dopo il warm-up, mediana della finestra finale contro quella iniziale e pendenza ai minimi quadrati per RSS, descriptor, thread e file temporanei; descriptor residui dopo l'attesa a riposo; picchi assoluti |
| concorrenza e rate | picco di richieste in servizio e massimo di richieste in un secondo per Engine, visti dal server, contro max_concurrent_requests e requests_per_second |
| retry oltre max_attempts | richieste ricevute dal server per operazione |
| submit duplicati al resume | submit contati per job |
| file incompleti pubblicati | destinazione assente dopo un download corrotto o interrotto, nessun file parziale residuo |
| perdita d'ordine nell'enrichment | record in uscita confrontati posizione per posizione |
| segreti o path | risultati, errori e messaggi del runtime serializzati e cercati per il segreto della campagna e per i path assoluti della directory di lavoro |
| cancellazione, chiusura, recovery | tempo dal segnale al ritorno, cancellazione remota con handle di recovery, Engine chiuso e sessione chiusa rifiutati prima della rete |
| remote_effect e retry advice | per classe di guasto: none dove nessun byte è partito, mai none dopo l'invio, mai safe per un POST interrotto, requires_recovery per un polling timeout |
| metriche | metrics.requests mai inferiore alle richieste ricevute dal server |

### Soglie proposte

I limiti numerici sono una decisione dell'utente. campaign/limits.json
contiene default proposti per il server locale, ognuno con la motivazione
scritta accanto, e il campo approval che il report ricopia; oggi vale
«proposta, da approvare». I principali:

| soglia | proposta |
| --- | --- |
| esiti inattesi, retry oltre max_attempts, submit duplicati, file incompleti, perdita d'ordine, esposizioni | 0 |
| p99 (ms) | ok 1000, page_offset 2000, job 2000, enrich 5000, download e upload 10000 |
| throughput | almeno il 90% delle operazioni pianificate |
| rate | requests_per_second + 10% + 2 nella finestra di un secondo |
| RSS | picco ≤ 512 MiB; crescita ≤ 64 MiB e pendenza ≤ 32 MiB/h dopo il warm-up |
| descriptor | picco ≤ 1024; crescita ≤ 32 e pendenza ≤ 16/h; residui a riposo ≤ 16 |
| thread | crescita ≤ 8 |
| file temporanei | picco ≤ 256, crescita ≤ 16, residui 0 |
| deadline e cancellazione | ritorno entro 1000 ms dal segnale |
| remote_effect più prudente del necessario | registrato, non bloccante |
| metrics.requests inferiore alle richieste ricevute | bloccante |

### Cosa resta per chiudere il gate

- approvare o sostituire le soglie di campaign/limits.json per l'ambiente
  Plenora e aggiornare il campo approval;
- eseguire le fasi in staging con configurazioni, dimensioni e servizi
  rappresentativi di Plenora: il server locale prova il motore, non i servizi
  reali, le credenziali reali né gli artifact source e sink del runtime;
- eseguire il soak per 4-6 ore su una macchina dedicata Linux;
- osservare le code interne del runtime Plenora, che la campagna del motore
  non vede;
- correggere i difetti aperti qui sotto e ripetere le fasi interessate.

### Difetti aperti trovati dalla campagna

Finché restano aperti, ogni fase della campagna fallisce: sono difetti del
motore, non delle soglie.

1. **Deadline della richiesta ignorata da execute_with_control** (bloccante).
   Engine::execute e execute_json applicano `options.deadline`;
   Engine::execute_with_control usa soltanto la deadline del controllo
   ricevuto e ignora quella della richiesta senza errore. Riproduzione:
   richiesta GET con `options.deadline` a 300 ms verso un servizio che accetta
   la connessione e non risponde, eseguita con
   `execute_with_control(request, ExecutionControl::default())`: termina con
   TIMEOUT solo al timeout di richiesta (15 s nella campagna) invece che alla
   deadline; un job in polling con intervallo lungo non termina fino alla fine
   del polling. Scenario `deadline_with_control`.
2. **Deadline nel payload del RuntimeBinding ignorata** (bloccante). Il
   binding applica soltanto la metadata `plenora.execution.deadline`; una
   `options.deadline` nel payload non è applicata né rifiutata, mentre una
   chiave di idempotenza nel payload è rifiutata con INVALID_INPUT.
   Riproduzione: messaggio rest.test con `options.deadline` a 300 ms nel
   payload e nessuna metadata di deadline verso un servizio che non risponde:
   il messaggio torna al timeout di richiesta. Scenario `runtime_deadline`.
3. **metrics.requests e metrics.retries azzerati nei fallimenti** (bloccante
   secondo la soglia proposta). Quando l'operazione fallisce per errore di
   trasporto, timeout, cancellazione, deadline, checksum o download
   interrotto, il risultato riporta `requests` e `retries` a 0 anche se il
   servizio ha ricevuto una o più richieste (per esempio 3 tentativi verso un
   servizio che chiude la connessione prima della risposta: 3 richieste
   ricevute, `requests` 0, `retries` 0). Gli errori HTTP dopo i retry sono
   invece contati. Scenari drop_before_response, post_body_then_drop,
   truncated, stall, deadline, cancel, job_cancel, job_deadline,
   download_corrupt, download_cut.
4. **remote_effect più prudente del necessario** (non bloccante). Un
   handshake TLS fallito e un connect timeout, dove nessun byte della
   richiesta HTTP è partito, sono riportati come TRANSPORT_ERROR o TIMEOUT
   con remote_effect unknown e retry quarantine invece di none e safe: un
   chiamante non può ritentarli automaticamente. Il report li conta come
   prudenti.

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
