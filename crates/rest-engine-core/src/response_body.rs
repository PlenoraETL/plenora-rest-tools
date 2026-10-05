use std::collections::BTreeSet;

use crate::error::ErrorDetail;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use quick_xml::{Reader, XmlVersion, escape::resolve_predefined_entity, events::Event};
use serde_json::{Map, Value, json};

use crate::{EngineError, ResponseConfig, ResponseFormat};

pub(crate) fn parse(body: &[u8], config: &ResponseConfig) -> Result<Value, EngineError> {
    match config.format {
        // Only where the parser stopped is kept: serde's message can quote
        // the body it was reading.
        ResponseFormat::Json => serde_json::from_slice(body).map_err(|error| {
            EngineError::InvalidResponse(ErrorDetail::at(
                "response body is not valid JSON",
                error.line(),
                error.column(),
            ))
        }),
        ResponseFormat::Csv => parse_csv(body, &config.delimiter),
        ResponseFormat::Xml => parse_xml(body),
        ResponseFormat::Ndjson => parse_ndjson(body),
        ResponseFormat::Text => String::from_utf8(body.to_vec())
            .map(Value::String)
            .map_err(|_| {
                EngineError::InvalidResponse(ErrorDetail::from("response body is not valid UTF-8"))
            }),
        ResponseFormat::Binary => Ok(json!({
            "data_base64": STANDARD.encode(body),
            "size": body.len(),
        })),
    }
}

fn parse_ndjson(body: &[u8]) -> Result<Value, EngineError> {
    let text = std::str::from_utf8(body).map_err(|_| {
        EngineError::InvalidResponse(ErrorDetail::from("NDJSON response is not valid UTF-8"))
    })?;
    let mut values = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value = serde_json::from_str(line).map_err(|error| {
            EngineError::InvalidResponse(ErrorDetail::at(
                "NDJSON line is not valid JSON",
                index + 1,
                error.column(),
            ))
        })?;
        values.push(value);
    }
    Ok(Value::Array(values))
}

/// The line where the CSV reader stopped, never its message, which can quote
/// the field it was reading.
fn csv_detail(text: &'static str, error: &csv::Error) -> ErrorDetail {
    match error.position() {
        Some(position) => ErrorDetail::at(
            text,
            usize::try_from(position.line()).unwrap_or(usize::MAX),
            0,
        ),
        None => ErrorDetail::from(text),
    }
}

fn parse_csv(body: &[u8], delimiter: &str) -> Result<Value, EngineError> {
    let delimiter = delimiter.as_bytes();
    if delimiter.len() != 1 {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "CSV delimiter must be one ASCII byte",
        )));
    }
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter[0])
        .from_reader(body);
    let headers = reader
        .headers()
        .map_err(|error| EngineError::InvalidResponse(csv_detail("invalid CSV header", &error)))?
        .clone();
    if headers.iter().any(str::is_empty)
        || headers.iter().collect::<BTreeSet<_>>().len() != headers.len()
    {
        return Err(EngineError::InvalidResponse(ErrorDetail::from(
            "CSV response has missing or duplicate headers",
        )));
    }
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record
            .map_err(|error| EngineError::InvalidResponse(csv_detail("invalid CSV row", &error)))?;
        if record.len() != headers.len() {
            return Err(EngineError::InvalidResponse(ErrorDetail::from(
                "CSV row has a different width than its header",
            )));
        }
        rows.push(Value::Object(
            headers
                .iter()
                .zip(record.iter())
                .map(|(key, value)| (key.to_owned(), Value::String(value.to_owned())))
                .collect(),
        ));
    }
    Ok(Value::Array(rows))
}

struct XmlNode {
    name: String,
    content: Map<String, Value>,
    text: String,
}

