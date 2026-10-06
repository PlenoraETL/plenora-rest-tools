# Sviluppo, verifica e release

Questo documento descrive il flusso operativo attuale del repository. I
comandi canonici sono gli script e i workflow versionati; la documentazione non
deve introdurre procedure parallele.

## Requisiti

Per il gate completo servono:

- Git;
- Docker con supporto alle build multi-stage;
- PowerShell 7 o successivo.

Per lavorare senza Docker sono utili:

- rustup: rust-toolchain.toml fissa il compilatore dei gate (1.98.1, con
  rustfmt e Clippy) e rustup lo installa al primo comando cargo;
- la toolchain 1.85.1 per controllare in locale la MSRV pubblicata;
- CPython da 3.10 a 3.14;
- Maturin compatibile con pyproject.toml.

La CI usa Linux (gate Docker) e Windows (job nativo). macOS può essere un
ambiente di sviluppo, ma non è una piattaforma distribuita o supportata.

## Struttura del repository

~~~text
.github/workflows             verifica PR/main e pubblicazione dei tag
crates/rest-cli               binario CLI plenora-rest (plenora-cli-v2)
crates/rest-engine-core       motore Rust e runtime binding
crates/rest-engine-python     estensione PyO3
crates/rest-campaign          campagna operativa (smoke, load, soak), non pubblicata
campaign                      profili, soglie e report della campagna
python/plenora_rest           SDK Python pubblico
python/tests                  test black-box della wheel installata
contracts/schemas             schemi JSON component-owned
contracts/bindings            mapping degli entrypoint Rust
contracts/compatibility-v1.json baseline immutabile v1
examples                      esempi provider-neutral
fuzz                          target cargo-fuzz e corpus iniziale (workspace separato)
scripts                       verifica, contratti e release
~~~

## Ciclo rapido Rust

Durante lo sviluppo è possibile eseguire controlli mirati:

~~~powershell
cargo check --workspace --all-targets --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --exclude plenora-rest-campaign --lib --bins --locked -- -D clippy::unwrap_used -D clippy::expect_used -D clippy::panic -D clippy::indexing_slicing -D clippy::unreachable -D clippy::todo -D clippy::unimplemented
cargo test --workspace --locked
cargo +1.85.1 check --workspace --all-targets --locked
~~~

rust-toolchain.toml fissa lo stesso compilatore in locale, nella CI Linux
e Windows e nella build di release; cambiarlo è una modifica esplicita che
riesegue il gate completo. La MSRV pubblicata (rust-version in Cargo.toml)
è una promessa distinta, verificata dallo stage msrv del gate Docker.

Il secondo Clippy è il gate anti-panic: nel codice delle librerie e del
binario CLI niente unwrap, expect, panic, unreachable, todo, unimplemented e
niente indicizzazioni o slicing che possano uscire dai limiti. Un invariante
interno violato diventa un errore tipizzato (RUNTIME_ERROR), mai un panic
dentro l'host. I test ne sono esenti.

## CLI

Il binario si prova come un orchestratore lo usa:

~~~powershell
cargo run -p plenora-rest-cli -- --help
cargo run -p plenora-rest-cli -- capabilities --format json
cargo run -p plenora-rest-cli -- test --input request.json --config engine.json --format json
cargo test -p plenora-rest-cli
~~~

I test di unità coprono il parser (ogni flag sconosciuto, valore mancante,
flag ripetuto, posizionale in più, formato diverso da json, comando
sconosciuto, argomento non Unicode), la proiezione degli exit code, la
conversione dei risultati e il panic convertito in errore internal. I test
black-box in crates/rest-cli/tests/cli.rs lanciano il binario compilato e
controllano un solo documento JSON più newline su stdout, stderr vuoto, exit
code per categoria (0, 2, 4, 5, 6 e, su Unix, 130 con un SIGINT vero) e la
validità di ogni envelope contro gli schemi comuni cli-envelope-v2, error-v1 e
capabilities-v2. Gli schemi sono copiati da plenora-contracts in
crates/rest-cli/tests/fixtures/contracts, con il digest canonico verificato dal
test; li interpreta un validatore minimo in tests/support/schema.rs, chiuso
sulle parole chiave e sui pattern che conosce, invece di una dipendenza
jsonschema che porterebbe decine di crate in Cargo.lock per tre schemi di
test.

