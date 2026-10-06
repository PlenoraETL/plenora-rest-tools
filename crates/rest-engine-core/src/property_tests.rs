//! Test di proprietà dei parser dell'input remoto e di richiesta.
//!
//! Ogni proprietà ha un oracolo scritto qui, indipendente dal codice che
//! verifica: un interprete di riferimento per i percorsi JSON, uno scrittore
//! CSV/NDJSON/XML che produce l'input e conosce già il valore atteso, la
//! grammatica RFC 8288 per l'header `Link`, quella RFC 9110 per `Retry-After`
//! e `Content-Range`, la regola documentata dei riferimenti runtime.
//!
//! Le esecuzioni sono deterministiche: seme fisso, numero di casi fissato nel
//! codice (le variabili d'ambiente di proptest non lo cambiano) e nessun file
//! di regressione scritto durante la prova. Un controesempio trovato diventa
//! un test unitario esplicito accanto al parser che corregge.

use std::{
    collections::BTreeMap,
    time::{Duration, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use proptest::{
    prelude::*,
    test_runner::{Config, RngSeed},
};
use serde_json::{Map, Value};

use crate::{EngineError, ResponseConfig, ResponseFormat};

/// Testo che nessun messaggio d'errore deve riportare: lo si inserisce
/// nell'input rifiutato e si controlla che non torni indietro.
const CANARY: &str = "Zq7canaryXv9";

fn config(cases: u32) -> Config {
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x5EED_2026),
        failure_persistence: None,
        ..Config::default()
    }
}

fn response(format: ResponseFormat, delimiter: &str) -> ResponseConfig {
    ResponseConfig {
        format,
        delimiter: delimiter.to_owned(),
        ..ResponseConfig::default()
    }
}

fn assert_no_canary(error: &EngineError) {
    let payload = serde_json::to_string(&error.payload()).expect("payload serializzabile");
    for rendered in [error.to_string(), format!("{error:?}"), payload] {
        assert!(
            !rendered.contains(CANARY),
            "l'errore riporta dati dell'input"
        );
    }
    assert_eq!(error.to_string(), error.payload().message);
}

fn text_from(characters: Vec<char>) -> String {
    characters.into_iter().collect()
}

/// Stringhe con ogni carattere, controlli e newline compresi.
fn any_text(max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..max).prop_map(text_from)
}

/// Valori JSON senza numeri in virgola mobile, il cui round-trip testuale
/// dipende dalla stampa del float e non dal parser.
fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        any_text(8).prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map(any_text(4), inner, 0..4)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

// ---------------------------------------------------------------------------
// Percorsi JSON: interprete di riferimento sulla grammatica documentata.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum PathSegment {
    Key(String),
    Index(isize),
    Filter { key: String, value: String },
}

#[derive(Clone, Debug)]
struct RenderedPath {
    segments: Vec<PathSegment>,
    text: String,
}

fn path_key() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "c", "id", "kind"]).prop_map(str::to_owned)
}

/// Una chiave che solo la forma tra parentesi può esprimere.
fn bracket_key() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "x y", "a.b", "é", "$", "1x"]).prop_map(str::to_owned)
}

fn filter_value() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "1", "2", "true", "null", "x y"]).prop_map(str::to_owned)
}

fn path_segment() -> impl Strategy<Value = (PathSegment, String)> {
    prop_oneof![
        path_key().prop_map(|key| (PathSegment::Key(key.clone()), format!(".{key}"))),
        (bracket_key(), any::<bool>()).prop_map(|(key, double)| {
            let quote = if double { '"' } else { '\'' };
            (
                PathSegment::Key(key.clone()),
                format!("[{quote}{key}{quote}]"),
            )
        }),
        (-4_isize..4, any::<bool>()).prop_map(|(index, spaced)| {
            let text = if spaced {
                format!("[ {index} ]")
            } else {
                format!("[{index}]")
            };
            (PathSegment::Index(index), text)
        }),
        (path_key(), filter_value(), 0_u8..3).prop_map(|(key, value, quoting)| {
            let rendered = match quoting {
                0 => value.clone(),
                1 => format!("'{value}'"),
                _ => format!("\"{value}\""),
            };
            (
                PathSegment::Filter {
                    key: key.clone(),
                    value,
                },
                format!("[{key}={rendered}]"),
            )
        }),
    ]
}

