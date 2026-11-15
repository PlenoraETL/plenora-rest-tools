//! Page traversal and continuation-header grammar.

use super::{
    BTreeMap, ConnectionConfig, Cow, CredentialScope, Engine, EngineError, ErrorDetail,
    ExecutionMetrics, HashSet, HttpResponseMetadata, JsonObject, PaginationConfig, Url, Value,
    json_path, response_records, same_origin, scoped_execution_options, usize_value,
    value_as_string,
};
impl Engine {
    pub(super) async fn paginated(
        &self,
        connection: &ConnectionConfig,
        base_parameters: &JsonObject,
        pagination: &PaginationConfig,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
        options: &crate::ExecutionOptions,
    ) -> Result<(Vec<Value>, Option<EngineError>), EngineError> {
        let mut output = Vec::new();
        // Set when the source itself says it has no more data: a short page,
        // no next cursor or link, or one already followed. Leaving a loop for
        // any other reason (max_rows, max_pages), or dropping rows of the last
        // page to fit max_rows, means the source still had data.
        let mut finished = false;
        let mut dropped = false;
        // Origin that owns the credentials for the whole pagination run. It is
        // fixed by the first page and never re-derived, so no later page can
        // become the origin that owns them. Cursor, offset, and page values are
        // remote input too: `prepare_request` substitutes placeholders anywhere
        // in the URL, including the host, so every mode is scoped, not only the
        // ones that follow an explicit link.
        let mut credential_scope: Option<CredentialScope> = None;

        match pagination {
            PaginationConfig::Offset {
                offset_param,
                limit_param,
                page_size,
                max_rows,
                start,
            } => {
                ensure_page_size(*page_size)?;
                let mut offset = *start;
                let mut request_index = 0_usize;
                while output.len() < *max_rows {
                    let limit = (*page_size).min(max_rows.saturating_sub(output.len()));
                    let mut parameters = base_parameters.clone();
                    parameters.insert(offset_param.clone(), usize_value(offset)?);
                    parameters.insert(limit_param.clone(), usize_value(limit)?);
                    let scoped_options = scoped_execution_options(options, request_index);
                    let (value, _, _, _) = self
                        .request_page(
                            connection,
                            &parameters,
                            None,
                            &mut credential_scope,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    let page = response_records(connection, &value)?;
                    let page_len = page.len();
                    dropped |= append_limited(&mut output, page, *max_rows);
                    if page_len < limit {
                        finished = true;
                        break;
                    }
                    offset = offset.saturating_add(*page_size);
                    request_index = request_index.saturating_add(1);
                }
            }
            PaginationConfig::Page {
                page_param,
                page_size_param,
                page_size,
                max_rows,
                start_page,
            } => {
                ensure_page_size(*page_size)?;
                let mut page_number = *start_page;
                let mut request_index = 0_usize;
                while output.len() < *max_rows {
                    // The page size stays the same on every request: page N
                    // of a smaller size is a different slice of the data
                    // (page 2 of size 1 is the second row, not the third).
                    // Rows beyond max_rows are cut locally instead.
                    let limit = *page_size;
                    let mut parameters = base_parameters.clone();
                    parameters.insert(page_param.clone(), usize_value(page_number)?);
                    parameters.insert(page_size_param.clone(), usize_value(limit)?);
                    let scoped_options = scoped_execution_options(options, request_index);
                    let (value, _, _, _) = self
                        .request_page(
                            connection,
                            &parameters,
                            None,
                            &mut credential_scope,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    let page = response_records(connection, &value)?;
                    let page_len = page.len();
                    dropped |= append_limited(&mut output, page, *max_rows);
                    if page_len < limit {
                        finished = true;
                        break;
                    }
                    page_number = page_number.saturating_add(1);
                    request_index = request_index.saturating_add(1);
                }
            }
            PaginationConfig::Cursor {
                cursor_param,
                cursor_path,
                max_rows,
                max_pages,
            } => {
                let mut cursor: Option<String> = None;
                let mut seen = HashSet::new();
                for page_index in 0..*max_pages {
                    if output.len() >= *max_rows {
                        break;
                    }
                    let mut parameters = base_parameters.clone();
                    if let Some(cursor) = &cursor {
                        parameters.insert(cursor_param.clone(), Value::String(cursor.clone()));
                    }
                    let scoped_options = scoped_execution_options(options, page_index);
                    let (value, _, _, _) = self
                        .request_page(
                            connection,
                            &parameters,
                            None,
                            &mut credential_scope,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    dropped |= append_limited(
                        &mut output,
                        response_records(connection, &value)?,
                        *max_rows,
                    );
                    cursor = json_path::get(&value, cursor_path).and_then(value_as_string);
                    match &cursor {
                        Some(value) if seen.insert(value.clone()) => {}
                        _ => {
                            finished = true;
                            break;
                        }
                    }
                }
            }
            PaginationConfig::Link {
                link_path,
                max_rows,
                max_pages,
                allow_cross_origin,
            } => {
                let mut next_url: Option<String> = None;
                let mut seen = HashSet::new();
                for page_index in 0..*max_pages {
                    if output.len() >= *max_rows {
                        break;
                    }
                    let empty = JsonObject::new();
                    let parameters = if page_index == 0 {
                        base_parameters
                    } else {
                        &empty
                    };
                    let scoped_options = scoped_execution_options(options, page_index);
                    let (value, final_url, _, _) = self
                        .request_page(
                            connection,
                            parameters,
                            next_url.as_deref(),
                            &mut credential_scope,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    dropped |= append_limited(
                        &mut output,
                        response_records(connection, &value)?,
                        *max_rows,
                    );
                    let Some(link) = json_path::get(&value, link_path).and_then(Value::as_str)
                    else {
                        finished = true;
                        break;
                    };
                    let resolved =
                        pagination_url(&final_url, link, *allow_cross_origin)?.to_string();
                    if !seen.insert(resolved.clone()) {
                        finished = true;
                        break;
                    }
                    next_url = Some(resolved);
                }
            }
            PaginationConfig::HeaderLink {
                relation,
                max_rows,
                max_pages,
                allow_cross_origin,
            } => {
                if relation.trim().is_empty() {
                    return Err(EngineError::InvalidInput(ErrorDetail::from(
                        "pagination Link relation cannot be empty",
                    )));
                }
                let mut next_url: Option<String> = None;
                let mut seen = HashSet::new();
                for page_index in 0..*max_pages {
                    if output.len() >= *max_rows {
                        break;
                    }
                    let empty = JsonObject::new();
                    let parameters = if page_index == 0 {
                        base_parameters
                    } else {
                        &empty
                    };
                    let scoped_options = scoped_execution_options(options, page_index);
                    let (value, final_url, _, headers) = self
                        .request_page(
                            connection,
                            parameters,
                            next_url.as_deref(),
                            &mut credential_scope,
                            metrics,
                            responses,
                            &scoped_options,
                        )
                        .await?;
                    dropped |= append_limited(
                        &mut output,
                        response_records(connection, &value)?,
                        *max_rows,
                    );
                    let Some(link) = link_header_target(&headers, relation)? else {
                        finished = true;
                        break;
                    };
                    let resolved =
                        pagination_url(&final_url, &link, *allow_cross_origin)?.to_string();
                    if !seen.insert(resolved.clone()) {
                        finished = true;
                        break;
                    }
                    next_url = Some(resolved);
                }
            }
        }

        let stopped = (dropped || !finished).then(|| pagination_limit(pagination));
        Ok((output, stopped))
    }
}

fn ensure_page_size(page_size: usize) -> Result<(), EngineError> {
    if page_size == 0 {
        Err(EngineError::InvalidInput(ErrorDetail::from(
            "pagination page_size must be greater than zero",
        )))
    } else {
        Ok(())
    }
}

/// Appends at most `max_rows - output.len()` values; true when some were left
/// out, which means the source had more rows than the limit admits.
fn append_limited(output: &mut Vec<Value>, values: Vec<Value>, max_rows: usize) -> bool {
    let remaining = max_rows.saturating_sub(output.len());
    let available = values.len();
    output.extend(values.into_iter().take(remaining));
    available > remaining
}

fn pagination_limit(pagination: &PaginationConfig) -> EngineError {
    match pagination {
        PaginationConfig::Offset { max_rows, .. } | PaginationConfig::Page { max_rows, .. } => {
            EngineError::PaginationLimit {
                max_rows: *max_rows,
                max_pages: None,
            }
        }
        PaginationConfig::Cursor {
            max_rows,
            max_pages,
            ..
        }
        | PaginationConfig::Link {
            max_rows,
            max_pages,
            ..
        }
        | PaginationConfig::HeaderLink {
            max_rows,
            max_pages,
            ..
        } => EngineError::PaginationLimit {
            max_rows: *max_rows,
            max_pages: Some(*max_pages),
        },
    }
}

fn pagination_url(base: &Url, target: &str, allow_cross_origin: bool) -> Result<Url, EngineError> {
    let resolved = base
        .join(target)
        .map_err(|_| EngineError::InvalidResponse(ErrorDetail::from("invalid pagination link")))?;
    if !allow_cross_origin && !same_origin(base, &resolved) {
        return Err(EngineError::UnsafeAddress(ErrorDetail::from(
            "cross-origin pagination is blocked",
        )));
    }
    Ok(resolved)
}

pub(crate) fn link_header_target(
    headers: &BTreeMap<String, String>,
    expected_relation: &str,
) -> Result<Option<String>, EngineError> {
    let Some(header) = headers.get("link") else {
        return Ok(None);
    };
    for entry in split_link_header(header)? {
        let entry = entry.trim();
        let Some(target_end) = entry.strip_prefix('<').and_then(|value| value.find('>')) else {
            return Err(EngineError::InvalidResponse(ErrorDetail::from(
                "Link header contains an invalid target",
            )));
        };
        let target = &entry[1..=target_end];
        let parameters = &entry[target_end + 2..];
        for parameter in split_quoted(parameters, ';')? {
            let Some((name, value)) = parameter.split_once('=') else {
                continue;
            };
            if !name.trim().eq_ignore_ascii_case("rel") {
                continue;
            }
            let value = unquote_header_value(value.trim())?;
            if value
                .split_ascii_whitespace()
                .any(|relation| relation_matches(relation, expected_relation))
            {
                return Ok(Some(target.to_owned()));
            }
            // RFC 8288, 3.3: occurrences of rel after the first in a link
            // value are ignored, so `rel=prev; rel=next` is not a next link.
            break;
        }
    }
    Ok(None)
}

fn split_link_header(value: &str) -> Result<Vec<&str>, EngineError> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut in_target = false;
    let mut in_quotes = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_quotes && character == '\\' {
            escaped = true;
            continue;
        }
        match character {
            '"' if !in_target => in_quotes = !in_quotes,
            '<' if !in_quotes => in_target = true,
            '>' if !in_quotes => in_target = false,
            ',' if !in_target && !in_quotes => {
                entries.push(&value[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if in_target || in_quotes || escaped {
        return Err(EngineError::InvalidResponse(ErrorDetail::from(
            "Link header is not well formed",
        )));
    }
    entries.push(&value[start..]);
    Ok(entries)
}

fn split_quoted(value: &str, separator: char) -> Result<Vec<&str>, EngineError> {
    let mut values = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_quotes && character == '\\' {
            escaped = true;
        } else if character == '"' {
            in_quotes = !in_quotes;
        } else if character == separator && !in_quotes {
            values.push(&value[start..index]);
            start = index + character.len_utf8();
        }
    }
    if in_quotes || escaped {
        return Err(EngineError::InvalidResponse(ErrorDetail::from(
            "Link header parameter is not well formed",
        )));
    }
    values.push(&value[start..]);
    Ok(values)
}

/// The value of a Link parameter: a token as written, or a quoted-string with
/// its quoted-pairs resolved (RFC 9110, 5.6.4). Comparing the escaped text
/// would make `rel="n\ext"` miss `next` and end the pagination silently.
fn unquote_header_value(value: &str) -> Result<Cow<'_, str>, EngineError> {
    let invalid = || {
        EngineError::InvalidResponse(ErrorDetail::from(
            "Link header contains an invalid quoted value",
        ))
    };
    match (value.strip_prefix('"'), value.strip_suffix('"')) {
        (Some(inner), Some(_)) if inner.ends_with('"') => {
            let inner = &inner[..inner.len() - 1];
            if !inner.contains('\\') {
                return Ok(Cow::Borrowed(inner));
            }
            let mut unescaped = String::with_capacity(inner.len());
            let mut characters = inner.chars();
            while let Some(character) = characters.next() {
                if character == '\\' {
                    unescaped.push(characters.next().ok_or_else(invalid)?);
                } else {
                    unescaped.push(character);
                }
            }
            Ok(Cow::Owned(unescaped))
        }
        (None, None) => Ok(Cow::Borrowed(value)),
        _ => Err(invalid()),
    }
}

fn relation_matches(actual: &str, expected: &str) -> bool {
    if expected.contains(':') {
        actual == expected
    } else {
        actual.eq_ignore_ascii_case(expected)
    }
}
