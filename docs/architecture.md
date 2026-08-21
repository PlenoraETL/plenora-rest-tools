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
5. Rust, Python e runtime espongono le stesse cinque operazioni normative.
6. Un cambiamento breaking crea una nuova versione del contratto.

## Componenti

~~~text
                                  +-----------------------+
Host Rust ----------------------> |                       |
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

Per i download:

- i byte vengono scritti in un file di staging;
- overwrite deve essere esplicito;
- il resume usa Range e validatori coerenti;
- dimensione e SHA-256 possono essere verificati;
- un output incompleto non viene promosso a risultato finale.

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

Nel runtime:

- l'autenticazione inline è rifiutata;
- Authorization, Proxy-Authorization, Cookie e API key inline sono rifiutati;
- le credenziali sono ottenute tramite credential_ref;
- artifact e direzione sono verificati;
- correlation id e causation id vengono preservati secondo il contratto.

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