fn json_path() -> impl Strategy<Value = RenderedPath> {
    (
        prop::collection::vec(path_segment(), 0..5),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(parts, dollar, padded)| {
            let mut text = String::new();
            if dollar {
                text.push('$');
            }
            let mut segments = Vec::new();
            for (index, (segment, rendered)) in parts.into_iter().enumerate() {
                // Senza `$` il primo segmento a punto si scrive anche senza
                // punto: `a.b` vale `.a.b`.
                if index == 0 && !dollar && matches!(segment, PathSegment::Key(_)) {
                    text.push_str(rendered.strip_prefix('.').unwrap_or(&rendered));
                } else {
                    text.push_str(&rendered);
                }
                segments.push(segment);
            }
            if padded {
                text = format!("  {text} ");
            }
            RenderedPath { segments, text }
        })
}

/// La semantica documentata, scritta senza guardare il parser: chiavi di
/// oggetto, indici di array (negativi dalla fine), filtro sul primo elemento
/// il cui campo stringa è uguale al letterale o il cui campo non stringa è
/// uguale al letterale letto come JSON.
fn reference_get<'a>(root: &'a Value, segments: &[PathSegment]) -> Option<&'a Value> {
    let mut current = root;
    for segment in segments {
        current =
            match segment {
                PathSegment::Key(key) => current.as_object()?.get(key)?,
                PathSegment::Index(index) => {
                    let items = current.as_array()?;
                    let position = if *index < 0 {
                        let back = index.unsigned_abs();
                        items.len().checked_sub(back)?
                    } else {
                        usize::try_from(*index).ok()?
                    };
                    items.get(position)?
                }
                PathSegment::Filter { key, value } => current.as_array()?.iter().find(|item| {
                    match item.as_object().and_then(|object| object.get(key)) {
                        Some(Value::String(text)) => text == value,
                        Some(other) => {
                            serde_json::from_str::<Value>(value).ok().as_ref() == Some(other)
                        }
                        None => false,
                    }
                })?,
            };
    }
    Some(current)
}

/// Documenti piccoli con le stesse chiavi dei percorsi, perché i percorsi
/// generati trovino spesso qualcosa.
fn path_document() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        (0_i64..3).prop_map(Value::from),
        filter_value().prop_map(Value::String),
    ];
    leaf.prop_recursive(5, 48, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map(prop_oneof![path_key(), bracket_key()], inner, 0..4)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

proptest! {
    #![proptest_config(config(1024))]

    #[test]
    fn json_path_matches_the_reference_interpreter(
        document in path_document(),
        path in json_path(),
    ) {
        prop_assert_eq!(
            crate::json_path::get(&document, &path.text),
            reference_get(&document, &path.segments)
        );
    }

    #[test]
    fn json_path_is_total_on_arbitrary_input(document in json_value(), path in any_text(16)) {
        // Nessun panic, e lo stesso esito a ogni chiamata.
        let first = crate::json_path::get(&document, &path);
        prop_assert_eq!(first, crate::json_path::get(&document, &path));
    }
}

// ---------------------------------------------------------------------------
// Corpi di risposta: lo scrittore conosce già il valore atteso.
// ---------------------------------------------------------------------------

fn csv_field() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            4 => prop::char::range('a', 'e'),
            1 => prop::sample::select(vec![',', ';', '\t', '|', '"', '\n', '\r', ' ', 'é', '😀']),
        ],
        0..6,
    )
    .prop_map(text_from)
}

fn csv_table() -> impl Strategy<Value = (Vec<String>, Vec<Vec<String>>)> {
    prop::collection::btree_set(
        csv_field().prop_filter("non vuoto", |name| !name.is_empty()),
        1..5,
    )
    .prop_flat_map(|headers| {
        let headers = headers.into_iter().collect::<Vec<_>>();
        let width = headers.len();
        (
            Just(headers),
            prop::collection::vec(prop::collection::vec(csv_field(), width), 0..6),
        )
    })
}

