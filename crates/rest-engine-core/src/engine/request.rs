//! Encode an admitted connection and parameters into a prepared request.

use super::{
    BTreeSet, BodyType, ConnectionConfig, Duration, Engine, EngineError, ErrorDetail, HttpMethod,
    JsonObject, PATH_SEGMENT_ENCODE_SET, ParameterLocation, PreparedBody, PreparedFile,
    PreparedFileSource, PreparedRequest, QuerySerialization, QueryStyle, STANDARD, Url, Value,
    apply_idempotency, insert_header, utf8_percent_encode, value_as_text,
};
use base64::Engine as _;
impl Engine {
    pub(super) fn prepare_request(
        &self,
        connection: &ConnectionConfig,
        parameters: &JsonObject,
        url_override: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> Result<PreparedRequest, EngineError> {
        let template = url_override.unwrap_or(&connection.url);
        #[cfg(test)]
        crate::injection::visit(crate::injection::Point::Prepare);
        if template.trim().is_empty() {
            return Err(EngineError::InvalidUrl(ErrorDetail::from("URL is empty")));
        }
        let (rendered_url, consumed) = render_template(template, parameters, true);
        let mut url = Url::parse(&rendered_url)
            .map_err(|_| EngineError::InvalidUrl(ErrorDetail::from("rendered URL is not valid")))?;
        let legacy_query_only = matches!(
            &connection.method,
            HttpMethod::Get | HttpMethod::Head | HttpMethod::Delete | HttpMethod::Options
        ) || connection.request.body_type == BodyType::None;
        let mut query_parameters = JsonObject::new();
        let mut body_parameters = JsonObject::new();
        let mut headers = connection.headers.clone();
        let mut cookies = Vec::new();
        for (name, value) in parameters {
            let spec = connection.parameters.iter().find(|spec| spec.name == *name);
            let location = spec.map_or(ParameterLocation::Auto, |spec| spec.location);
            if consumed.contains(name) {
                if !matches!(location, ParameterLocation::Auto | ParameterLocation::Path) {
                    return Err(EngineError::InvalidInput(ErrorDetail::from(
                        "parameter is used in the URL but has a non-path location",
                    )));
                }
                refuse_null_in_text(value)?;
                continue;
            }
            match location {
                ParameterLocation::Path => {
                    return Err(EngineError::InvalidInput(ErrorDetail::from(
                        "path parameter has no matching URL placeholder",
                    )));
                }
                ParameterLocation::Query => {
                    refuse_null_in_text(value)?;
                    query_parameters.insert(name.clone(), value.clone());
                }
                ParameterLocation::Header => {
                    refuse_null_in_text(value)?;
                    insert_header(&mut headers, name, parameter_header_value(value));
                }
                ParameterLocation::Body => {
                    body_parameters.insert(name.clone(), value.clone());
                }
                ParameterLocation::Cookie => {
                    refuse_null_in_text(value)?;
                    validate_cookie_name(name)?;
                    cookies.push((name.clone(), cookie_value(value)?));
                }
                ParameterLocation::Auto if legacy_query_only => {
                    refuse_null_in_text(value)?;
                    query_parameters.insert(name.clone(), value.clone());
                }
                ParameterLocation::Auto => {
                    body_parameters.insert(name.clone(), value.clone());
                }
            }
        }
        if let Some(key) = idempotency_key {
            apply_idempotency(
                connection,
                key,
                &mut query_parameters,
                &mut body_parameters,
                &mut headers,
            )?;
        }
        append_query(&mut url, &query_parameters, &connection.parameters)?;
        append_cookies(&mut headers, &cookies);

        let build_body = !body_parameters.is_empty()
            || !legacy_query_only
            || (connection.request.body_type == BodyType::Raw
                && connection.request.raw_body.is_some());
        let body = if !build_body {
            PreparedBody::None
        } else {
            match connection.request.body_type {
                BodyType::Json => PreparedBody::Json(Value::Object(body_parameters)),
                BodyType::FormUrlencoded => {
                    for value in body_parameters.values() {
                        refuse_null_in_text(value)?;
                    }
                    PreparedBody::Form(
                        body_parameters
                            .iter()
                            .map(|(key, value)| (key.clone(), value_as_text(value)))
                            .collect(),
                    )
                }
                BodyType::Multipart => {
                    for value in body_parameters.values() {
                        refuse_null_in_text(value)?;
                    }
                    multipart_body(&body_parameters, self.config.max_request_bytes)?
                }
                BodyType::Raw => {
                    let raw = connection.request.raw_body.as_deref().unwrap_or_default();
                    let (rendered, consumed) = render_template(raw, parameters, false);
                    for name in &consumed {
                        if let Some(value) = parameters.get(name) {
                            refuse_null_in_text(value)?;
                        }
                    }
                    ensure_request_size(rendered.len(), self.config.max_request_bytes)?;
                    PreparedBody::Raw(rendered)
                }
                BodyType::None => PreparedBody::None,
            }
        };
        match &body {
            PreparedBody::Json(value) => {
                let size = serde_json::to_vec(value)
                    .map_err(|_| {
                        EngineError::InvalidInput(ErrorDetail::from(
                            "JSON body could not be serialized",
                        ))
                    })?
                    .len();
                ensure_request_size(size, self.config.max_request_bytes)?;
            }
            PreparedBody::Form(values) => {
                let mut serializer = url::form_urlencoded::Serializer::new(String::new());
                for (name, value) in values {
                    serializer.append_pair(name, value);
                }
                ensure_request_size(serializer.finish().len(), self.config.max_request_bytes)?;
            }
            PreparedBody::None
            | PreparedBody::Raw(_)
            | PreparedBody::Multipart { .. }
            | PreparedBody::Stream(_) => {}
        }

        let mut retry = connection.retry.clone();
        if idempotency_key.is_some() {
            retry.retry_non_idempotent = true;
        }
        Ok(PreparedRequest {
            url,
            method: connection.method.clone(),
            headers,
            auth: connection.auth.clone(),
            body,
            timeout: Duration::from_millis(
                connection
                    .request
                    .timeout_ms
                    .unwrap_or(self.config.request_timeout_ms),
            ),
            allow_redirects: connection.request.allow_redirects,
            max_redirects: connection.request.max_redirects,
            retry,
            cookies: connection.cookies.clone(),
            admitted_jar: None,
            caller_session: connection.cookies.session.clone(),
            cache: connection.cache.clone(),
            circuit_breaker: connection.circuit_breaker.clone(),
            requests_per_second: connection.requests_per_second,
            tls: connection.tls.clone(),
            proxy: connection.proxy.clone(),
        })
    }
}

pub(super) fn render_template(
    template: &str,
    parameters: &JsonObject,
    url_encode: bool,
) -> (String, BTreeSet<String>) {
    let mut rendered = template.to_owned();
    let mut consumed = BTreeSet::new();
    for (key, value) in parameters {
        let placeholder = format!("{{{key}}}");
        if rendered.contains(&placeholder) {
            let text = value_as_text(value);
            let replacement = if url_encode {
                utf8_percent_encode(&text, PATH_SEGMENT_ENCODE_SET).to_string()
            } else {
                text
            };
            rendered = rendered.replace(&placeholder, &replacement);
            consumed.insert(key.clone());
        }
    }
    (rendered, consumed)
}

fn append_query(
    url: &mut Url,
    parameters: &JsonObject,
    specs: &[crate::ParameterSpec],
) -> Result<(), EngineError> {
    if parameters.is_empty() {
        return Ok(());
    }
    let mut pairs = Vec::new();
    for (name, value) in parameters {
        let serialization = specs
            .iter()
            .find(|spec| spec.name == *name)
            .and_then(|spec| spec.query_serialization.as_ref());
        serialize_query_parameter(&mut pairs, name, value, serialization)?;
    }
    let mut query = url.query_pairs_mut();
    for (name, value) in pairs {
        query.append_pair(&name, &value);
    }
    Ok(())
}

fn serialize_query_parameter(
    pairs: &mut Vec<(String, String)>,
    name: &str,
    value: &Value,
    serialization: Option<&QuerySerialization>,
) -> Result<(), EngineError> {
    let Some(serialization) = serialization else {
        match value {
            Value::Array(values) => {
                pairs.extend(
                    values
                        .iter()
                        .map(|value| (name.to_owned(), value_as_text(value))),
                );
            }
            value => pairs.push((name.to_owned(), value_as_text(value))),
        }
        return Ok(());
    };

    let explode = serialization
        .explode
        .unwrap_or(serialization.style == QueryStyle::Form);
    match (serialization.style, value) {
        (QueryStyle::Form, Value::Array(values)) if explode => {
            pairs.extend(
                values
                    .iter()
                    .map(|value| (name.to_owned(), value_as_text(value))),
            );
        }
        (QueryStyle::Form, Value::Array(values)) => {
            pairs.push((name.to_owned(), join_values(values, ",")));
        }
        (QueryStyle::Form, Value::Object(values)) if explode => {
            pairs.extend(
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), value_as_text(value))),
            );
        }
        (QueryStyle::Form, Value::Object(values)) => {
            pairs.push((name.to_owned(), flatten_object(values, ",")));
        }
        (QueryStyle::SpaceDelimited, Value::Array(values)) => {
            pairs.push((name.to_owned(), join_values(values, " ")));
        }
        (QueryStyle::SpaceDelimited, Value::Object(values)) => {
            pairs.push((name.to_owned(), flatten_object(values, " ")));
        }
        (QueryStyle::PipeDelimited, Value::Array(values)) => {
            pairs.push((name.to_owned(), join_values(values, "|")));
        }
        (QueryStyle::PipeDelimited, Value::Object(values)) => {
            pairs.push((name.to_owned(), flatten_object(values, "|")));
        }
        (QueryStyle::DeepObject, Value::Object(values)) => {
            pairs.extend(
                values
                    .iter()
                    .map(|(key, value)| (format!("{name}[{key}]"), value_as_text(value))),
            );
        }
        (QueryStyle::DeepObject, _) => {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "deep_object query parameter must be an object",
            )));
        }
        (_, value) => pairs.push((name.to_owned(), value_as_text(value))),
    }
    Ok(())
}

