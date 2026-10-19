# Limiti e deviazioni

Registro unico di dove le garanzie del motore si fermano: ogni limite di
memoria, tempo e risorse con il suo default e con quello che succede oltre, i
limiti dichiarati che il motore applica senza errore, e le deviazioni dai
contratti adottati. I valori vengono dal codice (`EngineConfig::default`,
`ConnectionConfig` e le costanti dei moduli); se un valore qui diverge dal
codice, vale il codice e questo documento Ã¨ un difetto da correggere.

La regola che governa la tabella: oltre un limite il motore **rifiuta con un
errore tipizzato** oppure **rende esplicito nel risultato** che cosa manca. Un
limite che si limita a tagliare in silenzio non Ã¨ ammesso; le poche eccezioni
volute sono nella sezione [Limiti applicati senza errore](#limiti-applicati-senza-errore),
ciascuna con il motivo.

## Limiti del motore (EngineConfig)

| Campo | Default | Che cosa limita | Oltre il limite |
| --- | --- | --- | --- |
| `connect_timeout_ms` | 5 000 | apertura di una connessione | `TIMEOUT` o `TRANSPORT_ERROR`, categoria timeout/transient |
| `request_timeout_ms` | 30 000 | una richiesta HTTP intera (sovrascrivibile per richiesta con `request.timeout_ms`) | `TIMEOUT`, remote_effect `unknown`, retry `quarantine` |
| `max_request_bytes` | 32 MiB | body costruito in memoria (JSON, form, multipart, raw) | `REQUEST_TOO_LARGE` prima dell'invio, `details.limit_bytes` |
| `max_response_bytes` | 32 MiB | body di risposta letto in memoria | `RESPONSE_TOO_LARGE`, la lettura si interrompe al limite |
| `max_file_transfer_bytes` | 1 GiB | upload e download in streaming (abbassabile per richiesta con `input.file.max_bytes`) | `FILE_TOO_LARGE`; un download non pubblica mai il file di staging incompleto |
| `max_concurrent_requests` | 64 | richieste HTTP in volo nello stesso Engine | le richieste in piÃ¹ **aspettano** un permesso (entro deadline e cancellazione); 0, o un valore sopra `Semaphore::MAX_PERMITS` di tokio (`usize::MAX >> 3`), fa fallire ogni esecuzione con `INVALID_INPUT`; `Engine::new` non va mai in panic |
| `requests_per_second` | nessuno | ritmo delle richieste (sovrascrivibile per connessione) | le richieste aspettano; l'attesa Ã¨ in `metrics.rate_limit_wait_ms`; l'intervallo Ã¨ `10^9 / rate` ns calcolato esattamente e arrotondato per eccesso al nanosecondo; un rate il cui intervallo non sta fra 1 ns e `u64::MAX` ns, circa 584 anni (0 o sopra 10^9 nell'Engine; non finito, non positivo, sopra 10^9 o sotto circa 5,4 Â· 10^-11, per esempio 2^-35, nella connessione) Ã¨ `INVALID_INPUT` prima della rete, mai limitato in silenzio |
| `max_pooled_origins` | 128 | client HTTP tenuti nel pool, uno per origin e configurazione | il meno recente viene chiuso; la richiesta successiva verso quell'origin ne crea uno nuovo |
| `pool_max_idle_per_host` | 50 | connessioni inattive per host | chiuse dal pool |
| `pool_idle_timeout_ms` | 90 000 | vita di una connessione inattiva | chiusa dal pool |
| `max_cookie_sessions` | 256 | sessioni cookie aperte | si espelle la sessione inattiva usata meno di recente, il cui handle Ã¨ poi rifiutato con `POLICY_VIOLATION`; se tutte sono in uso, aprirne una Ã¨ `POLICY_VIOLATION` |
| `max_cache_entries` / `max_cache_bytes` | 1 024 / 64 MiB | cache HTTP | espulsione LRU; una risposta piÃ¹ grande di `max_cache_bytes` non entra in cache; con uno dei due a 0 la cache Ã¨ spenta |
| `max_circuit_origins` | 256 | stati di circuit breaker (origin + gruppo) | si dimentica lo stato meno recente; con 0, chiedere un circuit breaker Ã¨ `POLICY_VIOLATION` |
| `max_idempotency_keys` | 4 096 | chiavi di idempotenza ricordate con l'impronta dell'input | si dimentica la piÃ¹ vecchia: il riuso di quella chiave con un input diverso non Ã¨ piÃ¹ riconoscibile in questo processo (vedi sotto); con 0, usare una chiave Ã¨ `POLICY_VIOLATION` |

## Limiti per richiesta (ConnectionConfig e opzioni)

| Campo | Default | Oltre il limite |
| --- | --- | --- |
| `request.max_redirects` | 5 (redirect spenti per default) | `INVALID_RESPONSE`, nessun redirect seguito oltre il limite |
| `retry.max_attempts` | 1 | si restituisce l'ultimo esito; i tentativi non superano mai il limite; 0 Ã¨ `INVALID_INPUT` |
| `retry.max_backoff_ms` | 30 000 | un'attesa il cui valore esatto lo supera aspetta questo valore |
| `retry.backoff_factor` | 2 | non finito o minore di 1: `INVALID_INPUT`. L'n-esimo retry aspetta `min(max_backoff_ms, floor(backoff_base_ms Â· backoff_factor^n))`, calcolato in aritmetica intera con 128 bit frazionari: mai piÃ¹ lungo, al piÃ¹ 1 ms piÃ¹ corto, uguale con fattore intero e dal tetto in poi; l'attesa non diminuisce mai; con base positiva e fattore strettamente maggiore di 1 cresce fino a raggiungere il tetto, ma il floor puÃ² dare attese consecutive uguali (base 1 e fattore 1,5: 1, 1, 2, 3, ...); con fattore 1 resta costante; con base 0 nessun retry aspetta |
| `request.timeout_ms` | timeout dell'Engine | 0 Ã¨ `INVALID_INPUT`; lo stesso per `connect_timeout_ms` e `request_timeout_ms` a 0 nell'Engine, a ogni esecuzione |
| `retry.max_retry_after_ms` | 300 000 | un `Retry-After` piÃ¹ lungo, anche oltre il rappresentabile, **non** viene accorciato: niente nuovo tentativo, l'operazione fallisce con lo status HTTP ricevuto |
| `pagination.page_size` | 100 | 0 Ã¨ `INVALID_INPUT` |
| `pagination.max_rows` | 10 000 | vedi [Paginazione](#paginazione); 0 Ã¨ `INVALID_INPUT` |
| `pagination.max_pages` (cursor, link, header_link) | 100 | vedi [Paginazione](#paginazione); 0 Ã¨ `INVALID_INPUT` |
| `polling.max_attempts` | 60 | `POLLING_TIMEOUT` con l'handle di recovery del job; 0 Ã¨ `INVALID_INPUT` |
| `polling.max_wait_ms` | nessuno | `POLLING_TIMEOUT`; 0 Ã¨ `INVALID_INPUT` |
| `polling.max_interval_ms` | 30 000 | l'intervallo con backoff (`interval_backoff`, stessa aritmetica esatta del retry) non lo supera |
| `polling.cancel.timeout_ms` | 5 000 | la cancellazione remota best-effort smette di aspettare; 0 Ã¨ `INVALID_INPUT` |
| `batch.max_size` | 100 | i record vengono divisi in blocchi; 0 Ã¨ `INVALID_INPUT` |
| `options.enrichment_concurrency` | 1 | record arricchiti in parallelo, ordine dell'output sempre quello dell'input |
| `options.idempotency_key` | â€” | da 1 a 255 byte ASCII visibili, altrimenti `INVALID_INPUT` |
| `polling.resume.job_id` | â€” | da 1 a 512 caratteri non di controllo, altrimenti `INVALID_INPUT` |
| riferimenti runtime (`artifact_source`, `artifact_sink`, `credential_ref`) | â€” | al piÃ¹ 512 byte e nella forma opaca del contratto, altrimenti `INVALID_INPUT` prima di `RuntimeResources` |

### Paginazione

`max_rows` e `max_pages` sono tetti di sicurezza, non un modo di chiedere un
campione. Se la paginazione si ferma a uno dei due mentre la sorgente ha
ancora dati (righe dell'ultima pagina lasciate fuori, una pagina piena
all'ultimo posto disponibile, un cursore o un link successivo ancora
presente), il risultato Ã¨ **partial**: le righe lette sono in `output` e
`errors` contiene `PAGINATION_LIMIT_REACHED` (categoria `resource_limit`,
fase `read`, remote_effect `unknown` e retry `requires_recovery`, perchÃ© le
pagine sono state richieste, vedi
[Errori ed effetti remoti](architecture.md#errori-ed-effetti-remoti);
`details.max_rows` e, per cursor e link, `details.max_pages`). La paginazione Ã¨ completa, e il risultato `success`,
solo quando Ã¨ la sorgente a dire che i dati sono finiti: una pagina corta,
nessun cursore o link successivo, oppure un cursore o link giÃ  seguito.

In modalitÃ  page la dimensione della pagina resta `page_size` su ogni
richiesta, anche l'ultima: le righe oltre `max_rows` si tagliano localmente.
Chiedere una pagina piÃ¹ piccola cambierebbe la porzione di dati restituita
(la pagina 2 di dimensione 1 Ã¨ la seconda riga, non la terza).

Caso limite dichiarato: in modalitÃ  offset e page, una sorgente con
esattamente `max_rows` righe e l'ultima pagina piena viene segnalata come
limite raggiunto, perchÃ© il motore non manda una richiesta in piÃ¹ per
scoprirlo. Ãˆ un falso allarme esplicito, mai un troncamento silenzioso.

## Limiti interni

| Limite | Valore | Oltre il limite |
| --- | --- | --- |
| handle di recovery in un risultato | 128 (limite del contratto) | i primi 128 in ordine di URL di polling; il primo errore porta `details.recoveries_omitted` con il numero degli handle lasciati fuori |
| token OAuth in cache | 256 | espulso il piÃ¹ vicino alla scadenza; la richiesta successiva ne ottiene uno nuovo |
| cancellazione remota dei job | 5 s complessivi, 8 in parallelo | i job non cancellati in tempo restano negli handle di recovery |
| lease della sonda di un circuito semiaperto | stimata dalla policy, al piÃ¹ 15 min | una sonda abbandonata non tiene il circuito aperto oltre la lease |
| arrotondamento (`round`) | 15 decimali | `INVALID_INPUT` in validazione |
| aritmetica delle trasformazioni | interi esatti fino a 2^53 in float, stringhe intere entro i128 | il record fallisce con `INVALID_RESPONSE`, mai un valore approssimato |

## Limiti applicati senza errore

Comportamenti voluti in cui il motore non restituisce un errore, con il
motivo. Sono gli unici.

- **Set-Cookie oltre 8 KiB**: l'intestazione viene ignorata. Ãˆ un limite di
  risorsa sullo stato tenuto dal motore per conto di un servizio remoto, non
  un dato del chiamante; un cookie legittimo Ã¨ molto piÃ¹ piccolo. Effetto
  osservabile: le richieste successive della sessione non portano quel cookie.
- **Chiavi di idempotenza dimenticate**: oltre `max_idempotency_keys` la
  chiave piÃ¹ vecchia esce dal registro. Il profilo REST chiede il rifiuto del
  riuso con input diverso solo finchÃ© la chiave Ã¨ nell'ambito locale; la
  deduplicazione durevole Ã¨ del servizio remoto o del runtime.
- **Stato dei circuiti dimenticato**: oltre `max_circuit_origins` si perde lo
  stato meno recente, e il primo errore successivo verso quell'origin riparte
  dal circuito chiuso.
- **Attese** (`max_concurrent_requests`, `requests_per_second`): non sono
  rifiuti ma code, sempre entro deadline e cancellazione della richiesta.

## Garanzie con un confine dichiarato

- **file_root**: la verifica avviene sul path canonicalizzato prima
  dell'apertura; la sostituzione concorrente di una directory intermedia con un
  collegamento simbolico non Ã¨ impedita. La radice non va condivisa con
  processi non fidati ([architettura](architecture.md#artifact-e-streaming)).
- **SHA-256 della sorgente di un upload**: ricalcolato a fine trasferimento,
  rileva una modifica ma non una modifica annullata prima del ricalcolo.
- **Fase di un upload interrotto**: un timeout durante un upload Ã¨ riportato
  con fase `read` (il trasporto non distingue l'invio del body dall'attesa
  della risposta); remote_effect resta `unknown` e il retry non Ã¨ mai
  automatico.

## Deviazioni dai contratti adottati

Il registro per requisito è `contracts/adoption-requirements.json`; il manifesto
riporta le deviazioni nel formato comune v4. Il suo stato `conforming` va letto
insieme alle deviazioni: non attesta conformità al requisito deviato.

| Regola | Ambito | Hazard | Rientro |
| --- | --- | --- | --- |
| RT-024 | destinazione runtime REST, materializzazione aggregata del risultato JSON | manca un budget complessivo in byte: esaurire l'allocazione può terminare il processo senza errore tipizzato; non rilevabile prima dell'invocazione | serializzazione aggregata limitata e budget fornito dall'host, con prove sotto/alla/sopra soglia e conservazione degli effetti già avvenuti |

I limiti già applicati a risposta, file e paginazione producono
`resource_limit`; sul runtime anche un risultato parziale che contiene questo
errore diventa un envelope di errore, mai un successo troncato. Fase, effetto e
retry restano quelli osservati dal motore; il conteggio dei recovery omessi si
conserva anche quando l'errore di limite non è il primo.

REST è una destinazione: RT-026/027 (negoziazione, inoltro e registro durevole
per identità) sono responsabilità dell'host e non sono implementati qui.
I limiti del trasporto vanno dichiarati e applicati dall'host. Discovery annuncia
Runtime Binding 2.0, mentre envelope e selettori rimangono v1.

Public Catalogs 2.0 è adottato per CAT-001/002 sul catalogo e profilo REST v1
esistenti, immutati: la revisione adottata non pubblica successori per REST.
Il documento v1 resta soggetto al suo contratto v1; non è rinominato v2.
CAT-003 non trova assenze intenzionali nel profilo REST. Arrow, geometria,
INTEROP-001..005 e l'adapter REST-to-Arrow sono non applicabili: REST scambia
JSON e artifact opachi; l'adapter appartiene all'applicazione.

I digest degli artifact nel manifesto restano quelli della release 0.3.0
precedente: questa adozione nel worktree non è una release qualificata. Prima
di pubblicarla occorre ricostruire gli artifact e registrarne i digest reali
con il gate di release; nessun digest è stato inventato o aggiornato senza build.

Decisioni del maintainer registrate nei contratti del componente, non
deviazioni dai contratti comuni:

| Regola | Ambito | Rischio | Rientro |
| --- | --- | --- | --- |
| superficie Rust v1 congelata (`contracts/compatibility-v1.json`) | 0.3.0: EngineError con campi testuali opachi; sessioni cookie (`CookieSession`, `open_cookie_session`, `close_cookie_session`) | un consumatore Rust 0.2.x non compila senza modifiche | cambio unico deciso per la 0.3.0, vedi [contratti](../contracts/README.md#compatibilitÃ ); le prossime modifiche incompatibili richiedono contratti v2 |
| null e valori assenti | fino a 0.2.2 un parametro fixed con value null veniva omesso | un consumatore che contava sull'omissione invia ora null | chiuso in 0.3.0, vedi [architettura](architecture.md#null-e-valori-assenti) |

## Riferimenti

- [Architettura](architecture.md)
- [Contratti](../contracts/README.md)
- [Sviluppo e gate](development.md)
- [Roadmap](roadmap.md)
