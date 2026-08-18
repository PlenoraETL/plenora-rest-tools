use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use quick_xml::{Reader, events::Event};
use serde_json::{Map, Value, json};

use crate::{EngineError, ResponseConfig, ResponseFormat};

pub(crate) fn parse(body: &[u8], config: &ResponseConfig) -> Result<Value, EngineError> {
    match config.format {
        ResponseFormat::Json => serde_json::from_slice(body).map_err(|error| {
            EngineError::InvalidResponse(format!("response body is not valid JSON: {error}"))
        }),
        ResponseFormat::Csv => parse_csv(body, &config.delimiter),
        ResponseFormat::Xml => parse_xml(body),
        ResponseFormat::Ndjson => parse_ndjson(body),
        ResponseFormat::Text => String::from_utf8(body.to_vec())
            .map(Value::String)
            .map_err(|_| {
                EngineError::InvalidResponse("response body is not valid UTF-8".to_owned())
            }),
        ResponseFormat::Binary => Ok(json!({
            "data_base64": STANDARD.encode(body),
            "size": body.len(),
        })),
    }
}

fn parse_ndjson(body: &[u8]) -> Result<Value, EngineError> {
    let text = std::str::from_utf8(body).map_err(|_| {
        EngineError::InvalidResponse("NDJSON response is not valid UTF-8".to_owned())
    })?;
    let mut values = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value = serde_json::from_str(line).map_err(|error| {
            EngineError::InvalidResponse(format!(
                "NDJSON line {} is not valid JSON: {error}",
                index + 1
            ))
        })?;
        values.push(value);
    }
    Ok(Value::Array(values))
}

fn parse_csv(body: &[u8], delimiter: &str) -> Result<Value, EngineError> {
    let delimiter = delimiter.as_bytes();
    if delimiter.len() != 1 {
        return Err(EngineError::InvalidInput(
            "CSV delimiter must be one ASCII byte".to_owned(),
        ));
    }
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter[0])
        .from_reader(body);
    let headers = reader
        .headers()
        .map_err(|error| EngineError::InvalidResponse(format!("invalid CSV header: {error}")))?
        .clone();
    if headers.iter().any(str::is_empty)
        || headers.iter().collect::<BTreeSet<_>>().len() != headers.len()
    {
        return Err(EngineError::InvalidResponse(
            "CSV response has missing or duplicate headers".to_owned(),
        ));
    }
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record
            .map_err(|error| EngineError::InvalidResponse(format!("invalid CSV row: {error}")))?;
        if record.len() != headers.len() {
            return Err(EngineError::InvalidResponse(
                "CSV row has a different width than its header".to_owned(),
            ));
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
    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<XmlNode> = Vec::new();
    let mut root: Option<(String, Value)> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                if stack.len() >= 128 {
                    return Err(EngineError::InvalidResponse(
                        "XML nesting exceeds the supported limit".to_owned(),
                    ));
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
                        EngineError::InvalidResponse("XML contains invalid text".to_owned())
                    })?;
                    node.text.push_str(&text);
                }
            }
            Ok(Event::CData(event)) => {
                if let Some(node) = stack.last_mut() {
                    let text = event.decode().map_err(|_| {
                        EngineError::InvalidResponse("XML contains invalid CDATA".to_owned())
                    })?;
                    node.text.push_str(&text);
                }
            }
            Ok(Event::End(event)) => {
                let node = stack.pop().ok_or_else(|| {
                    EngineError::InvalidResponse("XML has an unexpected closing tag".to_owned())
                })?;
                if xml_name(event.name().as_ref()) != node.name {
                    return Err(EngineError::InvalidResponse(
                        "XML closing tag does not match".to_owned(),
                    ));
                }
                attach_xml_node(&mut stack, &mut root, node)?;
            }
            Ok(Event::DocType(_) | Event::GeneralRef(_)) => {
                return Err(EngineError::InvalidResponse(
                    "XML DTDs and entity references are not allowed".to_owned(),
                ));
            }
            Ok(Event::Eof) => break,
            Ok(Event::Decl(_) | Event::PI(_) | Event::Comment(_)) => {}
            Err(_) => {
                return Err(EngineError::InvalidResponse(
                    "response body is not valid XML".to_owned(),
                ));
            }
        }
    }
    if !stack.is_empty() {
        return Err(EngineError::InvalidResponse(
            "XML contains unclosed elements".to_owned(),
        ));
    }
    let (name, value) =
        root.ok_or_else(|| EngineError::InvalidResponse("XML response is empty".to_owned()))?;
    Ok(Value::Object(Map::from_iter([(name, value)])))
}

fn xml_node(
    reader: &Reader<&[u8]>,
    event: &quick_xml::events::BytesStart<'_>,
) -> Result<XmlNode, EngineError> {
    let mut content = Map::new();
    for attribute in event.attributes().with_checks(true) {
        let attribute = attribute.map_err(|_| {
            EngineError::InvalidResponse("XML contains an invalid attribute".to_owned())
        })?;
        let key = format!("@{}", xml_name(attribute.key.as_ref()));
        let value = attribute
            .decode_and_unescape_value(reader.decoder())
            .map_err(|_| {
                EngineError::InvalidResponse("XML contains an invalid attribute value".to_owned())
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
        Err(EngineError::InvalidResponse(
            "XML response contains multiple root elements".to_owned(),
        ))
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
