# Architettura

Plenora REST Tools è progettata come componente black-box: l'host fornisce
contratti e risorse autorizzate, il componente restituisce risultati o errori
tipizzati. Tipi HTTP interni, client, connessioni e dettagli sensibili non
attraversano il confine pubblico.

## Principi

1. Il core è generico: nessun provider ha codice dedicato.
2. Il comportamento esterno è descritto da contratti versionati.
3. L'Engine possiede trasporto, resilienza e stato di connessione.
4. Le configurazioni pericolose richiedono autorizzazione esplicita.
5. Rust, CLI, Python e runtime espongono le stesse cinque operazioni
   normative.
6. Un cambiamento breaking crea una nuova versione del contratto.

## Componenti

~~~text
                                  +-----------------------+
Host Rust ----------------------> |                       |
Processo -> CLI plenora-rest ---> |                       |
SDK Python -> binding PyO3 -----> | plenora-rest-core     | -> HTTP/TLS/DNS
Runtime -> envelope + risorse --> | Engine persistente    | -> servizio REST
                                  |                       |
                                  +-----------------------+
                                     | risultati/errori
                                     | artifact autorizzati
~~~

| Componente | Responsabilità |
| --- | --- |
| crates/rest-engine-core | contratti Rust, Engine, trasporto, runtime binding ed errori |
| crates/rest-cli | binario plenora-rest: parser chiuso, envelope CLI 2.0, exit code, Ctrl-C |
| crates/rest-engine-python | estensione nativa PyO3 ABI3 |
| python/plenora_rest | facciata Python sincrona e tipi pubblici |
| contracts | schemi component-owned, binding e baseline compatibile |
| scripts | verifica, validazione e release riproducibile |

## Ownership

L'host possiede:

- selezione della capability e dell'operazione;
- configurazione del servizio espressa dal contratto;
- record e parametri di input;
- deadline, cancellazione e chiave di idempotenza;
- autorizzazione a reti, proxy, file e metodi custom;
- risoluzione di credential_ref e riferimenti artifact nel runtime.

L'Engine possiede:

- costruzione e serializzazione delle richieste;
- DNS, policy di rete, TLS, proxy e pool HTTP;
- autenticazione e cache dei token;
- retry, rate limit, cache, cookie e circuit breaker;
- paginazione, polling, batch, concorrenza e trasformazioni;
- limiti di memoria e trasferimento;
- redazione di risultati ed errori.

Il servizio remoto possiede:

- semantica applicativa;
- deduplicazione durevole delle chiavi di idempotenza;
- stato persistente dei job;
- code o broker interni;
- disponibilità e consistenza dei dati restituiti.

## Lifecycle dell'Engine

Engine è persistente e riutilizzabile. La stessa istanza conserva pool,
limiter, cache, cookie jar, circuit breaker e stato necessario a impedire
conflitti locali di idempotenza. Creare un Engine per ogni richiesta elimina
questi vantaggi.

close è idempotente. Dopo la chiusura, nuove esecuzioni falliscono localmente.
La cancellazione è cooperativa e può essere condivisa in modo thread-safe.
L'SDK Python implementa un context manager che richiama close in uscita.

L'API Python è sincrona; il binding nativo esegue il motore asincrono Rust senza
esporre un event loop al chiamante.

## Pipeline di esecuzione

Una richiesta segue queste fasi logiche:

1. deserializzazione e validazione del contratto;
2. applicazione delle autorizzazioni dell'EngineConfig;
3. risoluzione delle risorse runtime, se presenti;
4. costruzione di URL, parametri, header e body;
5. applicazione stabile dell'idempotenza;
6. acquisizione del rate limiter e controllo del circuit breaker;
7. invio HTTP con timeout, retry e redirect policy;
8. eventuale paginazione o polling;
9. parsing, iterazione, mapping e trasformazione;
10. produzione di output, metriche ed errori pubblici.

Il motore valida prima della rete tutte le condizioni verificabili localmente.
Una configurazione incoerente non deve produrre effetti remoti.

## Operazioni

rest.test esegue una singola interazione destinata a verificare configurazione
e accessibilità.

rest.generate produce record da una o più risposte. Può applicare paginazione,
iterazione annidata, mapping e trasformazioni.

rest.enrich associa ogni record di input a una richiesta o a un batch. La
concorrenza è limitata dalla configurazione e l'ordine finale rimane quello
dell'input.

