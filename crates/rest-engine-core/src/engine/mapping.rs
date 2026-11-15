//! Parameter resolution and response-to-record transformations.

use super::{
    BatchConfig, BatchInputFormat, ConnectionConfig, EngineError, ErrorDetail, JsonObject, Map,
    Number, OutputMapping, ParameterMode, ResponseConfig, ResponseTransform, Value, json_path,
};

pub(super) fn resolve_parameters(
    connection: &ConnectionConfig,
    source: &JsonObject,
) -> Result<JsonObject, EngineError> {
    let mut parameters = connection.static_parameters.clone();
    parameters.extend(source.clone());
    let source_value = Value::Object(source.clone());

    for parameter in &connection.parameters {
        let value = match parameter.mode {
            // A fixed parameter without a value has nothing to send: dropping
            // it would make a configuration mistake indistinguishable from an
            // optional parameter that is legitimately absent.
            ParameterMode::Fixed if parameter.value.is_none() => {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "fixed parameter has no value",
                )));
            }
            ParameterMode::Fixed => parameter.value.clone(),
            ParameterMode::Mapped => {
                let source_path = parameter.source.as_deref().unwrap_or(&parameter.name);
                source
                    .get(source_path)
                    .or_else(|| json_path::get(&source_value, source_path))
                    .cloned()
                    .or_else(|| parameter.value.clone())
            }
        };
        match value {
            Some(value) => {
                parameters.insert(parameter.name.clone(), value);
            }
            None if parameter.required => {
                return Err(EngineError::MissingParameter(ErrorDetail::from(
                    "a required parameter has no value",
                )));
            }
            None => {}
        }
    }
    Ok(parameters)
}

pub(super) fn response_records(
    connection: &ConnectionConfig,
    response: &Value,
) -> Result<Vec<Value>, EngineError> {
    let selected = match &connection.response.records_path {
        Some(path) => json_path::get(response, path).ok_or_else(|| {
            EngineError::InvalidResponse(ErrorDetail::from("records path was not found"))
        })?,
        None => response,
    };
    if connection.response.iterate_on.is_empty() {
        return match selected {
            Value::Array(values) => Ok(values.clone()),
            Value::Object(_) => Ok(vec![selected.clone()]),
            Value::Null => Ok(Vec::new()),
            _ => Err(EngineError::InvalidResponse(ErrorDetail::from(
                "records value must be an array, object, or null",
            ))),
        };
    }
    let mut output = Vec::new();
    expand_iterations(
        selected,
        &connection.response.iterate_on,
        0,
        &Map::new(),
        &mut output,
    );
    Ok(output)
}

pub(super) fn batch_payload(batch: &BatchConfig, parameters: Vec<JsonObject>) -> JsonObject {
    let values = match batch.input_format {
        // Every record was checked by `flat_batch_item`, so each one contributes
        // exactly one element and the array stays aligned with the input.
        BatchInputFormat::FlatArray => Value::Array(
            parameters
                .into_iter()
                .map(|parameters| parameters.into_values().next().unwrap_or(Value::Null))
                .collect(),
        ),
        BatchInputFormat::Array | BatchInputFormat::Object => {
            Value::Array(parameters.into_iter().map(Value::Object).collect())
        }
    };
    Map::from_iter([(batch.input_key.clone(), values)])
}

/// The single value a record contributes to a `flat_array` batch.
///
/// A flat array has one element per record and nothing else to align results
/// with. A record that resolves to several parameters, or to a `null`, used to
/// contribute its first non-null value in key order, or nothing at all, which
/// silently picked a field or shifted every later record. Both are refused.
pub(super) fn flat_batch_item(parameters: &JsonObject) -> Result<&Value, EngineError> {
    let mut values = parameters.values();
    match (values.next(), values.next()) {
        (Some(value), None) if !value.is_null() => Ok(value),
        _ => Err(EngineError::InvalidInput(ErrorDetail::from(
            "a flat_array batch record must resolve to exactly one non-null parameter",
        ))),
    }
}

