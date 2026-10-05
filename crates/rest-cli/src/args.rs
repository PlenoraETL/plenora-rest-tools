//! Parser degli argomenti, scritto a mano e chiuso per costruzione.
//!
//! La grammatica è piccola e fissa:
//!
//! ~~~text
//! plenora-rest --help
//! plenora-rest --version --format json
//! plenora-rest capabilities --format json
//! plenora-rest <test|generate|enrich|download|upload> --input PATH [--config PATH] --format json
//! ~~~
//!
//! Tutto ciò che non rientra fallisce: comandi e flag sconosciuti, la forma
//! `--flag=valore`, un flag ripetuto, un valore mancante (anche quando al suo
//! posto c'è un altro flag), un posizionale in più, un formato diverso da
//! `json`, un argomento non UTF-8. Nessun messaggio d'errore ripete il token
//! ricevuto: un argomento sbagliato può essere un segreto incollato nel posto
//! sbagliato.

use std::ffi::OsString;

use plenora_rest_core::{
    ERROR_CONTRACT, EXECUTION_RESULT_CONTRACT, ExecutionOperation, FILE_TRANSFER_RESULT_CONTRACT,
    REST_DOWNLOAD, REST_ENRICH, REST_GENERATE, REST_TEST, REST_UPLOAD,
};

/// Contratto del risultato di `--version`.
pub(crate) const VERSION_RESULT_CONTRACT: &str = "plenora-rest-version-result-v1";
/// Contratto del risultato di `capabilities`: il documento di discovery v2.
pub(crate) const CAPABILITIES_CONTRACT: &str = "plenora-capabilities-v2";

/// Un comando pubblico, con il suo nome canonico.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Help,
    Version,
    Capabilities,
    Operation(Operation),
}

/// Le cinque operazioni del catalogo, nella spellatura CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Test,
    Generate,
    Enrich,
    Download,
    Upload,
}

impl Operation {
    pub(crate) const ALL: [Self; 5] = [
        Self::Test,
        Self::Generate,
        Self::Enrich,
        Self::Download,
        Self::Upload,
    ];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Generate => "generate",
            Self::Enrich => "enrich",
            Self::Download => "download",
            Self::Upload => "upload",
        }
    }

    /// L'identificatore dell'operazione nel catalogo delle capability.
    pub(crate) const fn capability_id(self) -> &'static str {
        match self {
            Self::Test => REST_TEST,
            Self::Generate => REST_GENERATE,
            Self::Enrich => REST_ENRICH,
            Self::Download => REST_DOWNLOAD,
            Self::Upload => REST_UPLOAD,
        }
    }

    /// Il valore di `operation` che la richiesta deve dichiarare.
    pub(crate) const fn execution(self) -> ExecutionOperation {
        match self {
            Self::Test => ExecutionOperation::Test,
            Self::Generate => ExecutionOperation::Generate,
            Self::Enrich => ExecutionOperation::Enrich,
            Self::Download => ExecutionOperation::Download,
            Self::Upload => ExecutionOperation::Upload,
        }
    }

    /// Il contratto di output dichiarato dal catalogo per l'operazione.
    pub(crate) const fn output_contract(self) -> &'static str {
        match self {
            Self::Test | Self::Generate | Self::Enrich => EXECUTION_RESULT_CONTRACT,
            Self::Download | Self::Upload => FILE_TRANSFER_RESULT_CONTRACT,
        }
    }

    fn parse(token: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|operation| operation.name() == token)
    }
}

impl Command {
    /// Il nome canonico del comando nell'envelope.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Help => "help",
            Self::Version => "version",
            Self::Capabilities => "capabilities",
            Self::Operation(operation) => operation.name(),
        }
    }

    /// Il contratto del campo `contract` dell'envelope, anche in errore.
    pub(crate) const fn output_contract(self) -> &'static str {
        match self {
            Self::Help => ERROR_CONTRACT,
            Self::Version => VERSION_RESULT_CONTRACT,
            Self::Capabilities => CAPABILITIES_CONTRACT,
            Self::Operation(operation) => operation.output_contract(),
        }
    }

    fn parse(token: &str) -> Option<Self> {
        match token {
            "--help" => Some(Self::Help),
            "--version" => Some(Self::Version),
            "capabilities" => Some(Self::Capabilities),
            other => Operation::parse(other).map(Self::Operation),
        }
    }
}

/// Un'invocazione valida.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Invocation {
    Help,
    Version,
    Capabilities,
    Operation {
        operation: Operation,
        input: Source,
        config: Option<String>,
    },
}

/// Da dove si legge la richiesta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Stdin,
    File(String),
}