/// Il delimitatore: un byte ASCII qualsiasi tranne le virgolette e i
/// terminatori di riga, che il formato riserva.
fn csv_delimiter() -> impl Strategy<Value = u8> {
    (1_u8..0x80).prop_filter("byte riservato", |byte| {
        !matches!(byte, b'"' | b'\n' | b'\r')
    })
}

/// RFC 4180 con le virgolette solo dove servono: delimitatore, virgolette,
/// fine riga, spazi ai bordi, e il campo vuoto di una riga a una colonna (che
/// altrimenti sarebbe una riga vuota).
fn write_csv_row(fields: &[String], delimiter: u8, always_quote: bool, terminator: &str) -> String {
    let delimiter = char::from(delimiter);
    let rendered = fields
        .iter()
        .map(|field| {
            let needs_quotes = always_quote
                || field.contains(['"', '\n', '\r', delimiter])
                || (fields.len() == 1 && field.is_empty())
                || field.starts_with(' ')
                || field.ends_with(' ');
            if needs_quotes {
                format!("\"{}\"", field.replace('"', "\"\""))
            } else {
                field.clone()
            }
        })
        .collect::<Vec<_>>();
    let mut line = rendered.join(&delimiter.to_string());
    line.push_str(terminator);
    line
}

proptest! {
    #![proptest_config(config(512))]

    #[test]
    fn csv_written_then_parsed_gives_the_same_rows(
        (headers, rows) in csv_table(),
        delimiter in csv_delimiter(),
        always_quote in any::<bool>(),
        crlf in any::<bool>(),
    ) {
        let terminator = if crlf { "\r\n" } else { "\n" };
        let mut body = write_csv_row(&headers, delimiter, always_quote, terminator);
        for row in &rows {
            body.push_str(&write_csv_row(row, delimiter, always_quote, terminator));
        }
        let expected = Value::Array(
            rows.iter()
                .map(|row| {
                    Value::Object(
                        headers
                            .iter()
                            .cloned()
                            .zip(row.iter().cloned().map(Value::String))
                            .collect(),
                    )
                })
                .collect(),
        );
        let delimiter = char::from(delimiter).to_string();
        let parsed = crate::response_body::parse(
            body.as_bytes(),
            &response(ResponseFormat::Csv, &delimiter),
        );
        prop_assert_eq!(parsed.ok(), Some(expected));
    }

    #[test]
    fn csv_rows_of_another_width_are_refused_without_data(
        (headers, rows) in csv_table(),
        extra in 1_usize..3,
    ) {
        let mut body = write_csv_row(&headers, b',', true, "\n");
        let mut wider = rows.first().cloned().unwrap_or_else(|| vec![String::new(); headers.len()]);
        wider.extend(std::iter::repeat_n(CANARY.to_owned(), extra));
        body.push_str(&write_csv_row(&wider, b',', true, "\n"));
        let Err(error) = crate::response_body::parse(body.as_bytes(), &response(ResponseFormat::Csv, ","))
        else {
            return Err(TestCaseError::fail("una riga più larga deve essere rifiutata"));
        };
        prop_assert!(matches!(error, EngineError::InvalidResponse(_)));
        assert_no_canary(&error);
    }

    #[test]
    fn ndjson_written_then_parsed_gives_the_same_records(
        values in prop::collection::vec(json_value(), 0..6),
        blank in prop::collection::vec(0_u8..4, 6),
        crlf in any::<bool>(),
    ) {
        let terminator = if crlf { "\r\n" } else { "\n" };
        let mut body = String::new();
        for (value, blank) in values.iter().zip(blank.iter().cycle()) {
            match blank {
                0 => body.push_str(terminator),
                1 => body.push_str(&format!(" \t{terminator}")),
                _ => {}
            }
            let indent = if *blank == 3 { " \t" } else { "" };
            body.push_str(&format!("{indent}{}{indent}{terminator}", serde_json::to_string(value).unwrap()));
        }
        let parsed = crate::response_body::parse(body.as_bytes(), &response(ResponseFormat::Ndjson, ","));
        prop_assert_eq!(parsed.ok(), Some(Value::Array(values)));
    }

    #[test]
    fn an_invalid_ndjson_line_is_located_without_its_text(
        values in prop::collection::vec(json_value(), 0..5),
        broken in any::<prop::sample::Index>(),
    ) {
        let mut lines = values
            .iter()
            .map(|value| serde_json::to_string(value).unwrap())
            .collect::<Vec<_>>();
        let at = broken.index(lines.len() + 1);
        lines.insert(at, format!("{{\"{CANARY}\": {CANARY}}}"));
        let body = lines.join("\n");
        let Err(error) = crate::response_body::parse(body.as_bytes(), &response(ResponseFormat::Ndjson, ","))
        else {
            return Err(TestCaseError::fail("una riga non JSON deve essere rifiutata"));
        };
        let EngineError::InvalidResponse(detail) = &error else {
            return Err(TestCaseError::fail("errore di un altro tipo"));
        };
        prop_assert_eq!(detail.position().map(|(line, _)| line), Some(at as u64 + 1));
        assert_no_canary(&error);
    }

    #[test]
    fn json_written_then_parsed_gives_the_same_value(value in json_value(), pretty in any::<bool>()) {
        let body = if pretty {
            serde_json::to_vec_pretty(&value).unwrap()
        } else {
            serde_json::to_vec(&value).unwrap()
        };
        let parsed = crate::response_body::parse(&body, &response(ResponseFormat::Json, ","));
        prop_assert_eq!(parsed.ok(), Some(value));
    }

    #[test]
    fn text_and_binary_keep_every_byte(body in prop::collection::vec(any::<u8>(), 0..64)) {
        let binary = crate::response_body::parse(&body, &response(ResponseFormat::Binary, ",")).unwrap();
        let decoded = STANDARD.decode(binary["data_base64"].as_str().unwrap()).unwrap();
        prop_assert_eq!(&decoded, &body);
        prop_assert_eq!(binary["size"].as_u64(), Some(body.len() as u64));

        let text = crate::response_body::parse(&body, &response(ResponseFormat::Text, ","));
        match std::str::from_utf8(&body) {
            Ok(expected) => prop_assert_eq!(text.ok(), Some(Value::String(expected.to_owned()))),
            Err(_) => prop_assert!(matches!(text, Err(EngineError::InvalidResponse(_)))),
        }
    }
}