## Fuzz e test di proprietà

I parser dell'input remoto non fidato (corpo della risposta JSON, NDJSON,
CSV, XML, testo e binario; percorsi JSON; header Link, Retry-After,
Content-Range, ETag, Cache-Control, Set-Cookie) e dell'input di richiesta
(ExecutionRequest, RuntimeMessage) hanno due livelli di prova.

I test di proprietà stanno nel crate core
(`crates/rest-engine-core/src/property_tests.rs`) e girano con
`cargo test --workspace`, quindi in ogni gate. Ogni proprietà ha un oracolo
scritto nel test: un interprete di riferimento per i percorsi JSON, scrittori
CSV, NDJSON e XML che conoscono già il valore atteso, la grammatica di RFC 8288
e RFC 9110 per gli header, la regola documentata dei riferimenti runtime.
L'input rifiutato porta un canarino che nessun errore deve riportare. Il seme
e il numero di casi sono fissati nel codice e la prova non scrive file: la
suite è deterministica e dura meno di un secondo. Un controesempio trovato
diventa un test unitario accanto al parser che corregge.

I target di fuzz stanno in `fuzz/`, crate `cargo-fuzz` con workspace e
Cargo.lock propri, fuori dalla MSRV: target, proprietà, formato degli input,
cosa non è raggiungibile e comandi sono in [fuzz/README.md](../fuzz/README.md).
Raggiungono i parser privati con la feature `fuzzing` del crate core, che
abilita un modulo `doc(hidden)` non pubblico e fuori dal contratto v1. I due
target che eseguono il motore riscrivono ogni URL in un loopback letterale
che la configurazione di default rifiuta: nessuna connessione, nessun DNS.

Il workflow Fuzz controlla formattazione e compilazione dei target e
l'allineamento del corpus iniziale su ogni pull request e push a main; la
campagna gira ogni lunedì e a mano per 600 secondi a target, sulle pull
request che toccano `fuzz/` per 60 secondi. Un crash o un timeout rendono rosso
il job e i reperti restano come artefatto. libFuzzer non gira su Windows MSVC:
in locale le campagne si lanciano su Linux.

## Gate completo

Prima di un merge:

~~~powershell
pwsh ./scripts/verify.ps1
~~~

Il gate costruisce ambienti self-contained e verifica:

1. cargo check dell'intero workspace con Rust 1.85.1 e Cargo.lock;
2. JSON Schema Draft 2020-12, un corpus di istanze valide e non valide per ogni
   schema, e compatibilità delle superfici v1;
3. rustfmt;
4. Clippy con tutti i warning negati;
5. Clippy anti-panic sulle librerie e sul binario CLI;
6. test unitari e black-box Rust;
7. baseline breve di concorrenza, fault transitori e streaming;
8. rustdoc dell'API con tutti i warning negati;
9. build release della wheel;
10. installazione e test dello SDK in un ambiente Python pulito;
11. installazione della stessa wheel ABI3 su CPython 3.10-3.14.

La baseline breve usa server locali e carichi deterministici. Non sostituisce
la campagna finale di staging descritta nella roadmap.

Il workflow Verify esegue lo stesso script su pull request, push a main e
avvio manuale. Nello stesso workflow il job Windows esegue rustfmt, Clippy con
`-D warnings`, il Clippy anti-panic, `cargo test --workspace --locked` e
rustdoc con il compilatore di rust-toolchain.toml, costruisce la wheel abi3 win_amd64 con maturin 1.14.1 e la
prova installata su CPython 3.10-3.14 con scripts/test_wheel.ps1, che esegue
python/tests da fuori del checkout.

## Campagna operativa

La campagna operativa è separata dal gate: dura da minuti a ore e il suo esito
dipende dalla macchina. cargo test esegue soltanto i test dell'harness (una
campagna in-process di pochi secondi e il verificatore su report con
violazioni). Le fasi si lanciano con:

~~~bash
scripts/campaign.sh smoke|load|soak [--quick] [--duration-min N] [--seed N]
~~~

