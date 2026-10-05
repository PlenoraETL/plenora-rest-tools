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

### Dipendenze

- thiserror non è più una dipendenza diretta: Display di EngineError è scritto
  a mano.