#[derive(Clone, Debug)]
struct XmlElement {
    name: String,
    attributes: BTreeMap<String, String>,
    text: String,
    cdata: bool,
    children: Vec<XmlElement>,
}

fn xml_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            4 => prop::char::range('a', 'e'),
            2 => prop::sample::select(vec![' ', '\t', '\n', '\r']),
            2 => prop::sample::select(vec!['&', '<', '>', '"', '\'', ';', '#', ']']),
            1 => prop::sample::select(vec!['é', '€', '😀']),
        ],
        0..10,
    )
    .prop_map(text_from)
}

fn xml_name() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "c", "item", "x-y", "z.1"]).prop_map(str::to_owned)
}

fn xml_element() -> impl Strategy<Value = XmlElement> {
    let leaf = (
        xml_name(),
        prop::collection::btree_map(xml_name(), xml_text(), 0..3),
        xml_text(),
        any::<bool>(),
    )
        .prop_map(|(name, attributes, text, cdata)| XmlElement {
            name,
            attributes,
            text,
            cdata,
            children: Vec::new(),
        });
    leaf.prop_recursive(4, 24, 4, |inner| {
        (
            xml_name(),
            prop::collection::btree_map(xml_name(), xml_text(), 0..3),
            xml_text(),
            any::<bool>(),
            prop::collection::vec(inner, 0..4),
        )
            .prop_map(|(name, attributes, text, cdata, children)| XmlElement {
                name,
                attributes,
                text,
                cdata,
                children,
            })
    })
}