fn parse_xml(body: &[u8]) -> Result<Value, EngineError> {
    // The reader does not trim text events: it reports every `&...;` as an
    // event of its own, so trimming each piece dropped the spaces around a
    // reference (`Fish &amp; Chips` read as `Fish&Chips`). The text of an
    // element is trimmed once, whole, when the element is attached.
    let mut reader = Reader::from_reader(body);
    let mut stack: Vec<XmlNode> = Vec::new();
    let mut root: Option<(String, Value)> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                if stack.len() >= 128 {
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "XML nesting exceeds the supported limit",
                    )));
                }
                stack.push(xml_node(&reader, &event)?);
            }
            Ok(Event::Empty(event)) => {
                let node = xml_node(&reader, &event)?;
                attach_xml_node(&mut stack, &mut root, node)?;
            }
            Ok(Event::Text(event)) => {
                if let Some(node) = stack.last_mut() {
                    let text = event.decode().map_err(|_| {
                        EngineError::InvalidResponse(ErrorDetail::from("XML contains invalid text"))
                    })?;
                    node.text.push_str(&text);
                }
            }
            Ok(Event::CData(event)) => {
                if let Some(node) = stack.last_mut() {
                    let text = event.decode().map_err(|_| {
                        EngineError::InvalidResponse(ErrorDetail::from(
                            "XML contains invalid CDATA",
                        ))
                    })?;
                    node.text.push_str(&text);
                }
            }
            Ok(Event::End(event)) => {
                let node = stack.pop().ok_or_else(|| {
                    EngineError::InvalidResponse(ErrorDetail::from(
                        "XML has an unexpected closing tag",
                    ))
                })?;
                if xml_name(event.name().as_ref()) != node.name {
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "XML closing tag does not match",
                    )));
                }
                attach_xml_node(&mut stack, &mut root, node)?;
            }
            // The reader reports every `&...;` in character data as its own
            // event. The five predefined entities and numeric character
            // references are part of well-formed XML and every real payload
            // uses them, so they are resolved here; anything else would need a
            // DTD, which stays refused.
            Ok(Event::GeneralRef(event)) => {
                let resolved = match event.resolve_char_ref() {
                    Ok(Some(character)) => String::from(character),
                    Ok(None) => {
                        let name = event.decode().map_err(|_| {
                            EngineError::InvalidResponse(ErrorDetail::from(
                                "XML contains an invalid entity reference",
                            ))
                        })?;
                        resolve_predefined_entity(&name)
                            .ok_or_else(|| {
                                EngineError::InvalidResponse(ErrorDetail::from(
                                    "XML DTDs and entity references are not allowed",
                                ))
                            })?
                            .to_owned()
                    }
                    Err(_) => {
                        return Err(EngineError::InvalidResponse(ErrorDetail::from(
                            "XML contains an invalid character reference",
                        )));
                    }
                };
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&resolved);
                }
            }
            Ok(Event::DocType(_)) => {
                return Err(EngineError::InvalidResponse(ErrorDetail::from(
                    "XML DTDs and entity references are not allowed",
                )));
            }
            Ok(Event::Eof) => break,
            Ok(Event::Decl(_) | Event::PI(_) | Event::Comment(_)) => {}
            Err(_) => {
                return Err(EngineError::InvalidResponse(ErrorDetail::from(
                    "response body is not valid XML",
                )));
            }
        }
    }
    if !stack.is_empty() {
        return Err(EngineError::InvalidResponse(ErrorDetail::from(
            "XML contains unclosed elements",
        )));
    }
    let (name, value) = root
        .ok_or_else(|| EngineError::InvalidResponse(ErrorDetail::from("XML response is empty")))?;
    Ok(Value::Object(Map::from_iter([(name, value)])))
}

fn xml_node(
    reader: &Reader<&[u8]>,
    event: &quick_xml::events::BytesStart<'_>,
) -> Result<XmlNode, EngineError> {
    let mut content = Map::new();
    for attribute in event.attributes().with_checks(true) {
        let attribute = attribute.map_err(|_| {
            EngineError::InvalidResponse(ErrorDetail::from("XML contains an invalid attribute"))
        })?;
        let key = format!("@{}", xml_name(attribute.key.as_ref()));
        // Attribute values are normalized as XML 1.0 requires. Absent an XML
        // declaration the specification assumes 1.0, and the 1.1 specific
        // newline forms are deliberately not honoured for remote payloads.
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|_| {
                EngineError::InvalidResponse(ErrorDetail::from(
                    "XML contains an invalid attribute value",
                ))
            })?;
        content.insert(key, Value::String(value.into_owned()));
    }
    Ok(XmlNode {
        name: xml_name(event.name().as_ref()),
        content,
        text: String::new(),
    })
}

