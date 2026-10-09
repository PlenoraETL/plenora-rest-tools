# Changelog

Le modifiche che cambiano il comportamento osservabile, l'API pubblica o la
politica delle dipendenze. La versione dei manifest resta quella dell'ultima
release finché una release non viene preparata.

## Unreleased

### Contratti: plenora-contracts v1.1.0

- Il pin adottato passa da 1e902df alla v1.1.0 di plenora-contracts
  (3c395a8d) in adoption-manifest.json e contracts/upstream/source.json, che
  cita anche il tag. La v1.1.0 discende dal pin precedente e non cambia nessuno
  schema, vettore o binding già pubblicato: i file copiati prima restano
  identici, tranne la specifica dei vettori (nuovo §6 sulle sonde di rifiuto),
  aggiornata.
- Le sonde di rifiuto runtime-probes-v1 e lo schema runtime-probe-v1
  (Runtime Binding 1.0 §11-13, RT-016..RT-023, decisione 0010) sono ora
  normative: passano da contracts/proposte, che non esiste più, a
  contracts/upstream con gli stessi byte. Il test runtime_vectors le esegue
  come contratto adottato (rejection_probes_hold_on_the_rest_request_vector),
  comprese le due sul vettore di rest.upload, e scripts/validate_contracts.py
  le valida contro il loro schema. Il binding runtime non cambia: rispettava
  già la matrice.
- CLI (decisione 0012): le spellature di plenora-rest sono quelle del binding
  comune bindings/cli-v1.json e diventano normative; il catalogo rest-tools
  seleziona la CLI come superficie facoltativa. Nessun cambio del binario.

### Effetto remoto dopo un invio (ERR-014)