rest.download invia la richiesta e trasferisce il body verso un artifact
usando streaming. Il file finale viene pubblicato soltanto dopo completamento
e controlli di integrità.

rest.upload legge un artifact in streaming e lo invia come body raw o parte
multipart. I campi regolari restano gestiti dal contratto.

## Configurazione dei provider

Una configurazione di servizio è composta da dati:

- URL e metodo;
- autenticazione o credential_ref;
- parametri statici e mappati;
- request e response format;
- retry, rate limit e circuit breaker;
- paginazione, polling o batch;
- regole di mapping e trasformazione.

Questa configurazione può vivere in Plenora, in un catalogo applicativo o nel
chiamante. Non viene compilata nel core. Un comportamento proprietario ancora
esprimibile tramite HTTP deve essere modellato estendendo un contratto
versionato; un protocollo non HTTP appartiene a una capability separata.

### Null e valori assenti

Nei campi il cui valore è un JSON arbitrario (value di un parametro, value di
una trasformazione, default di un output_mapping) un null esplicito e un campo
assente sono distinti: null è un valore che il chiamante ha scritto, l'assenza
no.

- Un parametro fixed con value null invia null; un parametro fixed senza value
  è un errore di configurazione, non un parametro opzionale omesso.
- Un parametro mapped senza valore nella sorgente e senza value resta assente
  (errore MISSING_PARAMETER se required); con value null usa null.
- Null resta tale soltanto in un body JSON. Path, query, header, cookie,
  campi form e multipart e il template del body raw sono testo, e il testo non
  ha una grafia per null: un null in quelle posizioni, anche dentro un array o
  un oggetto, rifiuta la richiesta con INVALID_INPUT prima di qualunque attività
  di rete, invece di inviare una stringa vuota.

Lo stesso vale per i valori della risposta. Una trasformazione prefix, suffix
o replace su una sorgente null restituisce null, non una stringa costruita da
""; una condition su una colonna assente o null non è soddisfatta né da `==`
né da `!=`; uno status di polling null è un errore INVALID_RESPONSE invece di
coincidere con un valore vuoto configurato; un job id null equivale a un job id
assente e non viene inserito in un URL. Un value null in una trasformazione è
ammesso solo per default_if_null: altrove la richiesta è rifiutata con
INVALID_INPUT prima della rete.

Nei campi opzionali con un tipo proprio (per esempio request.timeout_ms,
response.records_path, response.error_path, response.success_when) null
equivale all'assenza del campo, come negli schemi v1 che dichiarano quei campi
con tipo ["…", "null"]. Per success_when questo significa nessuna condizione.

Deviazione dichiarata: fino a 0.2.2 un fixed con value null veniva omesso e un
null in una posizione testuale veniva inviato come stringa vuota. Gli schemi v1
ammettevano già entrambe le forme; cambia la semantica, non lo schema.

### Trasformazioni della risposta

Le trasformazioni sono validate prima di qualunque attività di rete. Sono
errori di configurazione (INVALID_INPUT): un'operazione sconosciuta, column o
source vuoti, un argomento mancante o del tipo sbagliato (add, subtract,
multiply e divide richiedono un numero; divide non accetta zero; round accetta
un intero di decimali da 0 a 15; prefix e suffix una stringa, un numero o un
booleano; replace un oggetto con find non vuoto e replace stringa;
default_if_null un valore; le conversioni di temperatura, uppercase e lowercase
nessun valore) e una condition che non sia `colonna == valore` o
`colonna != valore`, dove il valore è racchiuso in una sola coppia di apici
uguali (`'attivo'`, `"attivo"`) oppure è nudo senza apici né operatori. Un
apice non chiuso (`status == 'active`) o in eccesso è un errore, non un
valore da confrontare. L'operatore è il primo `==` o `!=` dopo il nome della
colonna: dentro un letterale tra apici `==` e `!=` sono testo
(`status != 'a==b'` confronta con `a==b`).

Durante l'esecuzione null si propaga: ogni operazione tranne default_if_null
trasforma null in null. Un valore che l'operazione non sa trattare fa fallire
il record con INVALID_RESPONSE invece di essere lasciato invariato o sostituito
da null: un operando non numerico per un'operazione numerica, un non-stringa
per uppercase e lowercase, un array o un oggetto per prefix, suffix e replace.