/// Perché gli argomenti sono stati rifiutati. Ogni variante ha un codice e un
/// messaggio statici.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArgumentError {
    MissingCommand,
    UnknownCommand,
    UnknownFlag,
    DuplicateFlag,
    MissingValue,
    UnexpectedArgument,
    UnsupportedFormat,
    MissingFormat,
    MissingInput,
    ConfigFromStdin,
    NotUnicode,
}

impl ArgumentError {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::MissingCommand => "MISSING_COMMAND",
            Self::UnknownCommand => "UNKNOWN_COMMAND",
            Self::UnknownFlag => "UNKNOWN_FLAG",
            Self::DuplicateFlag => "DUPLICATE_FLAG",
            Self::MissingValue => "MISSING_VALUE",
            Self::UnexpectedArgument => "UNEXPECTED_ARGUMENT",
            Self::UnsupportedFormat => "UNSUPPORTED_FORMAT",
            Self::MissingFormat => "MISSING_FORMAT",
            Self::MissingInput => "MISSING_INPUT",
            Self::ConfigFromStdin => "CONFIG_FROM_STDIN",
            Self::NotUnicode => "ARGUMENT_NOT_UNICODE",
        }
    }

    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::MissingCommand => "a command is required; run plenora-rest --help",
            Self::UnknownCommand => "the command is not known; run plenora-rest --help",
            Self::UnknownFlag => "a flag is not accepted by this command",
            Self::DuplicateFlag => "a flag was given more than once",
            Self::MissingValue => "a flag is missing its value",
            Self::UnexpectedArgument => "an unexpected positional argument was given",
            Self::UnsupportedFormat => "the only supported output format is json",
            Self::MissingFormat => "this command requires --format json",
            Self::MissingInput => "this command requires --input REQUEST.json",
            Self::ConfigFromStdin => "--config cannot read standard input; pass a file",
            Self::NotUnicode => "command-line arguments must be valid Unicode",
        }
    }
}

/// Un rifiuto, con quanto serve a scegliere il canale di uscita: il comando
/// riconosciuto (se il primo token lo era) e se l'invocazione chiedeva JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rejection {
    pub(crate) error: ArgumentError,
    pub(crate) command: Option<Command>,
    pub(crate) json: bool,
}

/// Se l'invocazione chiede la modalità macchina: la coppia `--format json`
/// compare fra gli argomenti, anche quando il resto è sbagliato. Così un
/// orchestratore riceve un envelope d'errore anche per una riga di comando
/// rifiutata.
pub(crate) fn requests_json(arguments: &[OsString]) -> bool {
    arguments
        .windows(2)
        .any(|pair| matches!(pair, [flag, value] if flag == "--format" && value == "json"))
}

/// Interpreta gli argomenti che seguono il nome del programma.
pub(crate) fn parse(arguments: Vec<OsString>) -> Result<Invocation, Rejection> {
    let json = requests_json(&arguments);
    let reject = |error, command| Rejection {
        error,
        command,
        json,
    };
    let mut tokens = Vec::with_capacity(arguments.len());
    for argument in arguments {
        match argument.into_string() {
            Ok(token) => tokens.push(token),
            Err(_) => return Err(reject(ArgumentError::NotUnicode, None)),
        }
    }
    let mut tokens = tokens.into_iter();
    let Some(first) = tokens.next() else {
        return Err(reject(ArgumentError::MissingCommand, None));
    };
    let Some(command) = Command::parse(&first) else {
        return Err(reject(ArgumentError::UnknownCommand, None));
    };
    let flags = Flags::collect(command, tokens).map_err(|error| reject(error, Some(command)))?;
    let fail = |error| Err(reject(error, Some(command)));
    match command {
        // `--help` è soltanto testo umano: `Flags::collect` non gli concede
        // alcun flag, nemmeno `--format json`, invece di ignorarli.
        Command::Help => Ok(Invocation::Help),
        Command::Version | Command::Capabilities => {
            if let Err(error) = flags.require_json() {
                return fail(error);
            }
            Ok(if command == Command::Version {
                Invocation::Version
            } else {
                Invocation::Capabilities
            })
        }
        Command::Operation(operation) => {
            if let Err(error) = flags.require_json() {
                return fail(error);
            }
            let input = match flags.input {
                None => return fail(ArgumentError::MissingInput),
                Some(path) if path == "-" => Source::Stdin,
                Some(path) => Source::File(path),
            };
            if flags.config.as_deref() == Some("-") {
                return fail(ArgumentError::ConfigFromStdin);
            }
            Ok(Invocation::Operation {
                operation,
                input,
                config: flags.config,
            })
        }
    }
}

#[derive(Default)]
struct Flags {
    format: Option<String>,
    input: Option<String>,
    config: Option<String>,
}