pub(super) fn batch_response(
    response: &Value,
    batch: &BatchConfig,
    config: &ResponseConfig,
) -> Result<Vec<JsonObject>, EngineError> {
    let selected = if batch.output_path.is_empty() {
        response
    } else {
        json_path::get(response, &batch.output_path).ok_or_else(|| {
            EngineError::InvalidResponse(ErrorDetail::from("batch output path was not found"))
        })?
    };
    let values = selected.as_array().ok_or_else(|| {
        EngineError::InvalidResponse(ErrorDetail::from("batch output must be an array"))
    })?;
    values
        .iter()
        .map(|value| {
            if value.is_null() {
                return Ok(JsonObject::new());
            }
            let mut row: JsonObject = if config.output_mapping.is_empty() {
                value
                    .as_object()
                    .cloned()
                    .unwrap_or_else(|| Map::from_iter([("value".to_owned(), value.clone())]))
            } else {
                config
                    .output_mapping
                    .iter()
                    .map(|mapping| {
                        let path = mapping.path.strip_prefix("[0].").unwrap_or(&mapping.path);
                        let value = json_path::get(value, path)
                            .cloned()
                            .or_else(|| mapping.default.clone())
                            .unwrap_or(Value::Null);
                        (mapping.column.clone(), value)
                    })
                    .collect()
            };
            apply_transforms(&mut row, &config.transforms)?;
            Ok(row)
        })
        .collect()
}

fn expand_iterations(
    current: &Value,
    iterations: &[crate::IterationSpec],
    level: usize,
    context: &JsonObject,
    output: &mut Vec<Value>,
) {
    let Some(iteration) = iterations.get(level) else {
        output.push(Value::Object(context.clone()));
        return;
    };
    let selected = if iteration.path.is_empty() {
        Some(current)
    } else {
        json_path::get(current, &iteration.path)
    };
    let Some(selected) = selected else { return };
    match selected {
        Value::Array(values) => {
            for value in values {
                let mut next = context.clone();
                next.insert(iteration.alias.clone(), value.clone());
                expand_iterations(value, iterations, level + 1, &next, output);
            }
        }
        Value::Null => {}
        value => {
            let mut next = context.clone();
            next.insert(iteration.alias.clone(), value.clone());
            expand_iterations(value, iterations, level + 1, &next, output);
        }
    }
}

pub(super) fn map_records(
    values: Vec<Value>,
    response: &ResponseConfig,
) -> Result<Vec<JsonObject>, EngineError> {
    values
        .iter()
        .map(|value| {
            let mut row = map_value(value, &response.output_mapping);
            apply_transforms(&mut row, &response.transforms)?;
            Ok(row)
        })
        .collect()
}

pub(super) fn map_value(value: &Value, mappings: &[OutputMapping]) -> JsonObject {
    if mappings.is_empty() {
        return match value {
            Value::Object(object) => object.clone(),
            value => Map::from_iter([("value".to_owned(), value.clone())]),
        };
    }

    mappings
        .iter()
        .map(|mapping| {
            let value = json_path::get(value, &mapping.path)
                .cloned()
                .or_else(|| mapping.default.clone())
                .unwrap_or(Value::Null);
            (mapping.column.clone(), value)
        })
        .collect()
}

fn apply_transforms(
    row: &mut JsonObject,
    transforms: &[ResponseTransform],
) -> Result<(), EngineError> {
    for transform in transforms {
        if let Some(condition) = transform.condition.as_deref()
            && !transform_condition(row, condition)
        {
            continue;
        }
        let source = row.get(&transform.source).cloned().unwrap_or(Value::Null);
        let value = transform_value(&source, transform).map_err(|failure| {
            EngineError::InvalidResponse(ErrorDetail::from(failure.describe()))
        })?;
        row.insert(transform.column.clone(), value);
    }
    Ok(())
}

/// A transform condition, `column == 'literal'` or `column != 'literal'`.
pub(super) struct TransformCondition<'a> {
    pub(super) column: &'a str,
    pub(super) expected: &'a str,
    pub(super) equals: bool,
}

