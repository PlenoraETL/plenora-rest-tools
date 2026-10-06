//! `plenora-rest`: la superficie CLI (`plenora-cli-v2`) di plenora-rest-tools.
//!
//! Ogni comando operativo è una spellatura di un'operazione del catalogo e la
//! esegue con l'API pubblica di `plenora-rest-core`, come le superfici Rust,
//! Python e runtime. In modalità JSON il processo scrive esattamente un
//! documento JSON e un newline su stdout e niente su stderr, anche quando
//! qualcosa va in panic.

#![forbid(unsafe_code)]

mod args;
mod commands;
mod envelope;
mod signal;

use std::{
    ffi::OsString,
    io::{self, Write},
    panic::{self, AssertUnwindSafe},
    process::ExitCode,
};

use args::{Command, Invocation, Rejection};
use commands::OperationOutcome;
use envelope::{CliError, INTERNAL_EXIT, Identity, PANIC, exit_code, failure, no_details, success};

/// Che cosa scrivere, e dove, prima di uscire.
#[derive(Debug, PartialEq, Eq)]
enum Output {
    /// Testo umano su stdout (solo `--help`).
    Help(&'static str),
    /// Un documento JSON su stdout, nient'altro.
    Json { document: String, exit: u8 },
    /// Un messaggio umano su stderr: un'invocazione che non ha chiesto JSON.
    Human { message: &'static str, exit: u8 },
}

const HELP: &str = "\
plenora-rest: command-line surface of plenora-rest-tools (plenora-cli-v2)

Usage:
  plenora-rest --help
  plenora-rest --version --format json
  plenora-rest capabilities --format json
  plenora-rest <operation> --input REQUEST.json [--config ENGINE.json] --format json

Operations (capability operation, version 1):
  test       rest.test       request: plenora-rest-execution-request-v1
                             result:  plenora-rest-execution-result-v1
  generate   rest.generate   request: plenora-rest-execution-request-v1
                             result:  plenora-rest-execution-result-v1
  enrich     rest.enrich     request: plenora-rest-execution-request-v1
                             result:  plenora-rest-execution-result-v1
  download   rest.download   request: plenora-rest-file-transfer-input-v1
                             result:  plenora-rest-file-transfer-result-v1
  upload     rest.upload     request: plenora-rest-file-transfer-input-v1
                             result:  plenora-rest-file-transfer-result-v1

Options:
  --input PATH    the execution request (JSON); \"-\" reads standard input.
                  Its \"operation\" must match the command.
  --config PATH   the engine configuration (EngineConfig, JSON). Without it the
                  fail-closed defaults apply: no private networks, no proxies,
                  no insecure TLS, no local file transfers.
  --format json   the only machine format: one JSON document and a newline on
                  stdout, nothing on stderr.

Exit codes:
  0 ok; 2 invalid_plan, invalid_configuration; 3 schema, data_mapping,
  unsupported; 4 resource_limit; 5 io, protocol, authentication, authorization,
  timeout, transient; 6 execution; 70 internal; 130 cancelled (Ctrl-C).

Secrets are never accepted as command-line values: keep credentials in the
request or configuration file, or pass the request on standard input.
";

fn main() -> ExitCode {
    // In modalità JSON nessun panic può scrivere su stderr: l'hook è muto e il
    // panic diventa un errore `internal` qui sotto.
    panic::set_hook(Box::new(|_| {}));
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let json = args::requests_json(&arguments);
    let output = panic::catch_unwind(AssertUnwindSafe(|| execute(arguments)))
        .unwrap_or_else(|_| panicked(json, None));
    emit(output)
}

/// L'uscita di un panic: senza il suo messaggio, con l'identità del comando
/// quando era già nota.
fn panicked(json: bool, command: Option<Command>) -> Output {
    if json {
        Output::Json {
            document: failure(
                Identity::from_command(command),
                &PANIC.payload(no_details()),
            ),
            exit: INTERNAL_EXIT,
        }
    } else {
        Output::Human {
            message: PANIC.message,
            exit: INTERNAL_EXIT,
        }
    }
}

fn execute(arguments: Vec<OsString>) -> Output {
    match args::parse(arguments) {
        Ok(invocation) => {
            let command = command_of(&invocation);
            panic::catch_unwind(AssertUnwindSafe(|| dispatch(invocation)))
                .unwrap_or_else(|_| panicked(true, Some(command)))
        }
        Err(rejection) => rejected(rejection),
    }
}

fn command_of(invocation: &Invocation) -> Command {
    match invocation {
        Invocation::Help => Command::Help,
        Invocation::Version => Command::Version,
        Invocation::Capabilities => Command::Capabilities,
        Invocation::Operation { operation, .. } => Command::Operation(*operation),
    }
}

fn rejected(rejection: Rejection) -> Output {
    let error = CliError::invalid(rejection.error.code(), rejection.error.message());
    let exit = exit_code(error.category);
    if rejection.json {
        Output::Json {
            document: failure(
                Identity::from_command(rejection.command),
                &error.payload(no_details()),
            ),
            exit,
        }
    } else {
        Output::Human {
            message: error.message,
            exit,
        }
    }
}

fn dispatch(invocation: Invocation) -> Output {
    match invocation {
        Invocation::Help => Output::Help(HELP),
        Invocation::Version => Output::Json {
            document: success(Identity::of(Command::Version), envelope::version_result()),
            exit: 0,
        },
        Invocation::Capabilities => {
            let identity = Identity::of(Command::Capabilities);
            match commands::capability_document() {
                Ok(document) => Output::Json {
                    document: success(identity, document),
                    exit: 0,
                },
                Err(error) => error_output(identity, &error.payload(no_details())),
            }
        }
        Invocation::Operation {
            operation,
            input,
            config,
        } => {
            let identity = Identity::of(Command::Operation(operation));
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => {
                    let error = CliError::internal(
                        "RUNTIME_UNAVAILABLE",
                        "the asynchronous runtime could not be started",
                    );
                    return error_output(identity, &error.payload(no_details()));
                }
            };
            let outcome = runtime.block_on(commands::run_operation(operation, input, config));
            // Una lettura bloccata dello standard input non deve trattenere
            // il processo dopo la risposta.
            runtime.shutdown_background();
            match outcome {
                OperationOutcome::Success(result) => Output::Json {
                    document: success(identity, result),
                    exit: 0,
                },
                OperationOutcome::Failure(error) => error_output(identity, &error),
            }
        }
    }
}

fn error_output(identity: Identity, error: &plenora_rest_core::ErrorPayload) -> Output {
    Output::Json {
        document: failure(identity, error),
        exit: exit_code(error.category),
    }
}

/// Scrive l'uscita. Un errore di scrittura (stdout chiuso) non può essere
/// riportato altrove senza violare il flusso macchina: resta l'exit code, e
/// un successo che non si è potuto scrivere non esce con 0.
fn emit(output: Output) -> ExitCode {
    let (written, exit) = match output {
        Output::Help(text) => (write_line(&mut io::stdout().lock(), text), 0),
        Output::Json { document, exit } => (write_line(&mut io::stdout().lock(), &document), exit),
        Output::Human { message, exit } => (
            write_line(
                &mut io::stderr().lock(),
                &format!("plenora-rest: {message}"),
            ),
            exit,
        ),
    };
    if written.is_err() && exit == 0 {
        return ExitCode::from(envelope::OUTPUT_FAILURE_EXIT);
    }
    ExitCode::from(exit)
}

fn write_line(stream: &mut impl Write, text: &str) -> io::Result<()> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    stream.write_all(text.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use serde_json::Value;

    use super::{HELP, Output, execute, panicked};

    fn run(tokens: &[&str]) -> Output {
        execute(tokens.iter().map(OsString::from).collect())
    }

    #[test]
    fn a_panic_becomes_an_internal_error_envelope() {
        let Output::Json { document, exit } = panicked(true, Some(super::Command::Capabilities))
        else {
            panic!("JSON mode must answer with JSON");
        };
        assert_eq!(exit, 70);
        let value: Value = serde_json::from_str(&document).unwrap();
        assert_eq!(value["status"], "error");
        assert_eq!(value["command"], "capabilities");
        assert_eq!(value["contract"], "plenora-capabilities-v2");
        assert_eq!(value["error"]["category"], "internal");
        assert_eq!(value["error"]["code"], "INTERNAL_PANIC");
    }

    #[test]
    fn a_caught_panic_is_reported_without_its_message() {
        let caught = std::panic::catch_unwind(|| -> Output { panic!("secret-value") })
            .unwrap_or_else(|_| panicked(true, None));
        let Output::Json { document, exit } = caught else {
            panic!("JSON mode must answer with JSON");
        };
        assert_eq!(exit, 70);
        assert!(!document.contains("secret-value"));
        let value: Value = serde_json::from_str(&document).unwrap();
        assert_eq!(value["command"], "unknown");
        assert_eq!(value["contract"], "plenora-error-v1");
    }

    #[test]
    fn help_lists_only_the_compiled_commands() {
        assert_eq!(run(&["--help"]), Output::Help(HELP));
        for command in [
            "--version",
            "capabilities",
            "test",
            "generate",
            "enrich",
            "download",
            "upload",
        ] {
            assert!(HELP.contains(command), "{command}");
        }
    }

    #[test]
    fn rejections_without_json_go_to_stderr_with_exit_2() {
        assert_eq!(
            run(&["test", "--input", "x"]),
            Output::Human {
                message: "this command requires --format json",
                exit: 2
            }
        );
    }
}
