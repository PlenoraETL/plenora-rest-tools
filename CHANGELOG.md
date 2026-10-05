# Changelog

Le modifiche che cambiano il comportamento osservabile, l'API pubblica o la
politica delle dipendenze. La versione dei manifest resta quella dell'ultima
release finché una release non viene preparata.

## Non rilasciato

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

### Dipendenze

- thiserror non è più una dipendenza diretta: Display di EngineError è scritto
  a mano.