fn parse_transform_condition(condition: &str) -> Option<TransformCondition<'_>> {
    let is_operator_or_quote = |character: char| matches!(character, '=' | '!' | '\'' | '"');
    // The column cannot contain `=`, `!` or a quote, so the first of those
    // characters starts the operator. Searching the whole string for `==`
    // instead would find one inside a quoted literal, as in
    // `status != 'a==b'`, and split there.
    let operator = condition.find(is_operator_or_quote)?;
    let (left, rest) = condition.split_at(operator);
    let (right, equals) = if let Some(right) = rest.strip_prefix("==") {
        (right, true)
    } else {
        (rest.strip_prefix("!=")?, false)
    };
    let column = left.trim();
    if column.is_empty() {
        return None;
    }
    // The literal is either quoted with one matching pair, or bare. Trimming
    // quote characters from both ends would accept `'active` or `active"` and
    // compare against a value the author never wrote.
    let right = right.trim();
    let expected = match right.chars().next() {
        Some(quote @ ('\'' | '"')) => {
            let inner = right
                .strip_prefix(quote)
                .and_then(|rest| rest.strip_suffix(quote))?;
            if inner.contains(quote) {
                return None;
            }
            inner
        }
        _ if right.is_empty() || right.contains(is_operator_or_quote) => return None,
        _ => right,
    };
    Some(TransformCondition {
        column,
        expected,
        equals,
    })
}

/// Evaluates a condition validated by [`validate_transforms`].
///
/// A missing or `null` column satisfies neither `==` nor `!=`: comparing it
/// as an empty string would make `column == ''` match a value that is not
/// there. As in SQL, the comparison is unknown and the transform is skipped.
fn transform_condition(row: &JsonObject, condition: &str) -> bool {
    let Some(condition) = parse_transform_condition(condition) else {
        // Unreachable after validation; never apply a transform on a condition
        // that was not understood.
        return false;
    };
    match row.get(condition.column) {
        None | Some(Value::Null) => false,
        Some(actual) => (value_as_text(actual) == condition.expected) == condition.equals,
    }
}

/// Largest decimal count accepted by `round`: beyond it `f64` has no digits
/// left to round.
const MAX_ROUND_DECIMALS: u64 = 15;

/// Validates response transforms before any network activity.
///
/// An unknown operation, a missing or ill-typed argument, or a condition that
/// cannot be parsed used to leave the column untouched or write `null`, which
/// is indistinguishable from data. They are configuration errors.
pub(super) fn validate_transforms(response: &ResponseConfig) -> Result<(), EngineError> {
    for transform in &response.transforms {
        let invalid =
            |reason: &'static str| Err(EngineError::InvalidInput(ErrorDetail::from(reason)));
        if transform.column.is_empty() {
            return invalid("has an empty column");
        }
        if transform.source.is_empty() {
            return invalid("has an empty source");
        }
        if transform
            .condition
            .as_deref()
            .is_some_and(|condition| parse_transform_condition(condition).is_none())
        {
            return invalid(
                "has a condition that is not `column == 'value'` or `column != 'value'`",
            );
        }
        let value = transform.value.as_ref();
        match transform.operation.as_str() {
            "add" | "subtract" | "multiply" | "divide" => {
                let Some(argument) = value else {
                    return invalid("requires a numeric value");
                };
                match numeric_operand(argument) {
                    Ok(argument) => {
                        if transform.operation == "divide" && argument.is_zero() {
                            return invalid("divides by zero");
                        }
                    }
                    Err(_) => return invalid("requires a numeric value"),
                }
            }
            "round" => {
                if let Some(decimals) = value
                    && !decimals
                        .as_u64()
                        .is_some_and(|decimals| decimals <= MAX_ROUND_DECIMALS)
                {
                    return invalid("requires an integer number of decimals from 0 to 15");
                }
            }
            "kelvin_to_celsius" | "celsius_to_kelvin" | "uppercase" | "lowercase" => {
                if value.is_some() {
                    return invalid("does not take a value");
                }
            }
            "prefix" | "suffix" => {
                if !value.is_some_and(is_text_scalar) {
                    return invalid("requires a string, number, or boolean value");
                }
            }
            "replace" => {
                let pair = value.and_then(Value::as_object).and_then(|pair| {
                    let find = pair.get("find")?.as_str()?;
                    pair.get("replace")?.as_str()?;
                    (pair.len() == 2 && !find.is_empty()).then_some(())
                });
                if pair.is_none() {
                    return invalid(
                        "requires a value with exactly a non-empty string 'find' and a string 'replace'",
                    );
                }
            }
            "default_if_null" => {
                if value.is_none() {
                    return invalid("requires a value");
                }
            }
            _ => {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "transform for column has an unknown operation",
                )));
            }
        }
    }
    Ok(())
}

