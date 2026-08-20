# Plenora REST

Libreria REST autoconsistente e service-agnostic scritta in Rust, disponibile
come crate, SDK Python e binding per il runtime Plenora.

L'host passa configurazione e dati attraverso contratti versionati. HTTP, TLS,
DNS, connessioni, autenticazione, retry, limiti, paginazione, polling,
trasferimenti e trasformazioni rimangono interni alla libreria.

~~~text
Host -> contratto versionato -> Plenora REST -> servizio HTTP
Host <- risultato o errore tipizzato <- Plenora REST
~~~

Il core non contiene adapter, preset o codice dedicato a OpenMeteo, Sister o
altri provider. Le differenze tra servizi sono dati del contratto.

## Operazioni pubbliche

La capability plenora.rest-tools espone cinque operazioni v1:

| Operazione | Scopo | Input | Output |
| --- | --- | --- | --- |
| rest.test | prova una connessione o richiesta | plenora-rest-execution-request-v1 | plenora-rest-execution-result-v1 |
| rest.generate | genera record da una o pi� risposte | plenora-rest-execution-request-v1 | plenora-rest-execution-result-v1 |
| rest.enrich | arricchisce record preservandone l'ordine | plenora-rest-execution-request-v1 | plenora-rest-execution-result-v1 |
| rest.download | trasferisce una risposta verso un artifact | plenora-rest-file-transfer-input-v1 | plenora-rest-file-transfer-result-v1 |
| rest.upload | trasferisce un artifact nella richiesta | plenora-rest-file-transfer-input-v1 | plenora-rest-file-transfer-result-v1 |

Tutte supportano cancellazione, deadline assoluta RFC 3339 e chiavi di
idempotenza. Il documento
Capability Discovery v2 � disponibile tramite capabilities.

## Copertura REST

Il motore include:

- GET, HEAD, POST, PUT, PATCH, DELETE e OPTIONS, oltre a metodi custom
  autorizzati da allowlist;
- parametri path, query, header, cookie e body, incluse le serializzazioni
  form, space-delimited, pipe-delimited e deep-object;
- body JSON, form URL-encoded, multipart e raw;
- autenticazione bearer, API key, basic, OAuth2 client credentials, OAuth2
  password e token ArcGIS;
- JSON, CSV, XML, NDJSON, testo UTF-8 e binario;
- retry con backoff e Retry-After, rate limit, pool, cache HTTP, cookie jar e
  circuit breaker;
- paginazione offset, page, cursor, link nel body e header Link;
- submit, polling, resume, cancellazione remota best-effort e recupero dei job
  asincroni;
- batch, enrichment concorrente ordinato e trasformazioni JSON;
- upload e download streaming con limiti, resume controllato e SHA-256;
- TLS personalizzato e proxy esplicito.

Non esiste una libreria che possa promettere ogni comportamento proprietario
di ogni API REST. Questa copre i meccanismi generali; un protocollo non HTTP o
un flusso proprietario non descrivibile dal contratto richiede una capability
separata, non un adapter nel core.

### Job asincroni e servizi basati su coda

La libreria copre un servizio che mette il lavoro in coda quando quel servizio
espone il ciclo di vita tramite REST: submit iniziale, identificativo o
`Location`, polling dello stato, recupero del risultato e, se disponibile,
cancellazione remota. Un'esecuzione interrotta restituisce un recovery handle
limitato al job id pubblico; lo stesso job puo poi essere ripreso senza inviare
una seconda submit.

~~~json
{
  "url": "https://api.example.com/exports",
  "method": "POST",
  "polling": {
    "url_template": "{base}/jobs/{job_id}",
    "status_path": "status",
    "result_url_path": "result_url",
    "resume": {"job_id": "job-42"},
    "cancel": {
      "url_template": "{base}/jobs/{job_id}",
      "method": "DELETE",
      "on_cancellation": true,
      "on_deadline": true
    }
  }
}
~~~

Celery, Redis, RabbitMQ, SQS e gli altri broker non sono dipendenze del core e
non vengono amministrati dalla libreria. Restano un dettaglio interno del
servizio remoto o del runtime che orchestra la chiamata. Questo mantiene la
libreria autoconsistente e provider-neutral.

## SDK Python

Richiede Python 3.10 o successivo. La wheel usa ABI3 da Python 3.10 e non
dipende da requests o da altri client HTTP Python.

~~~powershell
python -m pip install maturin
python -m pip install .
~~~

Uso sincrono:

~~~python
from plenora_rest import CancellationToken, Engine, PlenoraError

with Engine() as engine:
    result = engine.generate(
        {
            "url": "https://api.example.com/items",
            "method": "GET",
            "response": {"records_path": "data.items"},
            "pagination": {
                "type": "page",
                "page_param": "page",
                "page_size_param": "limit",
                "page_size": 100,
                "max_rows": 10_000,
            },
        },
        deadline="2026-08-20T22:00:00Z",
        idempotency_key="pipeline-run-42",
    )
~~~

La chiave viene collocata in header, query o body secondo
`connection.idempotency`; il default e l'header `Idempotency-Key`. Retry e
richieste figlie usano chiavi stabili. Il motore rifiuta localmente il riuso
della stessa chiave con un input diverso, mentre la deduplicazione durevole e
responsabilita del servizio remoto o del runtime.

