# Contratti pubblici di Plenora REST Tools

Questa directory contiene il wire contract normativo posseduto dal componente
plenora-rest-tools. Il catalogo comune di Plenora assegna identità e versione
alle operazioni; questo repository possiede i payload REST referenziati dal
catalogo e il mapping verso gli entrypoint Rust.

Il comportamento interno del client HTTP non fa parte del contratto. Provider,
preset e adapter specifici non devono entrare in questa directory.

## Schemi component-owned

Tutti gli schemi usano JSON Schema Draft 2020-12 e hanno versione v1:

| File | Responsabilità |
| --- | --- |
| schemas/plenora-rest-execution-request-v1.schema.json | richiesta per test, generate ed enrich |
| schemas/plenora-rest-execution-result-v1.schema.json | risultato comune, metriche ed errori |
| schemas/plenora-rest-file-transfer-input-v1.schema.json | input per upload e download |
| schemas/plenora-rest-file-transfer-result-v1.schema.json | risultato di un trasferimento |
| schemas/plenora-rest-async-job-recovery-v1.schema.json | handle limitato per riprendere un job REST |
| schemas/plenora-rest-capability-attributes-v1.schema.json | attributi pubblici della capability |

Gli identificatori canonici sono gli URI contenuti nel campo $id di ogni
schema. I file, gli URI e i digest canonici sono congelati in
compatibility-v1.json.

## Operazioni

Le operazioni pubbliche sono stabili e provider-neutral:

| Operazione | Input | Output |
| --- | --- | --- |
| rest.test | execution-request-v1 | execution-result-v1 |
| rest.generate | execution-request-v1 | execution-result-v1 |
| rest.enrich | execution-request-v1 | execution-result-v1 |
| rest.download | file-transfer-input-v1 | file-transfer-result-v1 |
| rest.upload | file-transfer-input-v1 | file-transfer-result-v1 |

Il file bindings/rust-v1.json collega queste identità agli entrypoint
plenora_rest_core::Engine. Lo stesso file dichiara capability discovery,
lifecycle del motore e trasporto JSON del runtime.

## Contratti comuni adottati

adoption-manifest.json registra la revisione di plenora-contracts e lo stato di
conformità alle superfici comuni. Attualmente il componente adotta:

- plenora-public-surfaces-v1;
- plenora-capabilities-v2;
- plenora-error-v1;
- plenora-public-security-v1;
- plenora-python-sdk-v1;
- plenora-runtime-binding-v1;
- plenora-surface-bindings-v1;
- plenora-composition-v1;
- plenora-cli-v2.

I file del repository dei contratti che il gate usa (i vettori runtime-v1 di
REST, le 21 sonde di rifiuto runtime-probes-v1 di Runtime Binding 1.0
§11-13, gli schemi runtime-vector-v1, runtime-probe-v1, error-v1 e
adoption-manifest-v4, la specifica dei vettori) sono copiati byte per byte in upstream/, con revisione e
SHA-256 in upstream/source.json. La revisione deve coincidere con quella di
adoption-manifest.json; il validatore e il test runtime_vectors rifiutano una
copia diversa dal suo pin o un file senza pin. Il test runtime_vectors esegue
ogni sonda sul vettore di richiesta di rest.upload (quelle scritte per altri
componenti con la stessa mutazione), tranne
storage-get-idempotency-key-unsupported: REST annuncia la chiave di
idempotenza su ogni operazione, quindi la premessa di quella sonda qui non
esiste.