fn attach_xml_node(
    stack: &mut [XmlNode],
    root: &mut Option<(String, Value)>,
    mut node: XmlNode,
) -> Result<(), EngineError> {
    let text = node.text.trim();
    let value = if node.content.is_empty() {
        Value::String(text.to_owned())
    } else {
        if !text.is_empty() {
            node.content
                .insert("#text".to_owned(), Value::String(text.to_owned()));
        }
        Value::Object(node.content)
    };
    if let Some(parent) = stack.last_mut() {
        match parent.content.get_mut(&node.name) {
            Some(Value::Array(values)) => values.push(value),
            Some(existing) => {
                let first = std::mem::replace(existing, Value::Null);
                *existing = Value::Array(vec![first, value]);
            }
            None => {
                parent.content.insert(node.name, value);
            }
        }
        Ok(())
    } else if root.is_none() {
        *root = Some((node.name, value));
        Ok(())
    } else {
        Err(EngineError::InvalidResponse(ErrorDetail::from(
            "XML response contains multiple root elements",
        )))
    }
}

fn xml_name(raw: &[u8]) -> String {
    let name = String::from_utf8_lossy(raw);
    name.rsplit(':').next().unwrap_or(&name).to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse, parse_csv, parse_xml};
    use crate::{ResponseConfig, ResponseFormat};

    #[test]
    fn a_parse_failure_keeps_its_position_and_not_the_body() {
        let config = ResponseConfig {
            format: ResponseFormat::Json,
            ..ResponseConfig::default()
        };
        let body = b"{\n  \"token\": \"abc123\" oops\n}";
        let Err(crate::EngineError::InvalidResponse(detail)) = parse(body, &config) else {
            panic!("invalid JSON must be refused");
        };
        assert_eq!(detail.text(), "response body is not valid JSON");
        assert_eq!(detail.position(), Some((2, 21)));
        assert!(!format!("{detail:?}").contains("abc123"));

        let config = ResponseConfig {
            format: ResponseFormat::Ndjson,
            ..ResponseConfig::default()
        };
        let Err(crate::EngineError::InvalidResponse(detail)) =
            parse(b"{\"a\":1}\n{\"secret\":\"abc123\"", &config)
        else {
            panic!("invalid NDJSON must be refused");
        };
        assert_eq!(detail.position().map(|(line, _)| line), Some(2));
        assert!(!format!("{detail:?}").contains("abc123"));
    }

    #[test]
    fn parses_csv_with_a_custom_delimiter() {
        let value = parse_csv(b"city;pop\nRoma;2873000\n", ";").unwrap();
        assert_eq!(value, json!([{"city": "Roma", "pop": "2873000"}]));
    }

    #[test]
    fn parses_xml_repeated_elements_and_attributes() {
        let value = parse_xml(b"<root id=\"1\"><item>A</item><item>B</item></root>").unwrap();
        assert_eq!(value, json!({"root": {"@id": "1", "item": ["A", "B"]}}));
    }

    #[test]
    fn xml_text_keeps_the_spaces_around_references() {
        // Found by the property test of the XML writer: each piece of text
        // between references was trimmed on its own.
        assert_eq!(
            parse_xml(b"<a>Fish &amp; Chips</a>").unwrap(),
            json!({"a": "Fish & Chips"})
        );
        assert_eq!(
            parse_xml(b"<a> 1 &#x3C; 2 </a>").unwrap(),
            json!({"a": "1 < 2"})
        );
        // Mixed content keeps the text as written, trimmed only at the ends.
        assert_eq!(
            parse_xml(b"<a id=\"1\"> x <b/> y </a>").unwrap(),
            json!({"a": {"@id": "1", "b": "", "#text": "x  y"}})
        );
        assert_eq!(
            parse_xml(b"<a>\n  <b>1</b>\n  <b>2</b>\n</a>").unwrap(),
            json!({"a": {"b": ["1", "2"]}})
        );
    }

    #[test]
    fn parses_ndjson_and_binary_without_losing_bytes() {
        let ndjson = ResponseConfig {
            format: ResponseFormat::Ndjson,
            ..ResponseConfig::default()
        };
        assert_eq!(
            parse(b"{\"id\":1}\n\n{\"id\":2}\n", &ndjson).unwrap(),
            json!([{"id": 1}, {"id": 2}])
        );

        let binary = ResponseConfig {
            format: ResponseFormat::Binary,
            ..ResponseConfig::default()
        };
        assert_eq!(
            parse(&[0, 255, 1], &binary).unwrap(),
            json!({"data_base64": "AP8B", "size": 3})
        );
    }
}
