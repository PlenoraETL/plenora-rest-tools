# Fuzz

Crate `cargo-fuzz` separato (workspace e `Cargo.lock` propri) che esercita i
parser dell'input remoto non fidato e dell'input di richiesta, senza rete:
nessun target apre una connessione o risolve un nome.

## Superficie non pubblica

I parser sono privati nel crate `plenora-rest-core`. I target li raggiungono
con la feature `fuzzing`, non di default, che abilita il modulo
`plenora_rest_core::fuzzing` (`#[doc(hidden)]`): involucri sottili sui
percorsi reali del motore. **Non è superficie pubblica**: non è riesportato
da `lib.rs`, non compare in `contracts/compatibility-v1.json`, non è coperto
dalla politica di compatibilità v1 e può cambiare in qualsiasi versione. Il
modulo non aggiunge dipendenze.

## Target e proprietà

Ogni target controlla proprietà, non solo l'assenza di panico. Comuni a
tutti: stesso esito su due esecuzioni dello stesso input; ogni errore è
tipizzato, il suo `Display` è il messaggio statico del payload, `details`
contiene solo le chiavi numeriche note (`comune/esiti.rs`).

| target | input | proprietà |
| --- | --- | --- |
| `response_body` | byte 0: formato (JSON, NDJSON, CSV, XML, testo, binario); byte 1: delimitatore CSV; resto: body | rifiuto `INVALID_RESPONSE`, o `INVALID_INPUT` solo per un delimitatore che non è un byte ASCII; posizione dell'errore dentro il body; JSON e NDJSON riserializzati e riletti uguali; CSV riscritto (RFC 4180) e riletto con le stesse righe; testo e binario byte per byte; XML nella forma documentata (una radice, `@attributi` stringa, `#text` non vuoto, figli ripetuti in array di almeno due, testo senza spazi ai bordi, annidamento limitato) |
| `json_path` | prima riga: percorso; resto: documento JSON | il valore trovato è un nodo del documento (identità di puntatore); `$` iniziale e spazi ai bordi non cambiano il risultato; `$` e percorso vuoto danno la radice |
| `link_header` | prima riga: relazione; resto: header `Link` | target trovato = contenuto esatto di una coppia `<…>`; relazione registrata confrontata senza maiuscole; rifiuto `INVALID_RESPONSE` senza posizione |
| `remote_headers` | byte 0: header; byte 1: lunghezza minima dei `Set-Cookie` (×64 byte); resto: valore | `Retry-After`: secondi saturati (oracolo), un istante successivo non allunga l'attesa; `Content-Range`: accettato solo se `inizio <= fine < totale`, forma canonica riletta uguale; `ETag`: validatore forte = valore rifilato, tra virgolette, mai `W/`; `Cache-Control`: direttive solo se scritte, `max-age` in secondi interi; `Set-Cookie`: il jar limitato memorizza esattamente ciò che memorizza togliendo a mano gli header oltre `MAX_SET_COOKIE_BYTES` |
| `execution_request` | JSON di una `ExecutionRequest` | round-trip stabile (serializzata e riletta dà lo stesso JSON); `Engine::execute` fallisce prima della rete (contatori di richieste, retry, autenticazione e polling a zero, nessuna risposta, nessun codice che esista solo dopo la rete; successo ammesso solo per un enrich senza record); stesso esito su due `Engine` nuovi; gli errori non contengono stringhe distintive della richiesta |
| `runtime_message` | JSON di un `RuntimeMessage` | `RuntimeBinding::invoke_json`: un testo che non è un `RuntimeMessage` è `INVALID_INPUT` con posizione dentro il testo; altrimenti un envelope d'errore (mai un successo senza rete, salvo enrich senza record) con payload pubblico senza dati della richiesta, correlazione e causazione ricopiate; stesso esito su due `Engine` nuovi, a parte l'identificativo casuale della risposta |

### Senza rete