impl Flags {
    fn collect(
        command: Command,
        mut tokens: impl Iterator<Item = String>,
    ) -> Result<Self, ArgumentError> {
        let mut flags = Self::default();
        while let Some(token) = tokens.next() {
            let slot = match (token.as_str(), command) {
                ("--format", Command::Version | Command::Capabilities | Command::Operation(_)) => {
                    &mut flags.format
                }
                ("--input", Command::Operation(_)) => &mut flags.input,
                ("--config", Command::Operation(_)) => &mut flags.config,
                (other, _) if other.starts_with('-') && other != "-" => {
                    return Err(ArgumentError::UnknownFlag);
                }
                _ => return Err(ArgumentError::UnexpectedArgument),
            };
            if slot.is_some() {
                return Err(ArgumentError::DuplicateFlag);
            }
            // Un valore vuoto, o un altro flag al posto del valore, è un
            // valore mancante: `--input --format json` non legge un file
            // chiamato `--format`. `-` resta il nome dello standard input.
            let value = match tokens.next() {
                Some(value) if value.is_empty() => return Err(ArgumentError::MissingValue),
                Some(value) if value.starts_with("--") => {
                    return Err(ArgumentError::MissingValue);
                }
                Some(value) => value,
                None => return Err(ArgumentError::MissingValue),
            };
            *slot = Some(value);
        }
        Ok(flags)
    }