Lo script compila il binario in release con la toolchain dei gate
(CAMPAIGN_RUST_TOOLCHAIN, default 1.98.1) e scrive
campaign-out/<data>-<fase>.json e .md. RSS, descriptor e thread si misurano
solo su Linux; load e soak su altre piattaforme falliscono per misure mancanti
invece di passare. I report delle esecuzioni reali da conservare vanno in
campaign/reports. Il workflow Campaign esegue le stesse fasi su runner GitHub.
Profili, soglie, criteri e lavoro residuo sono descritti nella
[roadmap](roadmap.md#campagna-come-codice).

## Copertura

Il workflow Coverage misura, su ogni pull request e push a main, tre superfici
separate con cargo-llvm-cov 0.9.1 e coverage.py 7.16.1:

- il core Rust (crates/rest-engine-core/src), con i test del workspace;
- il binding PyO3 (crates/rest-engine-python/src), dalla wheel instrumentata
  installata in un ambiente pulito ed esercitata da python/tests;
- lo SDK Python (plenora_rest), dalla stessa esecuzione, con i rami.

scripts/check_coverage.py confronta ogni report con il minimo dichiarato in
scripts/coverage_budget.json e fallisce chiuso se il report è incoerente o
misura file fuori dai sorgenti dichiarati; scripts/test_check_coverage.py ne
prova i rifiuti. I minimi partono appena sotto i valori misurati: alzarli è
una modifica ordinaria del budget, abbassarli va motivato nella pull request.

## Versioni delle dipendenze

Le dipendenze dirette in Cargo.toml sono fissate alla versione esatta
(`=x.y.z`) presente in Cargo.lock: il manifest dichiara così il grafo che i
gate hanno qualificato, e ogni cambio di versione è una modifica esplicita del
manifest in una pull request dedicata, non l'effetto di un `cargo update`. Le
dipendenze transitive sono fissate da Cargo.lock, usato con --locked da ogni
gate.

Il costo è dichiarato: un crate che dipenda da plenora-rest-core eredita i pin
esatti, e la risoluzione fallisce se richiede un'altra versione delle stesse
dipendenze (per esempio un serde o un tokio diversi). Il crate oggi è
distribuito come artifact di release, non su crates.io; se diventasse una
dipendenza di altri crate, i pin andrebbero rivalutati per quel caso.

Per aggiornare una dipendenza:

~~~powershell
cargo update -p <crate> --precise <versione>
~~~

poi allineare il pin in Cargo.toml e far girare il gate completo e il workflow
Audit. Una versione che dichiara una rust-version oltre la MSRV non si adotta
senza un innalzamento esplicito della MSRV.

## Audit delle dipendenze

Il workflow Audit esegue cargo-deny secondo la policy in deny.toml (advisory,
licenze, sorgenti, wildcard), sul Cargo.lock del workspace e su quello di
`fuzz/`, e pip-audit sulla toolchain Python fissata dai gate. Gira su pull
request, push a main, una volta al giorno e su avvio manuale. L'unica
eccezione di licenza è NCSA per libfuzzer-sys, che esiste solo nel lock di
`fuzz/`; sul workspace cargo-deny la segnala come eccezione non usata, un
avviso e non un errore.

Ogni advisory accettata in deny.toml lo è soltanto perché nessuna versione
corretta della dipendenza compila sulla MSRV pubblicata. Il workflow esegue
scripts/check_advisory_exemptions.py, che fallisce se una di quelle eccezioni
sopravvive a un innalzamento della MSRV.

È deliberatamente separato dal gate di verifica: interroga un database di
advisory esterno, quindi il suo esito può cambiare a parità di commit. Tenerlo
fuori da verify.ps1 mantiene riproducibile il gate di release.

## Modifiche ai contratti

Per una modifica interna che non tocca il confine pubblico:

1. modificare implementazione e test;
2. verificare che compatibility-v1.json resti invariato;
3. eseguire il gate completo.

Per una modifica pubblica:

1. stabilire se il cambiamento è compatibile;
2. per un breaking change creare una nuova versione di schema e binding;
3. mantenere i file v1 immutati;
4. aggiornare capability, binding, test delle superfici e adozione;
5. rigenerare un baseline soltanto come risultato di una revisione esplicita;
6. eseguire il gate completo.

Il comando seguente stampa la superficie osservata dal validatore, ma non
modifica file:

~~~powershell
python ./scripts/validate_contracts.py --print-baseline
~~~

## Build della release

La release locale riproducibile è:

~~~powershell
pwsh ./scripts/release.ps1
~~~

Lo script:

- costruisce crate, wheel e binario CLI Linux due volte senza cache;
- confronta nomi e byte degli artefatti (esattamente un .crate, una .whl e
  plenora-rest-linux-x86_64; qualunque altro file è un errore);
- copia il risultato in dist;
- genera un SBOM SPDX 2.3;
- aggiunge la wheel Windows e plenora-rest-windows-x86_64.exe indicate con
  -ExtraArtifacts (nel workflow Release; la directory deve contenere
  esattamente quei due file);
- genera SHA256SUMS;
- confronta i digest con adoption-manifest.json.

adoption-manifest.json registra i digest degli artefatti riproducibili
costruiti nell'immagine Linux fissata per digest (crate, wheel manylinux e
binario CLI Linux). Il binario Linux è costruito nello stesso stage di crate e
wheel, con lo stesso compilatore, SOURCE_DATE_EPOCH, rimappatura dei path e
strip dei simboli. Wheel ed eseguibile Windows sono linkati su un runner la cui
immagine non è fissata: sono in SHA256SUMS, nell'SBOM e nelle attestazioni di
provenance, ma non hanno un digest nel manifesto da confrontare.