L'aritmetica sugli interi è esatta. Un risultato che non ha una
rappresentazione esatta fallisce con INVALID_RESPONSE: un intero oltre i64 e
u64, un intero oltre 2^53 combinato con un float o diviso con resto, una stringa
intera oltre i128, un risultato float non finito. Un numero scritto come stringa
viene letto come numero e il risultato è un numero JSON.

Una condition su una colonna assente o null non è soddisfatta né da `==` né da
`!=`: il confronto è indeterminato, come in SQL, e la trasformazione non viene
applicata.

Limite dichiarato: il parser JSON legge un intero oltre u64 scritto come numero
(non come stringa) in un f64 prima che le trasformazioni lo vedano. Il valore è
già arrotondato all'ingresso e le trasformazioni non possono accorgersene. Un
servizio che invia identificativi oltre u64 deve inviarli come stringhe.

### Batch flat_array

Con input_format flat_array ogni record deve risolversi in esattamente un
parametro non null, che diventa l'elemento dell'array. Un record con più
parametri o con null è rifiutato con INVALID_INPUT, con il suo input_index, e
non entra nel batch: prima contribuiva il primo valore non null in ordine di
chiave, o niente, spostando l'allineamento dei record successivi.

## Job REST asincroni e code

Il polling copre servizi che rispondono alla submit con un job id o una
Location e rendono lo stato interrogabile via HTTP. Il contratto può descrivere
stato terminale, URL del risultato, intervallo, timeout, resume e
cancellazione remota.

In caso di deadline, cancellazione o fallimento durante il polling, il
risultato può includere un AsyncJobRecovery limitato. Il resume usa il job id
esistente e non ripete la submit.

Questo modello copre anche un backend basato su code quando la coda è un
dettaglio interno del servizio REST. Collegarsi direttamente a Celery, Redis,
RabbitMQ o SQS non è responsabilità di questa libreria.

## Artifact e streaming

I payload ordinari rispettano max_request_bytes e max_response_bytes. Upload e
download usano un limite separato, max_file_transfer_bytes, e non devono
caricare l'intero artifact in memoria.

I trasferimenti locali richiedono sia allow_file_transfers sia una file_root
configurata. Senza file_root non esiste un confine da applicare, quindi la
richiesta viene rifiutata; path assoluti e relativi vengono comunque risolti e
verificati all'interno della radice.

Il confinamento vale nei confronti del chiamante, non di un processo che possa
scrivere concorrentemente dentro la radice: la verifica avviene sul path
canonicalizzato prima dell'apertura, quindi la sostituzione concorrente di una
directory intermedia con un collegamento simbolico non è impedita. La file_root
deve essere una directory non condivisa con processi non fidati.

Per i download:

- i byte vengono scritti in un file di staging;
- overwrite deve essere esplicito;
- il resume usa Range e validatori coerenti;
- dimensione e SHA-256 possono essere verificati;
- un output incompleto non viene promosso a risultato finale;
- il file di staging viene rimosso anche quando il trasferimento viene
  cancellato o interrotto da una deadline.

La promozione finale dello staging non viene annullata: dopo che il rename è
andato a buon fine non è possibile stabilire in modo portabile che il path
punti ancora al file scritto dal motore, quindi un errore successivo può
lasciare un target completo invece di distruggere un file di un altro processo.

Per gli upload il motore ricalcola lo SHA-256 della sorgente al termine del
trasferimento e fallisce se differisce da quello dichiarato. È un rilevamento
best-effort, non una garanzia: la sorgente resta un file condiviso che il
trasporto riapre a ogni tentativo, quindi una modifica annullata prima del
ricalcolo non viene osservata. Per una garanzia forte la sorgente non deve
essere scrivibile da altri processi durante il trasferimento.

Nel runtime il payload contiene un riferimento opaco. RuntimeResources risolve
il riferimento verso un path autorizzato soltanto all'interno del processo. Il
path non viene incluso nel risultato pubblico.

## Sicurezza

EngineConfig blocca per default reti private, file transfer, proxy, cookie
persistenti e opzioni di trasporto pericolose. I metodi custom richiedono una
allowlist.

Il resolver DNS verifica gli indirizzi prima della connessione. I redirect
sono gestiti dal motore, disabilitati per default e limitati alla stessa
origin. Il client sottostante non applica proxy ambientali o redirect
automatici.