fn escape_xml(text: &str, attribute: bool) -> String {
    let mut escaped = String::new();
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' if attribute => escaped.push_str("&quot;"),
            // Un riferimento numerico conserva il carattere: un a capo o una
            // tabulazione letterali in un attributo diventano spazi per la
            // normalizzazione di XML 1.0, un `\r` letterale nel testo diventa
            // `\n`.
            '\r' => escaped.push_str("&#xD;"),
            '\n' if attribute => escaped.push_str("&#xA;"),
            '\t' if attribute => escaped.push_str("&#x9;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn write_xml(element: &XmlElement, output: &mut String) {
    output.push('<');
    output.push_str(&element.name);
    for (name, value) in &element.attributes {
        output.push_str(&format!(" {name}=\"{}\"", escape_xml(value, true)));
    }
    output.push('>');
    if element.cdata && !element.text.contains("]]>") && !element.text.contains('\r') {
        output.push_str(&format!("<![CDATA[{}]]>", element.text));
    } else {
        output.push_str(&escape_xml(&element.text, false));
    }
    for child in &element.children {
        write_xml(child, output);
    }
    output.push_str(&format!("</{}>", element.name));
}

/// La forma documentata: attributi come `@nome`, figli per nome (un array
/// quando si ripetono, nell'ordine del documento), testo senza spazi ai bordi,
/// in `#text` quando l'elemento ha anche attributi o figli.
fn expected_xml(element: &XmlElement) -> Value {
    let text = element.text.trim();
    if element.attributes.is_empty() && element.children.is_empty() {
        return Value::String(text.to_owned());
    }
    let mut content = Map::new();
    for (name, value) in &element.attributes {
        content.insert(format!("@{name}"), Value::String(value.clone()));
    }
    for child in &element.children {
        let value = expected_xml(child);
        match content.get_mut(&child.name) {
            Some(Value::Array(values)) => values.push(value),
            Some(existing) => *existing = Value::Array(vec![existing.take(), value]),
            None => {
                content.insert(child.name.clone(), value);
            }
        }
    }
    if !text.is_empty() {
        content.insert("#text".to_owned(), Value::String(text.to_owned()));
    }
    Value::Object(content)
}

proptest! {
    #![proptest_config(config(512))]

    #[test]
    fn xml_written_then_parsed_gives_the_documented_shape(
        root in xml_element(),
        declaration in any::<bool>(),
    ) {
        let mut body = String::new();
        if declaration {
            body.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        }
        write_xml(&root, &mut body);
        let expected = Value::Object(Map::from_iter([(root.name.clone(), expected_xml(&root))]));
        let parsed = crate::response_body::parse(body.as_bytes(), &response(ResponseFormat::Xml, ","));
        prop_assert_eq!(parsed.ok(), Some(expected));
    }

    #[test]
    fn malformed_xml_is_refused_without_data(root in xml_element(), cut in any::<prop::sample::Index>()) {
        let mut body = String::new();
        write_xml(&root, &mut body);
        // Un documento troncato prima della chiusura della radice, con il
        // canarino in coda: mai accettato, mai riportato.
        let end = body.rfind("</").unwrap_or(0);
        let mut at = cut.index(end.max(1));
        while !body.is_char_boundary(at) {
            at -= 1;
        }
        let truncated = format!("{}{CANARY}", &body[..at]);
        let Err(error) = crate::response_body::parse(truncated.as_bytes(), &response(ResponseFormat::Xml, ","))
        else {
            return Err(TestCaseError::fail("un XML troncato deve essere rifiutato"));
        };
        prop_assert!(matches!(error, EngineError::InvalidResponse(_)));
        assert_no_canary(&error);
    }
}

// ---------------------------------------------------------------------------
// Header remoti: grammatica delle RFC.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum LinkParameter {
    /// Relazioni, resa quotata (con un eventuale carattere preceduto da `\`)
    /// o come token.
    Rel {
        relations: Vec<String>,
        quoted: bool,
        escape_at: Option<usize>,
    },
    Other {
        name: String,
        value: String,
        quoted: bool,
    },
}

#[derive(Clone, Debug)]
struct LinkEntry {
    target: String,
    parameters: Vec<LinkParameter>,
}

const RELATIONS: [&str; 6] = [
    "next",
    "NEXT",
    "prev",
    "last",
    "http://example.com/rel",
    "http://example.com/Rel",
];

fn link_target() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            4 => prop::char::range('a', 'z'),
            2 => prop::sample::select(vec!['/', '?', '=', '&', '.', ':', '-']),
            1 => prop::sample::select(vec![',', ';', '"', ' ', '<']),
        ],
        0..12,
    )
    .prop_map(text_from)
}