    fn require_json(&self) -> Result<(), ArgumentError> {
        match self.format.as_deref() {
            Some("json") => Ok(()),
            Some(_) => Err(ArgumentError::UnsupportedFormat),
            None => Err(ArgumentError::MissingFormat),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{
        ArgumentError, Command, Invocation, Operation, Rejection, Source, parse, requests_json,
    };

    fn args(tokens: &[&str]) -> Vec<OsString> {
        tokens.iter().map(OsString::from).collect()
    }

    fn rejected(tokens: &[&str]) -> Rejection {
        match parse(args(tokens)) {
            Ok(invocation) => panic!("accepted {tokens:?} as {invocation:?}"),
            Err(rejection) => rejection,
        }
    }

    #[test]
    fn discovery_commands_parse() {
        assert_eq!(parse(args(&["--help"])), Ok(Invocation::Help));
        assert_eq!(
            parse(args(&["--version", "--format", "json"])),
            Ok(Invocation::Version)
        );
        assert_eq!(
            parse(args(&["capabilities", "--format", "json"])),
            Ok(Invocation::Capabilities)
        );
    }

    #[test]
    fn every_operation_parses_with_flags_in_any_order() {
        for operation in Operation::ALL {
            assert_eq!(
                parse(args(&[
                    operation.name(),
                    "--input",
                    "request.json",
                    "--format",
                    "json"
                ])),
                Ok(Invocation::Operation {
                    operation,
                    input: Source::File("request.json".to_owned()),
                    config: None,
                })
            );
            assert_eq!(
                parse(args(&[
                    operation.name(),
                    "--format",
                    "json",
                    "--config",
                    "engine.json",
                    "--input",
                    "-",
                ])),
                Ok(Invocation::Operation {
                    operation,
                    input: Source::Stdin,
                    config: Some("engine.json".to_owned()),
                })
            );
        }
    }

    #[test]
    fn missing_and_unknown_commands_are_rejected() {
        let missing = rejected(&[]);
        assert_eq!(missing.error, ArgumentError::MissingCommand);
        assert_eq!(missing.command, None);
        assert!(!missing.json);

        for command in ["delete", "TEST", "help", "-h", "--test", "version", ""] {
            let rejection = rejected(&[command, "--format", "json"]);
            assert_eq!(rejection.error, ArgumentError::UnknownCommand, "{command}");
            assert_eq!(rejection.command, None);
            assert!(rejection.json);
        }
    }

    #[test]
    fn every_unknown_flag_is_rejected() {
        let cases: &[&[&str]] = &[
            &["--version", "--format", "json", "--verbose"],
            &["--version", "--input", "x", "--format", "json"],
            &["capabilities", "--config", "x", "--format", "json"],
            &["test", "--input", "x", "--format", "json", "--output", "y"],
            &["test", "--input=x", "--format", "json"],
            &["test", "--format=json", "--input", "x"],
            &["test", "-i", "x", "--format", "json"],
            &["test", "--input", "x", "--format", "json", "--help"],
            &["--help", "--format", "json"],
            &["--help", "--version"],
        ];
        for case in cases {
            assert_eq!(rejected(case).error, ArgumentError::UnknownFlag, "{case:?}");
        }
    }

    #[test]
    fn missing_values_are_rejected() {
        let cases: &[&[&str]] = &[
            &["--version", "--format"],
            &["test", "--format", "json", "--input"],
            &["test", "--input", "--format", "json"],
            &["test", "--input", "", "--format", "json"],
            &["test", "--input", "x", "--format", "json", "--config"],
            &["test", "--input", "x", "--config", "--format", "json"],
        ];
        for case in cases {
            assert_eq!(
                rejected(case).error,
                ArgumentError::MissingValue,
                "{case:?}"
            );
        }
    }

    #[test]
    fn extra_positionals_are_rejected() {
        let cases: &[&[&str]] = &[
            &["--help", "extra"],
            &["--version", "--format", "json", "extra"],
            &["capabilities", "extra", "--format", "json"],
            &["test", "request.json", "--format", "json"],
            &["test", "--input", "x", "--format", "json", "-"],
        ];
        for case in cases {
            assert_eq!(
                rejected(case).error,
                ArgumentError::UnexpectedArgument,
                "{case:?}"
            );
        }
    }

    #[test]
    fn duplicate_flags_are_rejected() {
        let cases: &[&[&str]] = &[
            &["--version", "--format", "json", "--format", "json"],
            &["test", "--input", "a", "--input", "b", "--format", "json"],
            &[
                "test", "--input", "a", "--config", "b", "--config", "c", "--format", "json",
            ],
        ];
        for case in cases {
            assert_eq!(
                rejected(case).error,
                ArgumentError::DuplicateFlag,
                "{case:?}"
            );
        }
    }

    #[test]
    fn formats_other_than_json_are_rejected() {
        for format in ["human", "JSON", "text", "yaml"] {
            let rejection = rejected(&["--version", "--format", format]);
            assert_eq!(
                rejection.error,
                ArgumentError::UnsupportedFormat,
                "{format}"
            );
            assert!(!rejection.json);
            assert_eq!(
                rejected(&["test", "--input", "x", "--format", format]).error,
                ArgumentError::UnsupportedFormat
            );
        }
        for case in [
            &["--version"][..],
            &["capabilities"][..],
            &["test", "--input", "x"][..],
        ] {
            let rejection = rejected(case);
            assert_eq!(rejection.error, ArgumentError::MissingFormat, "{case:?}");
            assert!(!rejection.json);
        }
    }

    #[test]
    fn operations_require_an_input_and_a_config_file() {
        let missing = rejected(&["upload", "--format", "json"]);
        assert_eq!(missing.error, ArgumentError::MissingInput);
        assert_eq!(missing.command, Some(Command::Operation(Operation::Upload)));
        assert!(missing.json);
        assert_eq!(
            rejected(&["test", "--input", "-", "--config", "-", "--format", "json"]).error,
            ArgumentError::ConfigFromStdin
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_arguments_are_rejected() {
        use std::os::unix::ffi::OsStringExt;
        let mut arguments = args(&["test", "--format", "json", "--input"]);
        arguments.push(OsString::from_vec(vec![0xff, 0xfe]));
        let rejection = parse(arguments).unwrap_err();
        assert_eq!(rejection.error, ArgumentError::NotUnicode);
        assert!(rejection.json);
    }

    #[cfg(windows)]
    #[test]
    fn non_unicode_arguments_are_rejected() {
        use std::os::windows::ffi::OsStringExt;
        let mut arguments = args(&["test", "--format", "json", "--input"]);
        arguments.push(OsString::from_wide(&[0xd800]));
        let rejection = parse(arguments).unwrap_err();
        assert_eq!(rejection.error, ArgumentError::NotUnicode);
        assert!(rejection.json);
    }

    #[test]
    fn json_mode_is_detected_from_the_pair_only() {
        assert!(requests_json(&args(&["x", "--format", "json"])));
        assert!(!requests_json(&args(&["--format"])));
        assert!(!requests_json(&args(&["json", "--format"])));
        assert!(!requests_json(&args(&["--format", "JSON"])));
    }

    #[test]
    fn messages_are_static_and_never_echo_the_argument() {
        let secret = "Bearer-s3cr3t";
        let rejection = rejected(&[secret, "--format", "json"]);
        assert!(!rejection.error.message().contains(secret));
        let rejection = rejected(&["test", secret, "--format", "json"]);
        assert!(!rejection.error.message().contains(secret));
    }

    #[test]
    fn output_contracts_follow_the_catalog() {
        assert_eq!(
            Command::Operation(Operation::Test).output_contract(),
            "plenora-rest-execution-result-v1"
        );
        assert_eq!(
            Command::Operation(Operation::Download).output_contract(),
            "plenora-rest-file-transfer-result-v1"
        );
        assert_eq!(
            Command::Version.output_contract(),
            "plenora-rest-version-result-v1"
        );
        assert_eq!(
            Command::Capabilities.output_contract(),
            "plenora-capabilities-v2"
        );
    }
}
