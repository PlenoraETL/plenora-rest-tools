# Rust REST Engine

Motore REST autoconsistente scritto in Rust, distribuibile come wheel Python.
L'applicazione host lo usa come una black box: invia un documento JSON
versionato a un oggetto Engine persistente e riceve un risultato JSON stabile.

~~~text
Host -> Engine.execute(request) -> result
              |
              +-- HTTP, TLS, DNS, pool, auth, retry, redirect,
                  limiti, paginazione e mapping restano interni
~~~

Il codice Python non importa reqwest, non gestisce sessioni e non passa
callback al core. La wheel contiene l'estensione nativa e non dipende da
requests o da altri client HTTP Python.

## Funzionalità disponibili

Sono disponibili:

- operazioni test, generate ed enrich;
- metodi GET, HEAD, POST, PUT, PATCH, DELETE e OPTIONS, più metodi custom
  autorizzati da una allowlist dell'Engine;
- body JSON, form URL-encoded, multipart in-memory e raw;
- parametri indirizzabili separatamente a path, query, header, cookie e body,
  anche nella stessa request;
- serializzazione query form, space-delimited, pipe-delimited e deepObject;
- autenticazione bearer, API key, basic, OAuth2 client-credentials,
  OAuth2 password e token ArcGIS;
- pool persistente, timeout, retry/backoff e Retry-After sia in secondi sia
  nel formato HTTP-date;
- paginazione offset, page, cursor, link estratto dal body e header Link;
- polling asincrono tramite body, template o header, con recupero separato
  del risultato, backoff e timeout totale;
- limiti di frequenza globali e per connessione, più limite di concorrenza;
- enrichment concorrente opt-in con ordinamento deterministico dei risultati;
- modalità batch con chunk e ricomposizione dei risultati;
- cookie jar persistenti, opt-in e isolati per identificatore;
- cache HTTP bounded con ETag/Last-Modified e revalidation 304;
- circuit breaker bounded per origin e gruppo, con stato half-open;
- risposte JSON, CSV, XML, NDJSON, testo UTF-8 e binario base64;
- body vuoti, inclusi 204 e 205, rappresentati come null;
- decompressione automatica e disattivabile di gzip, Brotli, deflate e Zstandard;
- metadata delle risposte riuscite opzionali, con allowlist degli header;
- mapping, iterazioni annidate e trasformazioni tramite percorsi JSON,
  inclusi indici negativi e filtri;
- TLS personalizzato, CA PEM, identità client PEM e proxy esplicito;
- limiti hard di richiesta e risposta (32 MiB di default);
- TLS verificato, redirect disabilitati di default e solo same-origin;
- protezione SSRF con validazione DNS e connessione fissata all'IP validato.

Il core non contiene adapter applicativi, preset nominativi o client dedicati
a singoli servizi. Ogni integrazione è descritta tramite lo stesso contratto
universale.

## Installazione SDK Python

Servono Rust 1.85+ e Python 3.9+ per una build di sviluppo:

~~~text
python -m pip install maturin
python -m pip install .
~~~

Per la distribuzione si costruisce una wheel per ogni piattaforma/architettura:

~~~text
maturin build --release
~~~

L'estensione usa ABI3 a partire da Python 3.9.

## Uso SDK Python

~~~python
from rest_engine import Engine

engine = Engine()  # istanza longeva: conserva runtime e pool

connection = {
    "url": "https://api.example.com/weather",
    "method": "GET",
    "auth": {"type": "none"},
    "static_parameters": {"current_weather": "true"},
    "parameters": [
        {
            "name": "latitude",
            "mode": "mapped",
            "source": "lat",
            "required": True,
        },
        {
            "name": "longitude",
            "mode": "mapped",
            "source": "lon",
            "required": True,
        },
    ],
    "response": {
        "output_mapping": [
            {
                "path": "current.temperature",
                "column": "temperature",
            }
        ]
    },
    "retry": {"max_attempts": 3},
    "requests_per_second": 10,
}