- Un errore non riporta più remote_effect `none` se una richiesta
  dell'operazione è già partita, con qualunque metodo e qualunque risposta,
  compresi i redirect e le richieste di token OAuth. Prima un redirect
  cross-origin rifiutato dopo un POST, un URL di polling cross-origin dopo un
  submit accettato o un file sorgente che non si riapriva per un nuovo
  tentativo uscivano con remote_effect `none` e retry `never`. Ora escono con
  remote_effect `unknown` e retry `requires_recovery`; categoria, codice,
  messaggio e dettagli non cambiano (#32).
- La fase resta quella dell'errore (ERR-003): `write` per un file che non si
  riapre, `connect` per un token rifiutato. Solo `validate`, che vuol dire
  «prima di ogni attività di rete», diventa `read`: i redirect e gli URL di
  polling rifiutati nascono mentre il motore interpreta una risposta.
- Cambia anche `PAGINATION_LIMIT_REACHED` di un risultato partial: le pagine
  sono state richieste, quindi l'errore esce con `unknown` e
  `requires_recovery` invece di `none` e `never`; la fase resta `read`.
- In enrich si giudica ogni record sulle richieste partite per quel record (nel
  batch, quelle del suo blocco): un record la cui richiesta non è partita resta
  `none`. Gli errori prima di ogni invio restano `none` e `never`.
- Il contesto è il contatore degli invii che l'esecuzione aveva già, ora anche
  per record. Nessuna variante nuova di EngineError e nessun cambio della
  superficie congelata.
- La deviazione ERR-014 dichiarata con il passaggio alla v1.1.0 è rimossa da
  adoption-manifest.json.

### Deadline sul runtime (RT-023)

- Sul runtime la deadline viaggia solo come metadata
  `plenora.execution.deadline`. Un messaggio con `options.deadline` nel
  payload è rifiutato prima dell'invocazione con il nuovo codice
  RUNTIME_DEADLINE_IN_PAYLOAD (invalid_configuration, validate, none, never),
  anche quando la deadline è sola. Prima veniva applicata. Lo stesso codice
  sostituisce INVALID_INPUT per la deadline presente in entrambi i canali.
  Rust, CLI e SDK Python, che non hanno metadata, continuano a usare
  `options.deadline` (#35).

## 0.3.0 (2026-10-06)

Contiene modifiche incompatibili dell'API Rust e del contratto delle
richieste, raccolte in un'unica rottura.

### Sessioni cookie (incompatibile)

- I cookie vivono in sessioni aperte esplicitamente:
  Engine::open_cookie_session restituisce un handle opaco (CookieSession; in
  Python una stringa da `engine.open_cookie_session()`), la richiesta lo indica
  in `connection.cookies.session`, Engine::close_cookie_session lo chiude.
  CookiePolicy perde `enabled` e `jar_id`: la vecchia forma è INVALID_INPUT.
- L'handle porta slot, generazione, identificativo casuale dell'Engine e valore
  casuale della sessione. Una sessione chiusa o espulsa, un handle di un altro
  Engine o assemblato a mano sono rifiutati con POLICY_VIOLATION prima della
  rete, mai ricreando una sessione vuota.
- La memoria è limitata a `max_cookie_sessions` slot (nuovo campo di
  EngineConfig, default 256); le sessioni finite non lasciano traccia, quindi
  il ricambio di sessioni non esaurisce il motore. Sostituisce il registro di
  jar_id espulsi della correzione precedente. Uno slot con la generazione
  esaurita viene ritirato, non riusato.
- Una richiesta usa per tutta la sua durata la sessione risolta quando è stata
  ammessa: chiudere la sessione mentre la richiesta è in corso rifiuta solo le
  richieste nuove, e lo slot viene riusato soltanto dopo la fine di quella in
  corso. Il valore casuale dell'handle è di 128 bit dalla sorgente del sistema
  (getrandom, già nel grafo tramite uuid, ora dipendenza diretta pinnata).
- L'handle si valida all'inizio dell'operazione, prima della rete e prima che
  lo scope delle credenziali lo tolga dalle richieste cross-origin; queste
  ultime restano legate alla sessione del chiamante e non partono se è finita
  (polling ripreso, cancellazione remota).
- Superficie congelata aggiornata con decisione esplicita: export CookieSession
  ed entrypoint di sessione in compatibility-v1.json e bindings/rust-v1.json.

### Campagna operativa

- Nuovo crate crates/rest-campaign (non pubblicato, nessuna dipendenza nuova)
  con il binario plenora-rest-campaign: fasi smoke, load con iniezione di
  guasti e soak contro un server HTTP locale in-process, campionamento di RSS,
  descriptor e thread (Linux), file temporanei, latenze, throughput ed errori,
  verifica dei criteri di accettazione della roadmap con exit code non zero e
  report JSON e Markdown. Profili in campaign/profiles.json, soglie proposte e
  da approvare in campaign/limits.json, esecuzione con scripts/campaign.sh e
  con il workflow Campaign. Vedi
  [Campagna come codice](docs/roadmap.md#campagna-come-codice).

### Comportamento

- Un null esplicito in value di un parametro, di una trasformazione o in
  default di un output_mapping non è più confuso con l'assenza del campo. Un
  parametro fixed con value null invia null nel body JSON; un parametro fixed
  senza value è rifiutato con INVALID_INPUT. Vedi
  [Null e valori assenti](docs/architecture.md#null-e-valori-assenti).
- Un null in path, query, header, cookie, form, multipart o template raw è
  rifiutato con INVALID_INPUT invece di essere inviato come stringa vuota.
- Null non è più letto come stringa vuota nemmeno nei valori della risposta:
  prefix, suffix e replace su null restituiscono null; una condition su una
  colonna null non si applica; uno status di polling null è INVALID_RESPONSE;
  un job id null è un job id assente. Un value null in una trasformazione
  diversa da default_if_null è INVALID_INPUT.
- Le trasformazioni della risposta sono validate prima dell'esecuzione:
  un'operazione sconosciuta, un argomento mancante o di tipo sbagliato, una
  divisione per zero costante o una condition non riconosciuta (compresi
  apici non chiusi o in eccesso, come `status == 'active`) sono
  INVALID_INPUT. Prima l'operazione sconosciuta lasciava il valore invariato e
  una condition senza operatore applicava sempre la trasformazione.
- Un valore che una trasformazione non sa trattare, o un risultato senza
  rappresentazione esatta (overflow, interi oltre 2^53 in aritmetica float,
  stringhe intere oltre i128, float non finiti), fa fallire il record con
  INVALID_RESPONSE invece di restituire il valore originale o null. Null si
  propaga. Vedi
  [Trasformazioni della risposta](docs/architecture.md#trasformazioni-della-risposta).
- Una condition su una colonna assente o null non applica la trasformazione.
- Un batch flat_array rifiuta i record che non si risolvono in esattamente un
  parametro non null.

### Limiti espliciti (incompatibile)

- La paginazione fermata da max_rows o max_pages mentre la sorgente ha ancora
  dati restituisce un risultato partial: le righe lette più l'errore
  PAGINATION_LIMIT_REACHED (resource_limit, details.max_rows/max_pages). Prima
  il risultato era success con le righe troncate in silenzio. Nuova variante
  EngineError::PaginationLimit. Vedi
  [Paginazione](docs/limiti.md#paginazione).
- Un Retry-After oltre retry.max_retry_after_ms non viene più accorciato al
  massimo per riprovare prima del tempo chiesto dal server: l'operazione
  fallisce con lo status ricevuto.
- Oltre i 128 handle di recovery ammessi dal contratto, il primo errore porta
  details.recoveries_omitted con il numero degli handle lasciati fuori; prima
  venivano scartati in silenzio.
- AGENTS.md raccoglie le regole del repository; docs/limiti.md è il registro
  unico dei limiti, dei comportamenti oltre la soglia e delle deviazioni.

### Parser dell'input remoto (trovati da fuzz e test di proprietà)

- XML: il testo conserva gli spazi attorno ai riferimenti (`Fish &amp; Chips`
  era letto `Fish&Chips`); nel contenuto misto il testo resta quello scritto
  tra i figli, rifilato solo ai bordi.
- XML: contenuto fuori dalla radice (`<a/>junk`, `junk<a/>`, riferimenti o
  CDATA dopo la radice), nomi non UTF-8, nomi fuori dalla grammatica XML (per
  esempio `<@id>`, `<x:#text>`, `<a:>`) e attributi che collidono una volta
  tolto il prefisso (`x:id`, `y:id`) sono INVALID_RESPONSE invece di essere
  scartati, alterati o sovrascritti.
- JSON e NDJSON: i numeri sono letti con arrotondamento corretto; alcuni
  decimali erano letti con un errore di un'unità sull'ultima cifra.
- Header Link: i quoted-pair di una relazione quotata sono risolti
  (`rel="n\ext"` vale `next`) e conta solo il primo `rel` di un link, come
  vuole RFC 8288.
- Retry-After: un numero di secondi oltre il rappresentabile satura all'attesa
  massima (poi limitata da max_retry_after_ms) invece di essere ignorato.
- L'header Cookie inviato elenca le coppie in ordine lessicografico: prima
  l'ordine dipendeva dalle hash map del jar e cambiava da un Engine all'altro.

### Verifica

- Test di proprietà dei parser dell'input remoto e dei riferimenti runtime,
  ciascuno con un oracolo scritto nel test, deterministici (seme e casi
  fissati). Vedi [Fuzz e test di proprietà](docs/development.md#fuzz-e-test-di-proprietà).
- Crate di fuzz in `fuzz/` (workspace e lock propri) con sei target senza rete:
  corpo della risposta, percorsi JSON, header Link, altri header remoti,
  ExecutionRequest e RuntimeMessage. I target raggiungono i parser privati con
  la feature `fuzzing` del crate core: modulo `doc(hidden)`, non pubblico e
  fuori dal contratto v1. Workflow Fuzz: fmt e check dei target su ogni pull
  request, campagna settimanale, manuale e sulle pull request che toccano
  `fuzz/`.

### Runtime e contratti (incompatibile)

- Adottata la revisione 1e902dfa di plenora-contracts. I tre vettori
  runtime-v1 di REST (rest-upload-request, rest-download-success,
  rest-upload-unknown-error) sono copiati in contracts/upstream con il loro
  SHA-256 ed eseguiti attraverso RuntimeBinding (test runtime_vectors), con le
  mutazioni negative dell'instradamento e gli esempi negativi REST del
  contratto.
- Un riferimento runtime (artifact_source, artifact_sink, credential_ref) deve
  essere un riferimento opaco `schema:` o `schema://` secondo la grammatica dei
  contratti. Prima bastava non sembrare un path assoluto, `file:` o `..`: un
  path relativo come `dir/report.csv` o `report.csv` arrivava a
  RuntimeResources. Ora è INVALID_INPUT prima della risoluzione.
- scripts/validate_contracts.py verifica i pin dei file copiati, i vettori
  contro lo schema runtime-vector-v1 e il manifesto di adozione contro lo
  schema v4 e le regole incrociate di ADOPTION.md.

### Binding runtime allineato alla matrice comune (incompatibile)

Le quattro librerie con superficie runtime rispondono ora allo stesso modo agli
stessi casi, secondo Runtime Binding 1.0 §11-13 (RT-016..RT-023) proposti in
plenora-contracts #21 e non ancora normativi; le sonde di quella proposta sono
copiate in contracts/proposte ed eseguite (test
proposed_rejection_probes_hold_on_the_rest_request_vector).

- Rifiuti prima dell'invocazione: fase validate, remote_effect none, retry
  never (P). Categoria (P, R1): `unsupported` per un valore ben formato ma non
  annunciato (capability, versione del binding, operazione, versione
  dell'operazione, input contract, content type), `protocol` per un valore
  assente, malformato o non canonico; codici RUNTIME_UNSUPPORTED e
  RUNTIME_PROTOCOL_VIOLATION. Prima tutti erano INVALID_INPUT,
  invalid_configuration.
- Identità non canoniche (UUID maiuscoli, tra graffe, assenti) e valori di
  metadato non stringa (un `null` come idempotency key) sono `protocol`. Le
  chiavi `plenora.*` che il binding non riserva sono ignorate come membri
  facoltativi. L'ordine delle categorie è quello di RT-018: prima `protocol`
  su tutti i valori riservati, poi `unsupported`, poi `timeout`.
- Metadati del risultato (P, R2): `plenora.message.id` sempre nuovo;
  `plenora.message.causation_id` è il message id della richiesta;
  correlazione, operazione e versione dell'operazione sono copiate byte per
  byte solo se canoniche, altrimenti omesse. Prima un id non canonico veniva
  riflesso (anche come causazione), una correlazione assente sostituita con
  una nuova e una versione assente scritta come "1".
- Deadline: ogni grafia RFC 3339 di UTC (`Z` o `z`, `+00:00`, `t`
  minuscola, frazioni); un offset diverso da zero o `-00:00` è rifiutato
  (`protocol` sul runtime, INVALID_INPUT in ExecutionControl e
  `options.deadline`). Prima un offset qualunque era accettato. Una deadline già scaduta è DEADLINE_EXPIRED
  (timeout, validate, none, never) prima di risolvere credenziali o artefatti;
  prima era TIMEOUT (read, unknown, quarantine) e arrivava dopo la
  risoluzione. Sul runtime una deadline nel payload ora vale; nei metadati e
  nel payload insieme è rifiutata (invalid_configuration).
- Idempotency key vuota, oltre 255 byte o con caratteri non visibili:
  `protocol` prima dell'invocazione.
- Download: un errore di scrittura locale dopo l'invio della richiesta è
  DOWNLOAD_WRITE_FAILED (io, write, unknown, requires_recovery), perché la
  richiesta può aver avuto effetto remoto (un download può usare POST); prima
  FILE_IO con remote_effect none. Se la pubblicazione nel sink è avvenuta e
  fallisce solo la rimozione del file di staging: CLEANUP_AFTER_PUBLISH_FAILED
  (io, cleanup, committed, never).

### API Rust (incompatibile)

- EngineError non contiene più testo di terzi: i campi testuali delle varianti
  sono ErrorDetail, costruibili soltanto da stringhe statiche del motore
  (`"...".into()`); per i body illeggibili conservano solo riga e colonna, per
  l'I/O solo il tipo di errore. CircuitOpen e ChecksumMismatch non hanno più
  campi. Display restituisce il messaggio pubblico statico, uguale a
  payload().message. I nomi esportati e il wire contract non cambiano. Per
  semver 0.x la prossima versione deve essere 0.3.0. Vedi
  [Errori ed effetti remoti](docs/architecture.md#errori-ed-effetti-remoti).
- Il messaggio remoto al percorso error_path e lo status remoto di un job
  fallito non vengono più acquisiti nell'errore. Non erano consegnati al
  chiamante da alcun contratto (il message pubblico era già statico); se
  servissero, andrebbero consegnati come dato remoto in un campo dichiarato del
  risultato, non nell'errore.

### Rilascio

- Le note della GitHub Release sono la sezione datata di questa versione del
  CHANGELOG (scripts/release.py release-notes), non più l'elenco generato dei
  titoli delle pull request: senza sezione, senza data o vuota il workflow
  Release fallisce.

### Parametri numerici senza significato rifiutati (incompatibile)

- Erano sostituiti in silenzio e ora sono INVALID_INPUT prima di ogni
  richiesta: `requests_per_second` non finito o non positivo (ricadeva sul
  rate dell'Engine), `request.timeout_ms` zero, `retry.max_attempts` zero
  (diventava uno), `retry.backoff_factor` non finito o minore di uno
  (diventava uno), `max_rows` o `max_pages` zero nella paginazione (successo
  vuoto). Per l'Engine, `connect_timeout_ms`, `request_timeout_ms` e
  `max_concurrent_requests` a zero (la concorrenza diventava uno) e
  `requests_per_second` a zero (attesa di secoli) fanno fallire ogni
  esecuzione con INVALID_INPUT: `Engine::new` non può fallire.

### Limiti numerici esatti (incompatibile)

- `max_concurrent_requests` sopra `Semaphore::MAX_PERMITS` di tokio
  (`usize::MAX >> 3`) mandava in panic `Engine::new`. Ora `Engine::new` resta
  infallibile e ogni esecuzione lo rifiuta con INVALID_INPUT, come lo zero.
- L'intervallo del rate limiter era `(10^9 / rate)` in virgola mobile
  limitato e convertito: un rate sopra 10^9 dava intervallo zero, un rate
  subnormale circa 584 anni. Ora è `10^9 / rate` ns in aritmetica intera
  esatta, arrotondato per eccesso al nanosecondo; un rate il cui intervallo
  non sta fra 1 ns e `u64::MAX` ns (circa 584 anni) è INVALID_INPUT prima
  della rete, per la connessione e per l'Engine (sopra 10^9). Il limite è
  quello dei nanosecondi in `u64`, non della `Duration` più lunga: 2^-35
  richieste al secondo è rifiutato.
- Il backoff del retry era `base · factor^(n-1)` in virgola mobile: con base
  0 e fattore enorme il prodotto diventava NaN e l'attesa `max_backoff_ms`.
  Ora l'n-esimo retry aspetta `min(max_backoff_ms, floor(D_n))` con
  `D_0 = backoff_base_ms` e `D_{n+1} = D_n · backoff_factor`, in aritmetica
  intera sul valore esatto del fattore e in virgola fissa a 128 bit
  frazionari (troncati a ogni passo). Rispetto a
  `min(max_backoff_ms, floor(base · factor^n))` l'attesa non è mai più lunga
  e al più 1 ms più corta; coincide con un fattore intero, con un fattore
  `p / 2^k` per i primi `128 / k` retry e dal tetto in poi. Base 0 non
  aspetta mai, un valore esatto oltre il tetto aspetta il tetto, e l'attesa
  non diminuisce mai: con base positiva e fattore strettamente maggiore di 1
  cresce fino al tetto, anche se il floor può dare attese consecutive uguali
  (base 1 e fattore 1,5: 1, 1, 2, 3, ... fino a 30 000 ms); con fattore 1
  resta costante. Lo stesso calcolo vale per `polling.interval_backoff`. Un test
  di proprietà lo confronta con `floor(base · factor^n)` calcolato in
  razionali esatti, compreso il fattore `1 + EPSILON`.

### Licenza

- La licenza è proprietaria (LICENSE, come le altre librerie Plenora) e
  non più MIT OR Apache-2.0: `license-file` nei crate, `publish = false`
  per tutto il workspace, `License: Proprietary` nei metadati della wheel.
  cargo-deny ignora le licenze dei soli crate privati del workspace.

### CLI

- Nuova superficie: il binario plenora-rest (crate plenora-rest-cli, non
  pubblicato su crates.io) implementa CLI 2.0 (plenora-cli-v2). Discovery con
  `--help`, `--version --format json` (contratto
  plenora-rest-version-result-v1) e `capabilities --format json` (il documento
  del core con l'interfaccia `cli` e la superficie `cli` su ogni operazione);
  comandi `test`, `generate`, `enrich`, `download` e `upload` con
  `--input REQUEST.json` (anche `-`), `--config ENGINE.json` opzionale e
  `--format json`.
- In modalità JSON un solo documento e un newline su stdout, stderr vuoto,
  exit code proiettato dalla categoria (2, 3, 4, 5, 6, 70, 130); un panic è un
  errore internal senza dettagli. Parser chiuso: comandi e flag sconosciuti,
  `--flag=valore`, flag ripetuti, valori mancanti e posizionali in più sono
  rifiutati con exit 2 e messaggi che non citano gli argomenti.
- Ctrl-C (e SIGTERM su Unix) è una cancellazione cooperativa: errore
  cancelled, exit 130. tokio usa ora anche le feature `signal` e `io-std`;
  Cargo.lock aggiunge signal-hook-registry ed errno (solo Unix).
- La release include plenora-rest-linux-x86_64, costruito nella doppia build
  riproducibile con il suo digest in adoption-manifest.json, e
  plenora-rest-windows-x86_64.exe dal job Windows; entrambi sono in
  SHA256SUMS, nell'SBOM e nelle attestazioni. Il gate anti-panic di Clippy
  copre anche i binari (`--lib --bins`).

### Copertura

- Nuovo workflow Coverage: core Rust, binding PyO3 e SDK Python misurati
  separatamente (cargo-llvm-cov 0.9.1, coverage.py 7.16.1) contro i minimi di
  scripts/coverage_budget.json, con il verificatore fail-closed di
  plenora-database-tools e i suoi self-test.

### Piattaforme

- Windows x86_64 è una piattaforma supportata: il workflow Verify esegue su
  Windows formato, Clippy e test Rust, costruisce la wheel abi3 win_amd64 e la
  prova installata su CPython 3.10-3.14; il workflow Release la costruisce, la
  prova sulla stessa matrice e la include in SHA256SUMS, SBOM e attestazioni.

### Robustezza e gate

- Nessuna indicizzazione o slicing che possa andare in panic nelle
  librerie: i 16 punti trovati da Clippy (engine, json_path,
  response_body, transport) usano accessi controllati, e un invariante
  interno violato diventa RUNTIME_ERROR. Nell'enrichment concorrente un
  esito mancante, ripetuto o fuori indice fa fallire l'operazione invece
  di perdere o duplicare un record. Il gate anti-panic gira nel Docker e
  su Windows.
- Un percorso JSON malformato in records_path, error_path,
  output_mapping, iterate_on, batch.output_path, nei percorsi del polling
  o della paginazione è INVALID_INPUT prima di ogni richiesta. Prima non si
  risolveva mai e veniva letto come campo assente (null o default).
- rust-toolchain.toml fissa il compilatore 1.98.1 per gate locali, CI
  Linux e Windows e build di release; l'immagine del gate Docker è
  rust:1.98.1 e lo stage msrv resta su 1.85.1.
- `unsafe_code = "forbid"` vale per tutto il workspace ([workspace.lints]),
  test compresi; maturin è fissato a 1.14.1 anche in build-system.requires;
  rustdoc gira con `-D warnings` nel gate.
- plenora-rest-core dichiara `#![deny(missing_docs)]`: tutta l'API
  pubblica (tipi del contratto e loro campi, errori, capability, runtime
  binding, controlli di esecuzione, Engine) è documentata con default,
  unità, limiti ed errori reali; un elemento pubblico nuovo senza
  documentazione non compila. Nessun cambiamento di superficie.

### Correzioni trovate dalla campagna operativa

- `options.deadline` vale per ogni punto d'ingresso: Engine::execute_with_control
  e il RuntimeBinding (deadline nel payload) la ignoravano.
- Un risultato fallito riporta in metrics.requests e metrics.retries le
  richieste e i retry davvero inviati, compresa la cancellazione remota dei
  job; prima valevano 0 dopo errori di trasporto, timeout, deadline o
  cancellazione.

### MSRV e dipendenze

- MSRV pubblicata 1.98 (era 1.85.1), come plenora-database-tools; lo stage
  msrv del gate Docker usa rust:1.98.0. Decisione dell'utente del 2026-10-06.
- `time` passa da 0.3.45 a 0.3.55 (pin esatto), che corregge
  RUSTSEC-2026-0009. L'esenzione di quell'advisory in deny.toml, giustificata
  solo dalla MSRV 1.85.1, è rimossa: nessun advisory è esentato.

### Dipendenze

- thiserror non è più una dipendenza diretta: Display di EngineError è scritto
  a mano.
- serde_json attiva la feature float_roundtrip (stesso pin, Cargo.lock
  invariato).
- proptest =1.11.0 è una dev-dependency, senza feature di default.
- `fuzz/Cargo.lock` è un grafo separato, controllato dall'Audit con la stessa
  policy; libfuzzer-sys =0.4.13 vi entra come unica dipendenza propria, con
  un'eccezione di licenza NCSA limitata a quel crate in deny.toml.