fn is_text_scalar(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

/// Why a transform could not produce a value for one row.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TransformFailure {
    /// The source is not a number, nor a string holding one.
    NotNumeric,
    /// The source is not text-like (a string, number, or boolean).
    NotText,
    /// The source is not a string.
    NotString,
    /// The exact result, or an operand, has no exact representation.
    Unrepresentable,
}

impl TransformFailure {
    fn describe(&self) -> &'static str {
        match self {
            Self::NotNumeric => "received a value that is not numeric",
            Self::NotText => "received a value that is not a string, number, or boolean",
            Self::NotString => "received a value that is not a string",
            Self::Unrepresentable => "produced a result that cannot be represented exactly",
        }
    }
}

/// Operand of a numeric transform.
///
/// Integers stay integers: routing every value through `f64` silently rewrites
/// identifiers and counters beyond 2^53, which are common in REST payloads.
/// `Unsigned` exists because JSON integers above `i64::MAX` are legal and
/// `serde_json` represents them exactly.
#[derive(Clone, Copy)]
pub(super) enum Numeric {
    Signed(i64),
    Unsigned(u64),
    /// An integer written as a string and too large for `i64` or `u64`.
    /// Carrying it exactly keeps arithmetic that brings it back into range from
    /// being rounded on the way in.
    Wide(i128),
    Float(f64),
}

/// Largest integer magnitude that `f64` represents exactly, 2^53.
const F64_EXACT_INTEGER: i128 = 1 << 53;

impl Numeric {
    /// The value as `f64`, only when the conversion is exact.
    ///
    /// An integer beyond 2^53 has no exact `f64`; converting it would make
    /// floating point arithmetic start from a different number than the one
    /// received, so mixed or fractional arithmetic on it is refused.
    fn exact_f64(self) -> Option<f64> {
        match self {
            Self::Float(value) => Some(value),
            integer => integer
                .as_i128()
                .filter(|value| value.unsigned_abs() <= F64_EXACT_INTEGER.unsigned_abs())
                .map(|value| value as f64),
        }
    }

    fn is_zero(self) -> bool {
        match self {
            Self::Float(value) => value == 0.0,
            integer => integer.as_i128() == Some(0),
        }
    }

    /// The exact integer value, when there is one.
    fn as_i128(self) -> Option<i128> {
        match self {
            Self::Signed(value) => Some(i128::from(value)),
            Self::Unsigned(value) => Some(i128::from(value)),
            Self::Wide(value) => Some(value),
            Self::Float(_) => None,
        }
    }

    fn is_integer(self) -> bool {
        self.as_i128().is_some()
    }

    fn into_value(self) -> Result<Value, TransformFailure> {
        match self {
            Self::Signed(value) => Ok(Value::Number(Number::from(value))),
            Self::Unsigned(value) => Ok(Value::Number(Number::from(value))),
            Self::Wide(value) => integer_value(value),
            Self::Float(value) => float_value(value),
        }
    }
}

/// A finite `f64` as a JSON number; NaN and infinities have no JSON spelling.
fn float_value(value: f64) -> Result<Value, TransformFailure> {
    Number::from_f64(value)
        .map(Value::Number)
        .ok_or(TransformFailure::Unrepresentable)
}

/// Narrows an exact integer result back to a JSON representable integer.
///
/// Fails when the value fits neither `i64` nor `u64`: emitting a rounded `f64`
/// there would reintroduce exactly the silent precision loss this type exists
/// to prevent.
fn integer_value(value: i128) -> Result<Value, TransformFailure> {
    if let Ok(value) = i64::try_from(value) {
        return Ok(Value::Number(Number::from(value)));
    }
    u64::try_from(value)
        .map(|value| Value::Number(Number::from(value)))
        .map_err(|_| TransformFailure::Unrepresentable)
}