La cache HTTP è isolata per chiamante, non solo per URL. Nella chiave entrano
anche autenticazione, identità TLS client, proxy e policy dei redirect. Un
certificato client conta come autenticazione: cachearne la risposta richiede
allow_authenticated come per un bearer token. Il pool di connessioni era già
isolato, ma la cache viene consultata prima del pool e richiede quindi lo stesso
isolamento.

Cache e cookie store non si combinano: abilitarli insieme è rifiutato con una
violazione di policy. Una entry appartiene alla sessione che l'ha prodotta, ma
il jar cambia mentre l'operazione è in corso — un retry o un redirect possono
acquisire una sessione nuova dopo il calcolo della chiave, e i cookie scadono da
soli senza nulla da osservare. Rappresentare la sessione nella chiave darebbe
una garanzia solo apparente, quindi la combinazione resta esclusa finché il
motore non possiede uno store che possa fissare i cookie per singolo hop.

I cookie vivono soltanto dentro sessioni aperte esplicitamente. Il chiamante
apre una sessione con Engine::open_cookie_session (in Python
`engine.open_cookie_session()`), riceve un handle opaco e lo indica nella
richiesta come `connection.cookies.session`; la chiude con
close_cookie_session. Senza sessione la richiesta non porta cookie del motore.
Sul confine runtime l'handle viaggia come stringa nel payload: le sessioni le
apre e le chiude l'host che possiede l'Engine.

Il motore tiene al massimo `max_cookie_sessions` sessioni (256 per default),
una per slot. L'handle indica lo slot e la sua generazione, più un
identificativo casuale dell'Engine e un valore casuale della sessione di 128
bit, letti dalla sorgente casuale del sistema (se la sorgente fallisce, aprire
una sessione è un errore esplicito). Quando
una sessione finisce, perché chiusa o espulsa, la generazione dello slot
avanza: ogni copia dell'handle viene da quel momento rifiutata con
POLICY_VIOLATION prima di qualunque attività di rete, inclusa l'acquisizione di
un token OAuth, e non raggiunge mai una sessione vuota al suo posto. Chiudere
un handle già finito è un errore, non un'operazione nulla. Un handle di un
altro Engine, o assemblato a mano con slot e generazione giusti ma senza il
valore casuale, è rifiutato allo stesso modo; un handle malformato è
INVALID_INPUT.

La memoria è limitata agli slot: il motore non ricorda le sessioni finite, e
un numero qualsiasi di sessioni aperte una dopo l'altra non esaurisce nulla.
Se uno slot arrivasse all'ultima generazione rappresentabile verrebbe ritirato
invece di ripartire da zero, perché un vecchio handle potrebbe portare di nuovo
una generazione valida; con tutti gli slot ritirati l'apertura fallisce in modo
esplicito.

Una richiesta risolve l'handle una sola volta, quando viene ammessa, prima di
qualunque attività di rete; da lì in poi usa per tutta la sua durata il jar
ottenuto all'ammissione, senza risolvere di nuovo l'handle. Chiudere una
sessione mentre una richiesta ammessa è ancora in corso (per esempio in attesa
di un token OAuth) rende subito stantio l'handle per le richieste nuove, che
vengono rifiutate prima della rete, ma la richiesta in corso completa con la
sua sessione. Lo slot viene liberato, e la sua generazione avanza, soltanto
dopo che l'ultima richiesta in corso lo ha rilasciato: fino ad allora non viene
riusato.

L'handle del chiamante si valida una volta all'inizio dell'operazione, prima di
qualunque attività di rete e prima che lo scope delle credenziali possa toglierlo
da una richiesta di follow-up verso un'altra origin. Anche quando lo scope
toglie i cookie, la richiesta conserva l'handle del chiamante e il trasporto la
rifiuta se la sessione è finita nel frattempo: polling ripreso, paginazione,
result URL e cancellazione remota di una sessione chiusa non partono.

Aprire una sessione quando tutti gli slot sono occupati espelle quella usata
meno di recente fra quelle che nessuna operazione ha prenotato, mai una ancora
prenotata, perché espellere una sessione attiva la dividerebbe fra richieste
concorrenti. Le prenotazioni sono contate dal motore e non dedotte dal numero
di riferimenti al jar, e un client nel pool non è una prenotazione. Espellere
una sessione porta via i suoi cookie e i client costruiti su di essa; il suo
handle viene rifiutato come quello di una sessione chiusa. Se ogni sessione è
prenotata da un'operazione attiva l'apertura fallisce. I jar appartengono al
motore e non ai client del pool, quindi una sessione sopravvive all'espulsione
di un client. Lo store scarta inoltre header Set-Cookie oltre 8 KiB, come
limite di risorsa.