fn join_values(values: &[Value], separator: &str) -> String {
    values
        .iter()
        .map(value_as_text)
        .collect::<Vec<_>>()
        .join(separator)
}

fn flatten_object(values: &JsonObject, separator: &str) -> String {
    values
        .iter()
        .flat_map(|(key, value)| [key.clone(), value_as_text(value)])
        .collect::<Vec<_>>()
        .join(separator)
}

fn parameter_header_value(value: &Value) -> String {
    match value {
        Value::Array(values) => join_values(values, ","),
        value => value_as_text(value),
    }
}

fn cookie_value(value: &Value) -> Result<String, EngineError> {
    let value = parameter_header_value(value);
    if value
        .bytes()
        .any(|byte| byte <= 0x20 || byte >= 0x7f || matches!(byte, b';' | b','))
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "cookie parameter contains unsupported characters",
        )));
    }
    Ok(value)
}

fn validate_cookie_name(name: &str) -> Result<(), EngineError> {
    let valid = !name.is_empty()
        && name.bytes().all(|byte| {
            (0x21..=0x7e).contains(&byte)
                && !matches!(
                    byte,
                    b'(' | b')'
                        | b'<'
                        | b'>'
                        | b'@'
                        | b','
                        | b';'
                        | b':'
                        | b'\\'
                        | b'"'
                        | b'/'
                        | b'['
                        | b']'
                        | b'?'
                        | b'='
                        | b'{'
                        | b'}'
                )
        });
    if valid {
        Ok(())
    } else {
        Err(EngineError::InvalidInput(ErrorDetail::from(
            "invalid cookie parameter name",
        )))
    }
}