fn link_parameter() -> impl Strategy<Value = LinkParameter> {
    prop_oneof![
        (
            prop::collection::vec(prop::sample::select(RELATIONS.to_vec()), 1..3),
            any::<bool>(),
            prop::option::of(0_usize..32),
        )
            .prop_map(|(relations, quoted, escape_at)| {
                let relations = relations.into_iter().map(str::to_owned).collect::<Vec<_>>();
                // Un token non può contenere `:` o `/` né più relazioni.
                let quoted = quoted
                    || relations.len() > 1
                    || relations.iter().any(|relation| relation.contains(':'));
                LinkParameter::Rel {
                    relations,
                    quoted,
                    escape_at,
                }
            }),
        (
            prop::sample::select(vec!["title", "type", "anchor", "REL2"]),
            prop::collection::vec(
                prop_oneof![
                    3 => prop::char::range('a', 'z'),
                    1 => prop::sample::select(vec![',', ';', '"', '\\', '=', ' ', '<', '>']),
                ],
                1..8,
            ),
            any::<bool>(),
        )
            .prop_map(|(name, value, quoted)| {
                let value = text_from(value);
                let quoted = quoted
                    || !value
                        .chars()
                        .all(|character| character.is_ascii_lowercase());
                LinkParameter::Other {
                    name: name.to_owned(),
                    value,
                    quoted,
                }
            }),
    ]
}