fn numeric_operand(value: &Value) -> Result<Numeric, TransformFailure> {
    if let Some(integer) = value.as_i64() {
        return Ok(Numeric::Signed(integer));
    }
    if let Some(integer) = value.as_u64() {
        return Ok(Numeric::Unsigned(integer));
    }
    if let Some(float) = value.as_f64() {
        return Ok(Numeric::Float(float));
    }
    let text = value.as_str().ok_or(TransformFailure::NotNumeric)?.trim();
    if let Ok(integer) = text.parse::<i64>() {
        return Ok(Numeric::Signed(integer));
    }
    if let Ok(integer) = text.parse::<u64>() {
        return Ok(Numeric::Unsigned(integer));
    }
    // A numeric identifier carried as a string can exceed `u64`. Parsing it as
    // `i128` before reaching for `f64` keeps the value exact, which matters
    // because the very next step may bring it back into a representable range.
    if let Ok(integer) = text.parse::<i128>() {
        return Ok(Numeric::Wide(integer));
    }
    // An integer literal too wide even for `i128` must not fall through to the
    // float parser, which would accept it rounded.
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TransformFailure::Unrepresentable);
    }
    match text.parse::<f64>() {
        Ok(float) if float.is_finite() => Ok(Numeric::Float(float)),
        Ok(_) => Err(TransformFailure::Unrepresentable),
        Err(_) => Err(TransformFailure::NotNumeric),
    }
}

/// Result of an exact integer computation on two integral operands.
pub(super) enum IntegerOutcome {
    Exact(i128),
    /// The operands are integers but the result is not, as in `7 / 2`. Falling
    /// back to floating point is the answer the caller wants, provided both
    /// operands convert to `f64` exactly.
    Fractional,
    /// The result is an integer too large to represent. Falling back to
    /// floating point here would silently return a rounded value, which is the
    /// precision loss this type exists to prevent.
    Unrepresentable,
}

/// Applies `integer` when both operands are integral, and floating point
/// arithmetic when either operand is a float or the exact result is fractional.
///
/// `i128` is wide enough for any sum, difference, or product of two JSON
/// integers except the extremes of `u64 * u64`, which `checked_*` reports.
/// Floating point starts only from operands that convert to `f64` exactly, and
/// a non-finite result is refused: either would return a number other than the
/// one the arithmetic defines, without saying so.
fn numeric_arithmetic(
    left: Numeric,
    right: Numeric,
    integer: impl Fn(i128, i128) -> IntegerOutcome,
    float: impl Fn(f64, f64) -> f64,
) -> Result<Value, TransformFailure> {
    if let (Some(left), Some(right)) = (left.as_i128(), right.as_i128()) {
        match integer(left, right) {
            IntegerOutcome::Exact(result) => return integer_value(result),
            IntegerOutcome::Unrepresentable => return Err(TransformFailure::Unrepresentable),
            IntegerOutcome::Fractional => {}
        }
    }
    let (Some(left), Some(right)) = (left.exact_f64(), right.exact_f64()) else {
        return Err(TransformFailure::Unrepresentable);
    };
    float_value(float(left, right))
}

/// Wraps a checked `i128` operation: `None` means the exact result overflows.
fn checked_integer(result: Option<i128>) -> IntegerOutcome {
    result.map_or(IntegerOutcome::Unrepresentable, IntegerOutcome::Exact)
}