Gli artefatti prodotti sono:

- plenora-rest-core versione corrente in formato crate;
- wheel plenora-rest ABI3 manylinux2014 x86_64;
- wheel plenora-rest ABI3 win_amd64, costruita dal workflow Release su
  Windows e provata su CPython 3.10-3.14 prima di entrare nel pacchetto;
- binario CLI plenora-rest-linux-x86_64 (manylinux2014, glibc 2.17 o
  successiva);
- binario CLI plenora-rest-windows-x86_64.exe, costruito dallo stesso job
  Windows della wheel, che ne controlla `--version --format json` prima di
  conservarlo;
- SBOM SPDX JSON;
- SHA256SUMS.

## Preparazione di una nuova versione

La stessa versione deve essere presente in:

1. Cargo.toml del workspace;
2. pyproject.toml;
3. contracts/bindings/rust-v1.json;
4. tutti gli artifact di adoption-manifest.json;
5. release-metadata.json.

release-metadata.json deve anche contenere un SOURCE_DATE_EPOCH positivo. Dopo
l'aggiornamento della versione:

1. eseguire il gate completo;
2. generare una prima build con
   pwsh ./scripts/release.ps1 -SkipManifestCheck;
3. aggiornare in adoption-manifest.json i digest del crate, della wheel e
   del binario CLI Linux ottenuti dalla build;
4. rieseguire pwsh ./scripts/release.ps1 senza esclusioni;
5. aprire e verificare la pull request;
6. unire su main;
7. creare e pubblicare il tag annotato vX.Y.Z.

Esempio dell'ultimo passaggio:

~~~powershell
git tag -a vX.Y.Z -m "plenora-rest-tools X.Y.Z"
git push origin vX.Y.Z
~~~

Il workflow Release controlla che tag e cinque fonti di versione coincidano,
riesegue il gate, ricostruisce gli artefatti, genera attestazioni di provenance
e SBOM e pubblica una GitHub Release.

Il workflow non pubblica automaticamente su crates.io o PyPI.

## Regole per la documentazione

I documenti descrivono soltanto:

- comportamento presente verificabile nel codice;
- contratti pubblici correnti;
- supporto realmente coperto dai gate;
- lavoro futuro nella sola roadmap.

Versioni, revisioni e digest che hanno una fonte machine-readable non devono
essere copiati in più documenti. La storia delle decisioni appartiene a Git,
alle pull request e alle release.

Ogni modifica documentale deve mantenere:

- UTF-8 valido;
- link relativi risolvibili;
- esempi coerenti con l'API pubblica;
- nessun riferimento a file o comandi rimossi.

## Riferimenti

- [Panoramica](../README.md)
- [Architettura](architecture.md)
- [Contratti](../contracts/README.md)
- [Roadmap](roadmap.md)
