# Changelog

Le modifiche che cambiano il comportamento osservabile, l'API pubblica o la
politica delle dipendenze. La versione dei manifest resta quella dell'ultima
release finché una release non viene preparata.

## 0.3.0 (non ancora rilasciata)

La prossima versione è 0.3.0: contiene modifiche incompatibili dell'API Rust e
del contratto delle richieste, raccolte in un'unica rottura.

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

### Correzioni trovate dalla campagna operativa

- `options.deadline` vale per ogni punto d'ingresso: Engine::execute_with_control
  e il RuntimeBinding (deadline nel payload) la ignoravano.
- Un risultato fallito riporta in metrics.requests e metrics.retries le
  richieste e i retry davvero inviati, compresa la cancellazione remota dei
  job; prima valevano 0 dopo errori di trasporto, timeout, deadline o
  cancellazione.

### Dipendenze

- thiserror non è più una dipendenza diretta: Display di EngineError è scritto
  a mano.