/// Applies one transform validated by [`validate_transforms`] to one value.
///
/// `null` propagates: every operation except `default_if_null` maps a `null`
/// source to `null`, as SQL does. Any other value the operation cannot handle
/// fails the row instead of being passed through or replaced by `null`.
fn transform_value(
    source: &Value,
    transform: &ResponseTransform,
) -> Result<Value, TransformFailure> {
    if transform.operation == "default_if_null" {
        return Ok(if source.is_null() {
            transform.value.clone().unwrap_or(Value::Null)
        } else {
            source.clone()
        });
    }
    if source.is_null() {
        return Ok(Value::Null);
    }
    let argument = || {
        transform
            .value
            .as_ref()
            .map(numeric_operand)
            .ok_or(TransformFailure::NotNumeric)?
    };
    let text = || {
        if is_text_scalar(source) {
            Ok(value_as_text(source))
        } else {
            Err(TransformFailure::NotText)
        }
    };
    let fixed_text = || {
        transform
            .value
            .as_ref()
            .map(value_as_text)
            .unwrap_or_default()
    };
    match transform.operation.as_str() {
        "subtract" => numeric_arithmetic(
            numeric_operand(source)?,
            argument()?,
            |a, b| checked_integer(a.checked_sub(b)),
            |a, b| a - b,
        ),
        "add" => numeric_arithmetic(
            numeric_operand(source)?,
            argument()?,
            |a, b| checked_integer(a.checked_add(b)),
            |a, b| a + b,
        ),
        "multiply" => numeric_arithmetic(
            numeric_operand(source)?,
            argument()?,
            |a, b| checked_integer(a.checked_mul(b)),
            |a, b| a * b,
        ),
        "divide" => {
            let divisor = argument()?;
            if divisor.is_zero() {
                // Rejected by validation; kept so this function never divides
                // by zero on its own.
                return Err(TransformFailure::Unrepresentable);
            }
            // Only an exact integer division stays integral; a remainder
            // means the true result is fractional, not unrepresentable.
            numeric_arithmetic(
                numeric_operand(source)?,
                divisor,
                |a, b| match a.checked_rem(b) {
                    Some(0) => checked_integer(a.checked_div(b)),
                    Some(_) => IntegerOutcome::Fractional,
                    None => IntegerOutcome::Unrepresentable,
                },
                |a, b| a / b,
            )
        }
        "round" => {
            let value = numeric_operand(source)?;
            if value.is_integer() {
                // Rounding an integer to any number of decimals is the integer.
                return value.into_value();
            }
            let decimals = transform
                .value
                .as_ref()
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(MAX_ROUND_DECIMALS);
            let factor = 10_f64.powi(decimals as i32);
            let value = value.exact_f64().ok_or(TransformFailure::Unrepresentable)?;
            float_value((value * factor).round() / factor)
        }
        "kelvin_to_celsius" | "celsius_to_kelvin" => {
            let value = numeric_operand(source)?
                .exact_f64()
                .ok_or(TransformFailure::Unrepresentable)?;
            let offset = 273.15;
            float_value(if transform.operation == "kelvin_to_celsius" {
                value - offset
            } else {
                value + offset
            })
        }
        "uppercase" => source
            .as_str()
            .map(|value| Value::String(value.to_uppercase()))
            .ok_or(TransformFailure::NotString),
        "lowercase" => source
            .as_str()
            .map(|value| Value::String(value.to_lowercase()))
            .ok_or(TransformFailure::NotString),
        "prefix" => Ok(Value::String(format!("{}{}", fixed_text(), text()?))),
        "suffix" => Ok(Value::String(format!("{}{}", text()?, fixed_text()))),
        "replace" => {
            let pair = transform.value.as_ref().and_then(Value::as_object);
            let find = pair.and_then(|pair| pair.get("find")?.as_str());
            let replacement = pair.and_then(|pair| pair.get("replace")?.as_str());
            match (find, replacement) {
                (Some(find), Some(replacement)) if !find.is_empty() => {
                    Ok(Value::String(text()?.replace(find, replacement)))
                }
                // Rejected by validation.
                _ => Err(TransformFailure::NotText),
            }
        }
        // Rejected by validation: never pass an unknown operation through.
        _ => Err(TransformFailure::NotText),
    }
}

pub(super) fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

pub(super) fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        value => Some(value_as_text(value)),
    }
}

pub(super) fn usize_value(value: usize) -> Result<Value, EngineError> {
    let value = u64::try_from(value)
        .map_err(|_| EngineError::InvalidInput(ErrorDetail::from("numeric value is too large")))?;
    Ok(Value::Number(Number::from(value)))
}