Autorizzare una richiesta di follow-up verso un'altra origin non autorizza il
trasferimento delle credenziali. L'origin proprietaria è quella a cui viene
inviata la prima richiesta e non viene mai ricalcolata da una risposta, perché
l'URL su cui una risposta termina può essere già stato scelto dal servizio
remoto.

Quando un link di paginazione, un URL di polling, un result URL o una
cancellazione remota lascia quell'origin, il motore rimuove autenticazione,
cookie e identità TLS client, e conserva soltanto gli header di una allowlist
che descrivono la rappresentazione richiesta: Accept e varianti, Content-Type,
User-Agent, Cache-Control, Pragma, Range, If-Range e i condizionali If-Match,
If-None-Match, If-Modified-Since, If-Unmodified-Since. L'inoltro usa una
allowlist e non un elenco di nomi vietati perché un solo nome specifico del
fornitore non riconosciuto basterebbe a consegnare un segreto a un'origin
scelta dal servizio remoto.

L'header con la chiave di idempotenza fa eccezione ed è conservato: non è una
credenziale, è generato dal chiamante, e rimuoverlo lascerebbe attivi i retry
sui metodi non idempotenti senza la protezione che la chiave fornisce. Le
chiavi in query o nel body non sono mai state rimosse, essendo parte dell'URL o
del payload.

Poiché il nome di quell'header è configurabile, l'eccezione è concessa soltanto
a un nome che dichiara di trasportare una chiave di idempotenza. Un nome dalla
semantica di credenziale non viene conservato e, sul confine runtime, viene
rifiutato: altrimenti chiamare `Authorization` l'header di idempotenza sarebbe
un modo per allargare la allowlist.

Quando l'header di idempotenza non può attraversare l'origin, perché il suo
nome non dichiara una chiave di idempotenza, anche i retry che la chiave aveva
abilitato per i metodi non idempotenti vengono ritirati per quella richiesta:
restano attivi solo se retry_non_idempotent è impostato esplicitamente nella
policy di retry.

L'autorizzazione è monotona. Una volta che una catena ha lasciato l'origin
proprietaria, la revoca vale per ogni richiesta derivata, compresa una che
torni all'origin di partenza: altrimenti un'origin intermedia potrebbe scegliere
quale richiesta autenticata il motore invia all'origin proprietaria, che è un
confused deputy anche se l'intermediario non vede mai il segreto. Per lo stesso
motivo un polling cross-origin revoca le credenziali anche per il result URL e
per la cancellazione remota che introduce.

Nel runtime:

- l'autenticazione inline è rifiutata;
- gli header sensibili inline sono rifiutati, sia in connection.headers sia
  nei parametri con location header o cookie;
- le credenziali sono ottenute tramite credential_ref;
- artifact e direzione sono verificati;
- correlation id e causation id vengono preservati secondo il contratto.

Per il rifiuto dei segreti inline sul confine runtime e per la redazione dei
risultati pubblici vale invece una classificazione conservativa, non un elenco
esatto. Il nome viene diviso in componenti su qualunque carattere non
alfanumerico, non solo trattino e underscore, perché la grammatica HTTP ammette
anche punto, punto esclamativo e altri: X.Token va classificato come X-Token.

Un nome è sensibile quando vale una di queste condizioni. Primo, una componente
è una parola di credenziale: authorization, bearer, cookie, credential,
credentials, jwt, key, passcode, passphrase, passwd, password, secret, session,
signature, token. Secondo, una componente termina con uno dei suffissi ammessi —
accesskey, apikey, authorization, credential, jwt, passcode, passphrase, passwd,
password, privatekey, secret, signature, token — che copre le grafie incollate
come X-SessionToken. Terzo, componenti adiacenti compongono un marcatore come
X-Api-Key. Quarto, una componente inizia o finisce con otp, totp o hotp: queste
sequenze non aprono né chiudono alcuna parola inglese ordinaria, quindi la stessa
regola copre X-OTP, X-OTPCode e X-VendorOTP senza enumerare le parole che
possono affiancarle.

Prima del confronto viene rimossa una eventuale s finale, così X-Api-Keys e
X-Access-Tokens sono letti esattamente come le loro grafie singolari, mentre
X-Monkeys resta benigno perché monkey è fra le eccezioni.