result = engine.enrich(
    connection,
    [
        {"lat": 41.9028, "lon": 12.4964},
        {"lat": 45.4642, "lon": 9.1900},
    ],
)

if result["status"] not in {"success", "partial"}:
    raise RuntimeError(result["errors"])

records = result["output"]["records"]
~~~

Per servizi interni che risolvono su IP privati, la scelta deve essere
esplicita:

~~~python
engine = Engine({"allow_private_networks": True})
~~~

I limiti sono globali per l'istanza Engine:

~~~python
engine = Engine(
    {
        "max_concurrent_requests": 16,
        "requests_per_second": 20,
        "max_request_bytes": 33_554_432,
        "max_response_bytes": 33_554_432,
        "automatic_decompression": True,
        "allow_cookie_store": True,
        "max_cache_entries": 1_024,
        "max_cache_bytes": 67_108_864,
        "max_circuit_origins": 256,
    }
)
~~~

I metodi HTTP custom sono negati per default e devono essere autorizzati
quando si crea l'Engine:

~~~python
engine = Engine(
    {
        "allowed_custom_methods": ["PURGE", "REPORT"],
    }
)

result = engine.test(
    {
        "url": "https://api.example.com/cache/item",
        "method": "PURGE",
    }
)
~~~

## Parametri e formati

Una request può combinare query, header, cookie e body senza codice specifico
per il servizio. La configurazione di serializzazione è applicata soltanto
quando viene dichiarata, quindi i contratti esistenti mantengono il
comportamento precedente:

~~~python
connection = {
    "url": "https://api.example.com/search",
    "method": "POST",
    "parameters": [
        {
            "name": "filter",
            "mode": "fixed",
            "value": {"active": True, "role": "admin"},
            "location": "query",
            "query_serialization": {"style": "deep_object"},
        },
        {
            "name": "X-Tenant",
            "mode": "mapped",
            "source": "tenant",
            "location": "header",
        },
        {
            "name": "payload",
            "mode": "mapped",
            "source": "document",
            "location": "body",
        },
    ],
}
~~~

I formati non JSON sono espliciti:

~~~python
connection["response"] = {"format": "ndjson"}
~~~

Con format impostato a binary, output.value contiene data_base64 e size; il
limite max_response_bytes continua a essere applicato ai byte ricevuti e
decompressi prima della codifica base64.

## OAuth2, multipart e polling

Il token OAuth2 viene ottenuto, aggiornato e conservato soltanto nel core:

~~~python
connection["auth"] = {
    "type": "oauth2_client_credentials",
    "token_url": "https://identity.example.com/oauth/token",
    "client_id": "rest-client",
    "client_secret": "secret",
    "scope": "people.read",
    "client_auth": "basic",
}
~~~

Per multipart i campi ordinari restano valori JSON. Un file è un oggetto con
contenuto base64; non vengono esposti path locali al motore:

~~~python
connection["method"] = "POST"
connection["request"] = {"body_type": "multipart"}

result = engine.test(
    connection,
    params={
        "description": "import",
        "attachment": {
            "filename": "people.csv",
            "content_type": "text/csv",
            "data_base64": encoded_csv,
        },
    },
)
~~~

Il polling parte dalla risposta iniziale e usa per default l'header Location:

~~~python
connection["polling"] = {
    "status_path": "status",
    "result_path": "result",
    "pending_values": ["queued", "running"],
    "success_values": ["completed"],
    "failure_values": ["failed"],
    "interval_ms": 1_000,
    "max_attempts": 60,
}
~~~

In alternativa sono disponibili url_path oppure url_template con placeholder
{id} e {job_id}. Gli endpoint di polling cross-origin sono bloccati salvo
abilitazione esplicita.

## Contratto black-box

L'entry point universale è:

~~~python
result = engine.execute(
    {
        "schema_version": 1,
        "operation": "generate",
        "connection": {
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
        "input": {"params": {}, "records": []},
        "options": {"continue_on_error": True},
    }
)
~~~

Il risultato contiene sempre:

- schema_version;
- status: success, partial o failed;
- output: none, json o records;
- metrics: richieste, retry, OAuth, polling, cache hit/revalidation, attesa
  rate limit, record ed elapsed time;
- responses: metadata HTTP richiesti esplicitamente;
- errors: codici stabili, indice del record e stato HTTP quando presenti.

I metadata delle risposte sono disabilitati per default. Possono essere
richiesti sul contratto universale senza esporre automaticamente tutti gli
header:

~~~python
request["options"] = {
    "capture_response_metadata": True,
    "response_headers": ["etag", "last-modified", "x-request-id"],
}
~~~

Ogni elemento di responses contiene status, final_url, attempts e gli header
selezionati. response_headers impostato a ["*"] include tutti gli header ed è
quindi una scelta esplicita, utile solo quando il chiamante deve realmente
riceverli.

La paginazione tramite header Link usa una relazione configurabile e blocca
per default destinazioni cross-origin:

~~~python
connection["pagination"] = {
    "type": "header_link",
    "relation": "next",
    "max_rows": 10_000,
    "max_pages": 100,
}
~~~

## Sessioni, cache e resilienza

Le sessioni sono disabilitate per default sia nell'Engine sia nella
connessione. jar_id separa i cookie di tenant o integrazioni differenti:

~~~python
engine = Engine({"allow_cookie_store": True})
connection["cookies"] = {
    "enabled": True,
    "jar_id": "tenant-42",
}
~~~

La cache è disponibile soltanto per GET e HEAD. Con fresh_for_ms uguale a
zero il motore revalida sempre tramite ETag o Last-Modified; Cache-Control
no-store, no-cache, max-age e Vary: * vengono rispettati. Per richieste
autenticate serve un consenso aggiuntivo:

~~~python
connection["cache"] = {
    "enabled": True,
    "fresh_for_ms": 30_000,
    "allow_authenticated": False,
}
~~~

Il circuit breaker conta gli esiti finali dopo i retry ed evita nuove
connessioni finché non entra in half-open:

~~~python
connection["circuit_breaker"] = {
    "enabled": True,
    "group": "catalog",
    "failure_threshold": 5,
    "recovery_timeout_ms": 30_000,
    "failure_statuses": [429, 500, 502, 503, 504],
}
~~~

L'enrichment concorrente mantiene l'ordine degli input ed è comunque
vincolato dai limiti globali di concorrenza e frequenza dell'Engine:

~~~python
result = engine.enrich(
    connection,
    records,
    concurrency=8,
)
~~~

Errori di contratto che impediscono di creare un risultato (per esempio JSON
malformato) sollevano RestEngineError. Gli errori operativi REST sono invece
nel risultato, così l'applicazione host può gestirli senza conoscere eccezioni
Rust/PyO3.

## Limiti attuali

Il motore copre le forme REST più comuni, non l'intero spazio HTTP. Non sono
ancora inclusi:

- stili OpenAPI label/matrix per path, allowReserved e serializzazione
  avanzata dei campi form URL-encoded;
- streaming pubblico di upload/download;
- YAML, decodifica protobuf e parser personalizzati;
- autenticazioni firmate come Digest, NTLM, Kerberos o AWS SigV4;
- SSE, WebSocket, webhook callback e protocolli non request/response;
- coordinamento distribuito di rate limit e token cache;
- importazione automatica OpenAPI;
- variabili dinamiche basate su data e ora nei parametri;
- deduplicazione in-flight e scheduler distribuito dell'enrichment;
- espansione automatica di wildcard annidate e inferenza dello schema.

## Workspace

~~~text
crates/rest-engine-core    motore e contratto indipendenti da Python
crates/rest-engine-python  binding PyO3 minimale
python/rest_engine         facciata e tipi dell'SDK
~~~

Verifiche:

~~~text
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
python -m py_compile python/rest_engine/__init__.py python/rest_engine/_engine.py
~~~