fn append_cookies(
    headers: &mut std::collections::BTreeMap<String, String>,
    cookies: &[(String, String)],
) {
    if cookies.is_empty() {
        return;
    }
    let value = cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    let existing = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
        .map(|(name, value)| (name.clone(), value.clone()));
    if let Some((name, existing)) = existing {
        headers.insert(name, format!("{existing}; {value}"));
    } else {
        headers.insert("Cookie".to_owned(), value);
    }
}

fn multipart_body(
    parameters: &JsonObject,
    max_request_bytes: usize,
) -> Result<PreparedBody, EngineError> {
    let mut fields = Vec::new();
    let mut files = Vec::new();
    let mut estimated_size = 0_usize;

    for (name, value) in parameters {
        if let Some(file) = value
            .as_object()
            .filter(|object| object.contains_key("data_base64"))
        {
            let encoded = file
                .get("data_base64")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    EngineError::InvalidInput(ErrorDetail::from(
                        "multipart file has invalid data_base64",
                    ))
                })?;
            let data = STANDARD.decode(encoded).map_err(|_| {
                EngineError::InvalidInput(ErrorDetail::from("multipart file is not valid base64"))
            })?;
            let filename = file
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or(name)
                .to_owned();
            let content_type = file
                .get("content_type")
                .map(|value| {
                    value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                        EngineError::InvalidInput(ErrorDetail::from(
                            "multipart file has invalid content_type",
                        ))
                    })
                })
                .transpose()?;
            estimated_size = estimated_size
                .saturating_add(data.len())
                .saturating_add(name.len())
                .saturating_add(filename.len())
                .saturating_add(1_024);
            ensure_request_size(estimated_size, max_request_bytes)?;
            files.push(PreparedFile {
                field_name: name.clone(),
                filename,
                content_type,
                source: PreparedFileSource::Bytes(data),
            });
        } else {
            let text = value_as_text(value);
            estimated_size = estimated_size
                .saturating_add(name.len())
                .saturating_add(text.len())
                .saturating_add(256);
            ensure_request_size(estimated_size, max_request_bytes)?;
            fields.push((name.clone(), text));
        }
    }

    Ok(PreparedBody::Multipart { fields, files })
}

fn ensure_request_size(size: usize, limit: usize) -> Result<(), EngineError> {
    if size > limit {
        Err(EngineError::RequestTooLarge { limit_bytes: limit })
    } else {
        Ok(())
    }
}

/// Refuses a `null` (alone, in an array, or as an object member) in a
/// parameter that is rendered as text: URL path, query, header, cookie, form
/// field, multipart field, or raw body template.
///
/// Text has no spelling for `null`. Rendering it as an empty string would send
/// a value the caller never gave, and dropping it would make it absent, so the
/// request is rejected before any network activity. A JSON body keeps `null`.
fn refuse_null_in_text(value: &Value) -> Result<(), EngineError> {
    fn contains_null(value: &Value) -> bool {
        match value {
            Value::Null => true,
            Value::Array(values) => values.iter().any(contains_null),
            Value::Object(values) => values.values().any(contains_null),
            Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
        }
    }
    if contains_null(value) {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "parameter is null in a location rendered as text",
        )));
    }
    Ok(())
}