Una componente che termina in key è sensibile per default, con un elenco
esplicito di eccezioni benigne. È enumerato il lato benigno e non quello
credenziale perché le due omissioni non sono simmetriche: una voce mancante fra
le eccezioni sovra-classifica un header, mentre una voce mancante fra le grafie
credenziali sarebbe una fuga, e quelle grafie sono infinite. Le eccezioni sono di due tipi. Le
parole ordinarie come monkey sono esentate per componente. Gli identificatori di
fornitore sono invece esentati per nome completo, così l'esenzione copre
esattamente l'header verificato e non ogni header che ne contenga la componente:
è il caso di x-ms-documentdb-partitionkey, che Cosmos DB richiede sulle normali
operazioni sui documenti. Una passkey resta classificata come credenziale.

Il confronto per componente e per suffisso evita di classificare come
credenziali header ordinari: X-Author e X-Secretariat non lo sono, mentre
X-Author-Token e X-Secret sì. Gli stessi nomi non attraversano mai i risultati
pubblici, nemmeno con la cattura wildcard.

La redazione pubblica elimina segreti, body remoti non autorizzati, path,
indirizzi e dettagli di trasporto.

## Errori ed effetti remoti

I fallimenti vengono convertiti nel contratto plenora-error-v1. Oltre alla
categoria e alla fase, ogni errore dichiara remote_effect e una strategia di
retry.

Il motore non assume che uno status HTTP renda automaticamente sicura una
ripetizione. Quando una richiesta potrebbe essere stata inviata ma l'esito non
è noto, l'effetto remoto è unknown e il retry può richiedere quarantena o
recovery.

Anche il tipo Rust EngineError non contiene testo di terzi, nemmeno
nascosto. Le varianti che descrivono un fallimento a parole contengono un
ErrorDetail, che si costruisce soltanto da una stringa statica (`&'static str`)
scritta nel motore: un messaggio del servizio remoto, un estratto del body,
un indirizzo, un dominio, il testo di un `io::Error`, il messaggio di un parser
o un checksum dei dati non possono entrarvi, perché il tipo non li accetta. Per
un body che non si riesce a leggere (JSON, NDJSON, CSV) il dettaglio conserva
soltanto riga e colonna in cui il parser si è fermato; per un errore di I/O
soltanto il tipo di errore. ErrorDetail espone text() e position(), che quindi
non possono restituire dati. CircuitOpen e ChecksumMismatch non hanno più
campi.

Display di EngineError è il messaggio pubblico statico della variante, lo
stesso di payload().message. Le varianti con numeri scelti dal motore o dal
protocollo (limite in byte, status HTTP, tentativi di polling, versione del
contratto) li mantengono pubblici.

Il messaggio che un servizio restituisce al percorso error_path non viene
acquisito: la risposta fallisce con APPLICATION_ERROR e il dettaglio dice
soltanto che error_path riportava un errore. Lo stesso vale per lo status
remoto di un job asincrono fallito. Nessun contratto consegna oggi quel testo al
chiamante, né prima né dopo questa modifica (il message pubblico era già
statico); se servisse, andrebbe consegnato come dato remoto a parte, in un
campo del risultato dichiarato come tale, non dentro l'errore.

Deviazione dichiarata dal congelamento della superficie Rust v1: i nomi
esportati non cambiano, ma i campi testuali delle varianti passano da String a
ErrorDetail, CircuitOpen e ChecksumMismatch perdono i campi e il testo di
Display cambia. Un'implementazione di RuntimeResources che costruiva
`EngineError::InvalidInput(String)` scrive
`EngineError::InvalidInput("testo statico".into())`; una stringa costruita a
runtime non compila più, per scelta. Il wire contract (plenora-error-v1, schemi
v1) non cambia.

## Confini intenzionali

Non fanno parte dell'architettura attuale:

- adapter o preset incorporati per singoli provider;
- amministrazione diretta di message broker;
- persistenza applicativa dei job remoti;
- API Python asincrona;
- scambio Arrow;
- supporto dichiarato fuori dalla matrice Linux x86_64.

Le estensioni future devono mantenere il core provider-neutral e il confine
black-box.

## Riferimenti

- [Panoramica](../README.md)
- [Contratti pubblici](../contracts/README.md)
- [Sviluppo e release](development.md)
- [Roadmap](roadmap.md)