Una regola dei contratti comuni non rispettata si dichiara tra le deviazioni
del manifest, vedi
[limiti e deviazioni](../docs/limiti.md#deviazioni-dai-contratti-adottati).

plenora-arrow-interchange-v1 è dichiarato non applicabile. La libreria scambia
oggetti JSON e artifact opachi, non record batch Arrow.

## CLI

Il binario plenora-rest (crates/rest-cli) è conforme a CLI 2.0
(plenora-cli-v2): discovery con `--help`, `--version --format json` e
`capabilities --format json`, un solo documento JSON su stdout e niente su
stderr in modalità JSON, envelope cli-envelope-v2 con errori plenora-error-v1,
proiezione degli exit code di CLI 2.0 §8, cancellazione con exit 130.

| Comando | Operazione | Contratto dell'envelope |
| --- | --- | --- |
| `--version --format json` | — | plenora-rest-version-result-v1 |
| `capabilities --format json` | — | plenora-capabilities-v2 |
| `test --input REQUEST.json --format json` | rest.test v1 | plenora-rest-execution-result-v1 |
| `generate --input REQUEST.json --format json` | rest.generate v1 | plenora-rest-execution-result-v1 |
| `enrich --input REQUEST.json --format json` | rest.enrich v1 | plenora-rest-execution-result-v1 |
| `download --input REQUEST.json --format json` | rest.download v1 | plenora-rest-file-transfer-result-v1 |
| `upload --input REQUEST.json --format json` | rest.upload v1 | plenora-rest-file-transfer-result-v1 |

Ogni comando operativo accetta anche `--config ENGINE.json`. Un envelope
d'errore porta il contratto di output del comando, oppure plenora-error-v1
quando il comando non è riconosciuto. Il documento delle capability del
binario è quello di plenora_rest_core::capabilities() con in più l'interfaccia
`cli` (artifact plenora-rest) e la superficie `cli` sulle cinque operazioni;
la superficie Rust congelata non cambia.

Dalla v1.1.0 di plenora-contracts (decisione 0012) la CLI di rest-tools è una
superficie facoltativa: il catalogo la seleziona come `conditional` e la elenca
sulle cinque operazioni, e il binding comune bindings/cli-v1.json registra il
comando `plenora-rest`, le tre spellature di discovery e
`<operazione> --input REQUEST.json --format json`. Le spellature sopra sono
quelle del binding e sono quindi normative: cambiarle richiede un nuovo
contratto. La superficie Rust (plenora_rest_core::capabilities) non elenca
`cli`, come il profilo ammette per un artefatto senza il comando.

Nel manifest di adozione l'artefatto CLI si chiama plenora-rest-linux-x86_64,
il nome del file di release, perché plenora-rest identifica già la wheel e un
nome non può descrivere due artefatti diversi.

Il manifest di adozione è la fonte per revisione, versione e digest degli
artefatti. Questi valori non vengono duplicati nella documentazione.

## Regole del confine black-box

Il chiamante può dipendere soltanto da:

- capability discovery;
- operazioni e versioni dichiarate;
- schemi di input e output;
- errori Plenora tipizzati;
- lifecycle close e is_closed;
- risorse runtime autorizzate.

I messaggi runtime sono envelope JSON stretti. UUID, capability, operazione,
versioni, content type, deadline, direzione degli artifact e correlazione
devono essere validi prima dell'esecuzione.

Sul confine runtime sono vietati:

- byte grezzi di file;
- path locali privati;
- autenticazione e header sensibili inline, sia in connection.headers sia nei
  parametri con location header o cookie;
- credenziali del proxy;
- tipi interni del client HTTP.

I trasferimenti usano artifact_source o artifact_sink opachi risolti dall'host.
Le credenziali usano credential_ref. I chiamanti Rust e Python nello stesso
processo possono usare path locali soltanto dopo autorizzazione esplicita
dell'EngineConfig; i risultati non restituiscono mai il path risolto.

Gli errori pubblici conservano category, phase, remote_effect, retry, code,
message e details non sensibili. Un job asincrono interrotto può restituire un
recovery handle contenente il solo identificativo pubblico necessario al
resume.

## Compatibilità

La politica v1 è intenzionalmente rigida:

- gli schemi v1 pubblicati sono immutabili;
- gli export pubblici Rust e Python sono congelati;
- il mapping del binding Rust è congelato;
- una modifica incompatibile richiede nuovi schemi e binding versionati;
- compatibility-v1.json non deve essere aggiornato per aggirare un errore del
  gate.

Deviazione dichiarata per la 0.3.0, decisa dal maintainer: la superficie Rust
congelata cambia una volta sola per EngineError (campi testuali opachi, vedi
docs/architecture.md) e per le sessioni cookie (export CookieSession, entrypoint
Engine::open_cookie_session e Engine::close_cookie_session, CookiePolicy con
session al posto di enabled e jar_id). compatibility-v1.json e
bindings/rust-v1.json sono stati aggiornati insieme a questa decisione, non per
aggirare il gate. Gli schemi JSON v1 non cambiano: `connection.cookies` era già
un oggetto libero.

Un'aggiunta apparentemente compatibile alla superficie pubblica richiede
comunque una decisione esplicita e una revisione del contratto. Implementazioni
interne e configurazioni private possono evolvere senza cambiare il wire
contract.

## Verifica

Il controllo canonico è incluso nel gate completo:

~~~powershell
pwsh ./scripts/verify.ps1
~~~

Con jsonschema disponibile è possibile eseguire il solo controllo contratti:

~~~powershell
python ./scripts/validate_contracts.py
~~~

Il validatore controlla:

1. validità Draft 2020-12;
2. unicità degli $id;
3. risoluzione dei riferimenti posseduti dal componente;
4. digest canonici dei sei schemi;
5. export pubblici Rust e Python;
6. firma del binding Rust;
7. pin dei file in upstream/, vettori runtime e sonde di rifiuto contro i loro
   schemi e manifesto di adozione contro lo schema v4 e le regole di
   ADOPTION.md.

Per una modifica breaking si crea una nuova versione del contratto e si
mantiene la v1 invariata finché esistono consumer supportati.

## Riferimenti

- [Panoramica del progetto](../README.md)
- [Architettura e confini](../docs/architecture.md)
- [Sviluppo e release](../docs/development.md)
- [Roadmap di produzione](../docs/roadmap.md)