Le sole operazioni normative dell'SDK sono test, generate, enrich, download e
upload. La serializzazione universale resta privata alla facciata Python, cos�
il chiamante non pu� aggirare il contratto pubblico.

La cancellazione � cooperativa e thread-safe:

~~~python
token = CancellationToken()
token.cancel()
result = engine.test(connection, cancellation=token)
~~~

Engine � un oggetto persistente: conserva pool, cache, rate limiter, cookie jar
e circuit breaker. close � idempotente; il context manager lo richiama
automaticamente.

## Trasferimenti file

Sul surface Python i path locali sono disabilitati per default e devono essere
autorizzati esplicitamente:

~~~python
engine = Engine(
    {
        "allow_file_transfers": True,
        "file_root": "/var/lib/plenora/transfers",
        "max_file_transfer_bytes": 2_147_483_648,
    }
)

result = engine.download(
    {"url": "https://api.example.com/export", "method": "GET"},
    "/var/lib/plenora/transfers/export.parquet",
    overwrite=True,
)
~~~

Il risultato pubblico non espone il path risolto. Contiene direction,
artifact_reference, bytes_transferred e checksum SHA-256. I download usano uno
staging file e non pubblicano output parziali.

Nel binding runtime i path sono sempre vietati nel payload. L'host fornisce
artifact_source per upload e artifact_sink per download; il resolver autorizza
il riferimento opaco e restituisce il path soltanto all'interno del processo.

## Sicurezza della black box

Le impostazioni conservative sono:

- reti private e loopback bloccati;
- redirect solo same-origin;
- TLS verificato;
- proxy, file transfer, cookie persistenti e metodi custom disabilitati;
- limiti separati per request, response e file;
- header sensibili mai restituiti, anche con selezione wildcard;
- URL finali pubblici ridotti alla sola origin;
- dettagli di trasporto, body di errore, credenziali e path non inseriti negli
  errori pubblici.

Nel runtime autenticazione inline, header sensibili, chiavi private e
credenziali proxy sono rifiutati. credential_ref viene risolto tramite una
risorsa autorizzata dell'host.

## Errori

Ogni errore rispetta plenora-error-v1 e include:

- category e phase;
- remote_effect;
- retry con indicazione never, safe, quarantine o requires_recovery;
- code stabile in maiuscolo;
- message pubblico redatto;
- details strutturati e non sensibili.

Il retry � conservativo: uno status HTTP da solo non viene interpretato come
prova che sia sicuro ripetere una richiesta. Timeout, cancellazione e perdita
di trasporto dopo un possibile invio producono remote_effect unknown e
quarantine.

## Binding runtime

RuntimeBinding accetta esclusivamente envelope plenora-runtime-binding-v1 o
plenora-runtime-vector-v1. Valida:

- UUID canonici per message id e correlation id;
- capability, operazione e versioni;
- input contract coerente con il selettore;
- JSON object e content type;
- direzione degli artifact;
- deadline e token di cancellazione.

La risposta genera un nuovo message id, conserva correlation id e imposta il
message id in ingresso come causation id. I fallimenti usano
application/vnd.plenora.error+json e plenora-error-v1.

## Contratti e verifica

I cinque schemi component-owned sono in contracts/schemas. La mappatura degli
export Rust � contracts/bindings/rust-v1.json. Il manifest di adozione v4 �
adoption-manifest.json e pinna plenora-contracts al commit:

~~~text
83ed391212b28100ffa6097a2963f746b6209b37
~~~

Gate locale completo:

~~~powershell
./scripts/verify.ps1
~~~

Il gate esegue rustfmt, Clippy con warning negati, test Rust, build release
della wheel, installazione in un ambiente pulito e test black-box dell'SDK.

## Release riproducibile

La release 0.2.x distribuisce il crate Rust e una wheel ABI3
manylinux2014 x86_64. La toolchain Maturin, le immagini Docker e il
SOURCE_DATE_EPOCH sono fissati in release-metadata.json. Il comando locale
costruisce ogni artefatto due volte in ambienti isolati e fallisce se i byte o
i nomi non coincidono:

~~~powershell
./scripts/release.ps1
~~~

Il bundle in dist contiene crate, wheel, SBOM SPDX 2.3 e SHA256SUMS. Il digest
del crate e della wheel deve coincidere anche con adoption-manifest.json.

Il workflow di release parte soltanto da un tag v<versione> coerente con tutti
i manifest. Prima di creare la GitHub Release riesegue il gate completo,
verifica la riproducibilita e genera attestazioni Sigstore di provenance e
SBOM. Il tag e la pubblicazione restano operazioni esplicite; il workflow non
pubblica automaticamente su crates.io o PyPI.

## Struttura

~~~text
crates/rest-engine-core     core Rust e binding runtime
crates/rest-engine-python   estensione PyO3
python/plenora_rest         SDK Python pubblico e typing marker
python/tests                test della wheel installata
contracts/schemas           contratti component-owned
contracts/bindings          mapping degli export Rust
examples                    esempi d'uso
~~~

Licenza duale MIT OR Apache-2.0.