`execution_request` e `runtime_message` eseguono davvero il motore. Prima di
eseguire, `senza_rete` (in `comune/esiti.rs`) sostituisce ogni URL della
richiesta (chiavi `url` e `*_url`, quindi anche `token_url` e l'URL del
proxy) con un loopback letterale e toglie il rate limit per connessione.
L'`EngineConfig` di default blocca le reti private e i proxy: la validazione
dell'indirizzo rifiuta la richiesta prima di qualsiasi connessione, e un
indirizzo letterale non passa dal DNS. Il resto della richiesta (parametri,
template, body, autenticazione, paginazione, polling, trasformazioni,
metadati runtime) resta quello generato. Se un input riuscisse comunque a
raggiungere la rete, le proprietà sui contatori e sui codici d'errore
fallirebbero: il buco sarebbe dell'imbracatura e va chiuso lì.

Le risorse runtime del target risolvono ogni `credential_ref` in nessuna
autenticazione e rifiutano ogni artifact.

## Cosa non è raggiungibile

- Il percorso dopo una risposta HTTP reale (paginazione oltre la prima
  pagina, polling, redirect, download e upload, cache e circuit breaker
  alimentati da risposte): richiede la rete. I parser che quel percorso usa
  (corpo, percorsi JSON, header) hanno i loro target.
- Il parsing di `Content-Type`/charset: il motore non lo fa. Il body si
  legge come byte e il formato lo sceglie `response.format`.
- Il parsing di `Set-Cookie` è del crate `cookie` dentro reqwest: il target
  ne verifica il limite di lunghezza che il motore aggiunge, non la
  grammatica.
- I template di URL e di `raw_body` non hanno un parser: sono sostituzioni
  `{nome}`; sono esercitati dentro `execution_request`.

## Limiti noti, non corretti qui

Comportamenti dei parser che le proprietà dei target accettano, perché
correggerli cambia il contratto e non è una correzione locale:

- JSON: una chiave ripetuta nello stesso oggetto tiene l'ultimo valore
  (serde_json); un intero oltre i 64 bit diventa un f64 e perde precisione
  (serde_json senza `arbitrary_precision`). I float sono letti con
  arrotondamento corretto (`float_roundtrip`).
- XML: il parser rifiuta struttura malformata, DTD, entità non predefinite,
  nomi fuori dalla grammatica e contenuto fuori dalla radice, ma non è un
  validatore lessicale completo di XML 1.0: caratteri di controllo nel testo
  e una dichiarazione XML fuori posto sono accettati. I figli con lo stesso
  nome locale e prefissi diversi finiscono nello stesso array.
- CSV: il delimitatore di default di `ResponseConfig` è la stringa vuota,
  rifiutata come `INVALID_INPUT` solo quando arriva la risposta, non prima
  della richiesta.

## Corpus

I semi iniziali sono in `corpus/<target>/`, pochi file piccoli, uno per
forma significativa dell'input. Li scrive `genera_corpus.py` (deterministico):

```text
python fuzz/genera_corpus.py
```

`.gitattributes` li tiene byte per byte (`-text`), CRLF compresi. Una
campagna scrive i nuovi input in una directory di lavoro, non nel corpus
versionato.

## Esecuzione

libFuzzer non gira su Windows MSVC: le campagne girano su Linux con il
nightly e cargo-fuzz fissati in `.github/workflows/fuzz.yml`.

```text
rustup toolchain install nightly-2026-07-21 --profile minimal
cargo +nightly-2026-07-21 install cargo-fuzz --version 0.13.2 --locked
mkdir -p fuzz/corpus-lavoro/response_body
cargo +nightly-2026-07-21 fuzz run response_body \
  fuzz/corpus-lavoro/response_body fuzz/corpus/response_body -- \
  -max_total_time=600 -timeout=30 -rss_limit_mb=2048
```

La CI controlla formattazione e compilazione dei target su ogni pull
request e push a `main`, ed esegue la campagna ogni settimana, a mano e
sulle pull request che toccano `fuzz/**` (vedi `.github/workflows/fuzz.yml`).
Un crash o un timeout rendono rosso il job; i reperti restano come artefatto
della run. Un reperto riprodotto diventa un test di regressione nel
workspace principale, accanto al parser che corregge.