fn quote_header_value(value: &str, escape_at: Option<usize>) -> String {
    let mut quoted = String::from("\"");
    for (index, character) in value.chars().enumerate() {
        if matches!(character, '"' | '\\') || escape_at == Some(index) {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

fn write_link_header(entries: &[LinkEntry], spaced: bool) -> String {
    let space = if spaced { " " } else { "" };
    entries
        .iter()
        .map(|entry| {
            let mut rendered = format!("<{}>", entry.target);
            for parameter in &entry.parameters {
                rendered.push_str(&format!("{space};{space}"));
                match parameter {
                    LinkParameter::Rel {
                        relations,
                        quoted,
                        escape_at,
                    } => {
                        let joined = relations.join(if spaced { "  " } else { " " });
                        let value = if *quoted {
                            quote_header_value(&joined, *escape_at)
                        } else {
                            joined
                        };
                        rendered.push_str(&format!("rel{space}={space}{value}"));
                    }
                    LinkParameter::Other {
                        name,
                        value,
                        quoted,
                    } => {
                        let value = if *quoted {
                            quote_header_value(value, None)
                        } else {
                            value.clone()
                        };
                        rendered.push_str(&format!("{name}={value}"));
                    }
                }
            }
            rendered
        })
        .collect::<Vec<_>>()
        .join(&format!(",{space}"))
}

/// RFC 8288: il primo link il cui primo parametro `rel` (le occorrenze
/// successive si ignorano, §3.3) contiene la relazione; i tipi di relazione
/// registrati si confrontano senza distinguere le maiuscole, quelli estesi
/// (URI) esattamente. Il valore quotato è letto senza i caratteri di escape.
fn reference_link_target(entries: &[LinkEntry], expected: &str) -> Option<String> {
    entries.iter().find_map(|entry| {
        let relations = entry
            .parameters
            .iter()
            .find_map(|parameter| match parameter {
                LinkParameter::Rel { relations, .. } => Some(relations),
                LinkParameter::Other { .. } => None,
            })?;
        relations
            .iter()
            .any(|relation| {
                if expected.contains(':') {
                    relation == expected
                } else {
                    relation.eq_ignore_ascii_case(expected)
                }
            })
            .then(|| entry.target.clone())
    })
}

fn link_entries() -> impl Strategy<Value = Vec<LinkEntry>> {
    prop::collection::vec(
        (link_target(), prop::collection::vec(link_parameter(), 0..4))
            .prop_map(|(target, parameters)| LinkEntry { target, parameters }),
        1..4,
    )
}

/// Il valore numerico di `Retry-After` come lo fissa RFC 9110: secondi
/// interi; un valore oltre la rappresentazione resta l'attesa più lunga
/// possibile, che la politica limita poi a `max_retry_after_ms`.
fn reference_retry_seconds(digits: &str) -> u64 {
    digits
        .bytes()
        .try_fold(0_u64, |total, digit| {
            total
                .checked_mul(10)
                .and_then(|total| total.checked_add(u64::from(digit - b'0')))
        })
        .map_or(u64::MAX, |seconds| seconds.saturating_mul(1_000))
}

proptest! {
    #![proptest_config(config(1024))]

    #[test]
    fn link_header_selects_the_rfc_8288_target(
        entries in link_entries(),
        spaced in any::<bool>(),
        expected in prop::sample::select(RELATIONS.to_vec()),
    ) {
        let header = write_link_header(&entries, spaced);
        let headers = BTreeMap::from([("link".to_owned(), header)]);
        let selected = crate::engine::link_header_target(&headers, expected);
        prop_assert_eq!(selected.ok(), Some(reference_link_target(&entries, expected)));
    }

    #[test]
    fn link_header_is_total_and_never_quotes_the_header(header in any_text(24), relation in any_text(6)) {
        let headers = BTreeMap::from([("link".to_owned(), format!("{header}{CANARY}"))]);
        match crate::engine::link_header_target(&headers, &relation) {
            Ok(Some(target)) => prop_assert!(headers["link"].contains(&target)),
            Ok(None) => {}
            Err(error) => {
                prop_assert!(matches!(error, EngineError::InvalidResponse(_)));
                assert_no_canary(&error);
            }
        }
    }

    #[test]
    fn retry_after_seconds_follow_rfc_9110(
        digits in prop_oneof![
            any::<u64>().prop_map(|seconds| seconds.to_string()),
            "[0-9]{1,30}",
        ],
        before in "[ \t]{0,2}",
        after in "[ \t]{0,2}",
    ) {
        let now = UNIX_EPOCH + Duration::from_millis(1_700_000_000_250);
        prop_assert_eq!(
            crate::transport::parse_retry_after(&format!("{before}{digits}{after}"), now),
            Some(reference_retry_seconds(&digits))
        );
    }

    #[test]
    fn retry_after_dates_wait_until_the_date(offset in -1_000_000_i64..1_000_000) {
        let now = UNIX_EPOCH + Duration::from_millis(1_700_000_000_250);
        let whole = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let date = if offset < 0 {
            whole - Duration::from_secs(offset.unsigned_abs())
        } else {
            whole + Duration::from_secs(offset.unsigned_abs())
        };
        let expected = date
            .duration_since(now)
            .map_or(0, |delay| u64::try_from(delay.as_millis()).unwrap());
        prop_assert_eq!(
            crate::transport::parse_retry_after(&httpdate::fmt_http_date(date), now),
            Some(expected)
        );
    }

    #[test]
    fn content_range_accepts_exactly_the_satisfied_ranges(
        start in prop_oneof![any::<u64>(), 0_u64..8],
        end in prop_oneof![any::<u64>(), 0_u64..8],
        total in prop_oneof![any::<u64>(), 0_u64..8],
        unit in prop::sample::select(vec!["bytes", "BYTES", "Bytes"]),
    ) {
        let parsed = crate::transport::parse_content_range(&format!("{unit} {start}-{end}/{total}"));
        if start <= end && end < total {
            let range = parsed.ok().unwrap();
            prop_assert_eq!((range.start, range.end, range.total), (start, end, total));
        } else {
            prop_assert!(matches!(parsed, Err(EngineError::InvalidResponse(_))));
        }
    }

    #[test]
    fn set_cookie_over_the_bound_is_never_stored(length in 8_000_usize..8_400) {
        let header = format!("token={}", "v".repeat(length - "token=".len()));
        let url = reqwest::Url::parse("https://example.com/").unwrap();
        let jar = crate::transport::BoundedJar::default();
        let value = reqwest::header::HeaderValue::from_str(&header).unwrap();
        reqwest::cookie::CookieStore::set_cookies(&jar, &mut std::iter::once(&value), &url);
        let stored = reqwest::cookie::CookieStore::cookies(&jar, &url);
        prop_assert_eq!(stored.is_some(), length <= crate::transport::MAX_SET_COOKIE_BYTES);
    }
}

// ---------------------------------------------------------------------------
// Riferimenti runtime: la regola documentata e lo schema del contratto.
// ---------------------------------------------------------------------------

/// Un segmento di riferimento, anche ostile.
fn reference_segment() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => "[A-Za-z0-9_.-]{1,6}",
        1 => Just("..".to_owned()),
        1 => Just(".".to_owned()),
        1 => Just(String::new()),
        1 => prop::sample::select(vec!["C:", "file:", "FILE:x", "é", "\\..", "a\\..\\b"]).prop_map(str::to_owned),
    ]
}

fn runtime_reference() -> impl Strategy<Value = String> {
    (
        prop::sample::select(vec![
            "",
            "vault://",
            "artifact://",
            "/",
            "\\",
            "s3://",
            "x:",
        ]),
        prop::collection::vec(reference_segment(), 1..5),
        prop::sample::select(vec!["/", "\\"]),
        prop::option::of(500_usize..520),
    )
        .prop_map(|(prefix, segments, separator, padded)| {
            let mut reference = format!("{prefix}{}", segments.join(separator));
            if let Some(length) = padded {
                while reference.len() < length {
                    reference.push('a');
                }
            }
            reference
        })
}

/// La grammatica dei riferimenti opachi dei contratti adottati
/// (`^[a-z][a-z0-9+.-]{1,31}:(//)?[^\s\\]+$`, mai `file:`, un segmento `.` o
/// `..`, `%2E`), riscritta byte per byte come oracolo indipendente, con le due
/// strette dichiarate dal motore: al più 512 byte e un resto non vuoto dopo
/// `//` (`artifact://` da solo non nomina niente), niente caratteri di
/// controllo.
fn reference_is_opaque(reference: &str) -> bool {
    let bytes = reference.as_bytes();
    if bytes.len() < 4 || bytes.len() > 512 {
        return false;
    }
    let Some(colon) = bytes.iter().position(|byte| *byte == b':') else {
        return false;
    };
    let scheme = &bytes[..colon];
    let scheme_ok = (2..=32).contains(&scheme.len())
        && scheme[0].is_ascii_lowercase()
        && scheme[1..].iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"+.-".contains(byte)
        });
    let rest = &reference[colon + 1..];
    let body = rest.strip_prefix("//").unwrap_or(rest);
    let forbidden_character = reference.chars().any(|character| {
        character == '\\'
            || character.is_whitespace()
            || character.is_control()
            || ('\u{1c}'..='\u{1f}').contains(&character)
    });
    let segments_with_delimiter: Vec<&str> = reference.split(['/', ':']).collect();
    // Un segmento `.` o `..` seguito da `/` o dalla fine: l'ultimo pezzo dopo
    // un `:` seguito da altro testo non conta, come nella regex.
    let mut dot_segment = false;
    let mut offset = 0;
    for segment in &segments_with_delimiter {
        let next = reference.as_bytes().get(offset + segment.len());
        if (*segment == "." || *segment == "..") && matches!(next, None | Some(b'/')) {
            dot_segment = true;
        }
        offset += segment.len() + 1;
    }
    scheme_ok
        && scheme != b"file"
        && !body.is_empty()
        && !forbidden_character
        && !reference.contains("%2E")
        && !reference.contains("%2e")
        && !dot_segment
}

proptest! {
    #![proptest_config(config(1024))]

    #[test]
    fn runtime_references_follow_the_documented_rule(reference in runtime_reference()) {
        let accepted = crate::runtime::validate_reference(&reference).is_ok();
        prop_assert_eq!(accepted, reference_is_opaque(&reference));
        if accepted {
            // Lo schema v1 del contratto: 1..=512 caratteri.
            let characters = reference.chars().count();
            prop_assert!((1..=512).contains(&characters));
        }
    }
}

#[test]
fn property_configuration_is_deterministic() {
    let first = config(1);
    assert_eq!(first.rng_seed, RngSeed::Fixed(0x5EED_2026));
    assert!(first.failure_persistence.is_none());
}
