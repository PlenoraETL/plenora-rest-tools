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
- plenora-composition-v1.

plenora-arrow-interchange-v1 è dichiarato non applicabile. La libreria scambia
oggetti JSON e artifact opachi, non record batch Arrow.

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
- autenticazione e header sensibili inline;
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
6. firma del binding Rust.

Per una modifica breaking si crea una nuova versione del contratto e si
mantiene la v1 invariata finché esistono consumer supportati.

## Riferimenti

- [Panoramica del progetto](../README.md)
- [Architettura e confini](../docs/architecture.md)
- [Sviluppo e release](../docs/development.md)
- [Roadmap di produzione](../docs/roadmap.md)
