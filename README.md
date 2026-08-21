# Plenora REST Tools

Plenora REST Tools è una libreria REST autoconsistente e indipendente dai
provider. Il motore è scritto in Rust ed è disponibile attraverso tre superfici
pubbliche:

| Superficie | Artefatto | Uso |
| --- | --- | --- |
| Rust | plenora-rest-core | integrazione nativa e binding del runtime |
| Python | plenora-rest | SDK sincrono basato su una wheel ABI3 |
| Runtime Plenora | plenora.rest-tools | invocazione black-box tramite contratti versionati |

La versione corrente dei manifest è 0.2.2. Le operazioni e i payload pubblici
usano contratti v1; il catalogo delle capability usa Plenora Capabilities v2.

## Obiettivo

L'host descrive una richiesta tramite dati versionati. La libreria possiede
HTTP, TLS, DNS, autenticazione, retry, paginazione, polling, trasformazioni,
trasferimenti e stato di connessione.

~~~text
Host -> contratto v1 -> Plenora REST Tools -> servizio HTTP
Host <- risultato o errore tipizzato <- Plenora REST Tools
~~~

Il core non contiene adapter, preset o logica dedicata a OpenMeteo, Sister o
altri servizi. Le differenze tra provider sono configurazione del contratto.

## Operazioni pubbliche

La capability espone cinque operazioni stabili:

| Operazione | Comportamento |
| --- | --- |
| rest.test | esegue una richiesta e restituisce valore, metadati e metriche |
| rest.generate | genera record da una o più risposte, anche paginate |
| rest.enrich | arricchisce record conservandone ordine e contenuto |
| rest.download | trasferisce una risposta HTTP verso un artifact |
| rest.upload | invia un artifact come body raw o parte multipart |

Le operazioni accettano deadline assolute, cancellazione cooperativa e chiavi di
idempotenza. Il motore persistente conserva pool di connessioni, rate limiter,
cache, cookie jar e circuit breaker.

## Copertura attuale

Il motore implementa oggi:

- GET, HEAD, POST, PUT, PATCH, DELETE e OPTIONS, oltre a metodi custom
  autorizzati dall'host;
- parametri path, query, header, cookie e body con serializzazioni comuni;
- body JSON, form URL-encoded, multipart e raw;
- autenticazione Basic, Bearer, API key e flussi OAuth supportati dal contratto;
- risposte JSON, CSV, XML, NDJSON, testo UTF-8 e binario;
- retry con backoff e Retry-After, rate limit, cache HTTP, cookie e circuit
  breaker;
- paginazione per offset, pagina, cursore, link nel body e header Link;
- submit e polling di job REST asincroni, resume, recovery e cancellazione
  remota best-effort;
- batch, enrichment concorrente ordinato e trasformazioni di risposta;
- upload e download streaming con limiti, resume controllato e SHA-256;
- proxy esplicito e configurazione TLS autorizzata.

Una coda interna a un servizio è coperta quando il servizio espone submit,
stato e risultato tramite REST. La libreria non amministra direttamente Celery,
Redis, RabbitMQ, SQS o altri broker.

## SDK Python

Lo SDK supportato è sincrono e richiede CPython da 3.10 a 3.14. La stessa wheel
ABI3 py310 viene verificata su tutte queste versioni.

Installazione dalla working copy:

~~~powershell
python -m pip install .
~~~

Esempio provider-neutral:

~~~python
from plenora_rest import Engine

connection = {
    "url": "https://api.example.com/weather",
    "method": "GET",
    "parameters": [
        {
            "name": "latitude",
            "mode": "mapped",
            "source": "lat",
            "location": "query",
            "required": True,
        },
        {
            "name": "longitude",
            "mode": "mapped",
            "source": "lon",
            "location": "query",
            "required": True,
        },
    ],
    "response": {
        "output_mapping": [
            {"path": "current.temperature", "column": "temperature"}
        ]
    },
    "retry": {"max_attempts": 3},
    "requests_per_second": 10,
}

with Engine() as engine:
    result = engine.enrich(
        connection,
        [{"lat": 41.9028, "lon": 12.4964, "city": "Roma"}],
        concurrency=4,
        deadline="2099-01-01T00:00:00Z",
    )

if result["status"] == "failed":
    raise RuntimeError(result["errors"])
~~~

Lo SDK espone Engine, CancellationToken, PlenoraError, capability discovery e
tipi pubblici. Le operazioni normative sono test, generate, enrich, download e
upload; l'accesso JSON universale resta interno al binding nativo.

## Sicurezza predefinita

La configurazione iniziale è fail-closed:

- reti private e loopback sono bloccati;
- redirect, proxy e file transfer richiedono autorizzazione esplicita;
- i redirect autorizzati devono restare sulla stessa origin;
- TLS viene verificato;
- request, response e file hanno limiti distinti;
- header sensibili, credenziali, path locali e dettagli di trasporto non
  attraversano i risultati pubblici;
- il runtime accetta credential_ref e riferimenti artifact opachi invece di
  segreti o path inline.

I path locali sono disponibili soltanto per chiamanti Rust o Python in-process
che abilitano allow_file_transfers e configurano una file_root. Il runtime
risolve artifact_source e artifact_sink tramite risorse autorizzate dall'host.

## Supporto iniziale

| Area | Supporto dichiarato |
| --- | --- |
| Sistema | GNU/Linux manylinux2014, glibc 2.17 o successiva |
| Architettura | x86_64 |
| Rust | MSRV 1.85.1, target x86_64-unknown-linux-gnu |
| Python | CPython 3.10-3.14, ABI3 py310, API sincrona |
| Distribuzione | crate e wheel allegati alla GitHub Release |

Windows, macOS, ARM, musl, PyPy, CPython 3.15 e uno SDK Python asincrono non
fanno parte della matrice supportata attuale.

La base funzionale e i gate di qualità sono consolidati. Il go-live richiede
ancora la campagna operativa in staging, una release candidata e
l'integrazione canary in Plenora. Lo stato e i criteri sono mantenuti nella
[roadmap](docs/roadmap.md).

## Documentazione

- [Architettura e confini](docs/architecture.md)
- [Contratti pubblici](contracts/README.md)
- [Sviluppo, verifica e release](docs/development.md)
- [Stato e strada verso la produzione](docs/roadmap.md)

## Verifica

Il gate self-contained usato anche dalla CI è:

~~~powershell
pwsh ./scripts/verify.ps1
~~~

Il comando valida contratti e compatibilità v1, MSRV, formato, Clippy, test
Rust, wheel installata e matrice ABI3 CPython 3.10-3.14. Le release aggiungono
doppia build riproducibile, checksum, SBOM e attestazioni.

## Licenza

MIT OR Apache-2.0.
