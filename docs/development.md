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

- Rust 1.85.1 o successivo;
- rustfmt e Clippy;
- CPython da 3.10 a 3.14;
- Maturin compatibile con pyproject.toml.

La CI usa Linux. Windows e macOS possono essere ambienti di sviluppo, ma non
sono ancora piattaforme distribuite o supportate.

## Struttura del repository

~~~text
.github/workflows             verifica PR/main e pubblicazione dei tag
crates/rest-engine-core       motore Rust e runtime binding
crates/rest-engine-python     estensione PyO3
python/plenora_rest           SDK Python pubblico
python/tests                  test black-box della wheel installata
contracts/schemas             schemi JSON component-owned
contracts/bindings            mapping degli entrypoint Rust
contracts/compatibility-v1.json baseline immutabile v1
examples                      esempi provider-neutral
scripts                       verifica, contratti e release
~~~

## Ciclo rapido Rust

Durante lo sviluppo è possibile eseguire controlli mirati:

~~~powershell
cargo check --workspace --all-targets --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
~~~

Il controllo con la toolchain locale non sostituisce il gate Docker che
verifica la MSRV fissata.

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
5. test unitari e black-box Rust;
6. baseline breve di concorrenza, fault transitori e streaming;
7. build release della wheel;
8. installazione e test dello SDK in un ambiente Python pulito;
9. installazione della stessa wheel ABI3 su CPython 3.10-3.14.

La baseline breve usa server locali e carichi deterministici. Non sostituisce
la campagna finale di staging descritta nella roadmap.

Il workflow Verify esegue lo stesso script su pull request, push a main e
avvio manuale.

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
licenze, sorgenti, wildcard) e pip-audit sulla toolchain Python fissata dai
gate. Gira su pull request, push a main, una volta al giorno e su avvio
manuale.

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

- costruisce crate e wheel due volte senza cache;
- confronta nomi e byte degli artefatti;
- copia il risultato in dist;
- genera un SBOM SPDX 2.3;
- genera SHA256SUMS;
- confronta i digest con adoption-manifest.json.

Gli artefatti prodotti sono:

- plenora-rest-core versione corrente in formato crate;
- wheel plenora-rest ABI3 manylinux2014 x86_64;
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
3. aggiornare in adoption-manifest.json i digest del crate e della wheel
   ottenuti dalla build;
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
