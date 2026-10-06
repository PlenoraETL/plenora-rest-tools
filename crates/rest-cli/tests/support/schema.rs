//! Validatore JSON Schema minimo per i tre schemi comuni copiati nei fixture.
//!
//! Perché non una dipendenza: il crate `jsonschema` porterebbe decine di crate
//! transitivi in Cargo.lock (e nella policy di cargo-deny) per tre schemi di
//! test. Questi schemi usano un sottoinsieme piccolo e fisso di Draft 2020-12,
//! che qui è interpretato davvero, sui file degli schemi, invece di essere
//! ricopiato a mano in controlli che potrebbero divergere.
//!
//! È chiuso per costruzione: una parola chiave non elencata, un `$ref` non
//! risolvibile o un `pattern` che il validatore non conosce fanno fallire il
//! test, mai passare un'istanza senza controllo. I `pattern` sono
//! riconosciuti per stringa esatta e implementati a mano, perché il workspace
//! non ha un motore di espressioni regolari.

use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// I tre schemi di plenora-contracts al commit 1e902dfa, con il digest
/// canonico (JSON compatto a chiavi ordinate, come `canonical_digest` di
/// scripts/validate_contracts.py) che ne blocca il contenuto.
const SCHEMAS: [(&str, &str, &str); 3] = [
    (
        "cli-envelope-v2.schema.json",
        include_str!("../fixtures/contracts/cli-envelope-v2.schema.json"),
        "e2210f179e24190ea85d1c82b5cacc2429ea7ba3505bbc47395e638236db19ea",
    ),
    (
        "error-v1.schema.json",
        include_str!("../fixtures/contracts/error-v1.schema.json"),
        "164a0c580080d93ee1a020e7e33b651fe46950a4133e9bd8c044daa64cb9b914",
    ),
    (
        "capabilities-v2.schema.json",
        include_str!("../fixtures/contracts/capabilities-v2.schema.json"),
        "c3982782dcf9afe1e1a21e87292e73cf17feac742d1cf0bcb9cbc38d9480c450",
    ),
];

const ANNOTATIONS: [&str; 5] = ["$schema", "$id", "title", "$defs", "description"];

pub struct Registry {
    by_id: BTreeMap<String, Value>,
}

impl Registry {
    /// Carica gli schemi e verifica che siano quelli del commit di
    /// riferimento.
    pub fn load() -> Self {
        let mut by_id = BTreeMap::new();
        for (name, source, expected) in SCHEMAS {
            let document: Value = serde_json::from_str(source).unwrap();
            let canonical = serde_json::to_string(&document).unwrap();
            let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
            assert_eq!(
                digest, expected,
                "{name} differs from plenora-contracts 1e902dfa"
            );
            let id = document["$id"].as_str().unwrap().to_owned();
            by_id.insert(id, document);
        }
        Self { by_id }
    }

    fn document(&self, id: &str) -> &Value {
        self.by_id
            .get(id)
            .unwrap_or_else(|| panic!("unknown schema {id}"))
    }

    /// Valida `instance` contro lo schema con `$id` = `id`.
    pub fn validate(&self, id: &str, instance: &Value) -> Result<(), String> {
        let root = self.document(id);
        self.check(root, root, instance, "$")
    }

    fn resolve<'a>(&'a self, root: &'a Value, reference: &str) -> (&'a Value, &'a Value) {
        let (base, fragment) = reference.split_once('#').unwrap_or((reference, ""));
        let document = if base.is_empty() {
            root
        } else {
            self.document(base)
        };
        let mut target = document;
        if !fragment.is_empty() {
            let pointer = fragment
                .strip_prefix('/')
                .unwrap_or_else(|| panic!("unsupported anchor {reference}"));
            for token in pointer.split('/') {
                target = target
                    .get(token)
                    .unwrap_or_else(|| panic!("unresolved $ref {reference}"));
            }
        }
        (document, target)
    }

    fn check(
        &self,
        root: &Value,
        schema: &Value,
        instance: &Value,
        at: &str,
    ) -> Result<(), String> {
        let Some(schema) = schema.as_object() else {
            panic!("schema at {at} is not an object");
        };
        for (keyword, value) in schema {
            match keyword.as_str() {
                keyword if ANNOTATIONS.contains(&keyword) => {}
                "$ref" => {
                    let (document, target) = self.resolve(root, value.as_str().unwrap());
                    self.check(document, target, instance, at)?;
                }
                "type" => {
                    let accepted = match value {
                        Value::String(name) => vec![name.as_str()],
                        Value::Array(names) => names.iter().map(|n| n.as_str().unwrap()).collect(),
                        _ => panic!("bad type keyword"),
                    };
                    if !accepted.iter().any(|name| has_type(instance, name)) {
                        return Err(format!("{at}: expected type {accepted:?}"));
                    }
                }
                "enum" => {
                    if !value.as_array().unwrap().contains(instance) {
                        return Err(format!("{at}: {instance} is not in the enum"));
                    }
                }
                "const" => {
                    if value != instance {
                        return Err(format!("{at}: expected {value}, found {instance}"));
                    }
                }
                "required" => {
                    if let Some(object) = instance.as_object() {
                        for name in value.as_array().unwrap() {
                            if !object.contains_key(name.as_str().unwrap()) {
                                return Err(format!("{at}: missing required {name}"));
                            }
                        }
                    }
                }
                "properties" => {
                    if let Some(object) = instance.as_object() {
                        for (name, property) in value.as_object().unwrap() {
                            if let Some(member) = object.get(name) {
                                self.check(root, property, member, &format!("{at}.{name}"))?;
                            }
                        }
                    }
                }
                "additionalProperties" => {
                    assert_eq!(
                        value,
                        &Value::Bool(false),
                        "only additionalProperties: false"
                    );
                    if let Some(object) = instance.as_object() {
                        let declared = schema
                            .get("properties")
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        if let Some(extra) = object.keys().find(|key| !declared.contains_key(*key))
                        {
                            return Err(format!("{at}: unexpected property {extra}"));
                        }
                    }
                }
                "items" => {
                    if let Some(items) = instance.as_array() {
                        for (index, item) in items.iter().enumerate() {
                            self.check(root, value, item, &format!("{at}[{index}]"))?;
                        }
                    }
                }
                "minItems" => {
                    if let Some(items) = instance.as_array()
                        && (items.len() as u64) < value.as_u64().unwrap()
                    {
                        return Err(format!("{at}: too few items"));
                    }
                }
                "uniqueItems" => {
                    assert_eq!(value, &Value::Bool(true));
                    if let Some(items) = instance.as_array() {
                        for (index, item) in items.iter().enumerate() {
                            if items[..index].contains(item) {
                                return Err(format!("{at}: duplicate item {item}"));
                            }
                        }
                    }
                }
                "minLength" | "maxLength" => {
                    if let Some(text) = instance.as_str() {
                        let length = text.chars().count() as u64;
                        let bound = value.as_u64().unwrap();
                        let ok = if keyword == "minLength" {
                            length >= bound
                        } else {
                            length <= bound
                        };
                        if !ok {
                            return Err(format!("{at}: length {length} violates {keyword}"));
                        }
                    }
                }
                "minimum" | "maximum" => {
                    if let Some(number) = instance.as_f64() {
                        let bound = value.as_f64().unwrap();
                        let ok = if keyword == "minimum" {
                            number >= bound
                        } else {
                            number <= bound
                        };
                        if !ok {
                            return Err(format!("{at}: {number} violates {keyword}"));
                        }
                    }
                }
                "pattern" => {
                    if let Some(text) = instance.as_str()
                        && !matches_pattern(value.as_str().unwrap(), text)
                    {
                        return Err(format!("{at}: {text:?} does not match {value}"));
                    }
                }
                "allOf" => {
                    for branch in value.as_array().unwrap() {
                        self.check(root, branch, instance, at)?;
                    }
                }
                "oneOf" => {
                    let passing = value
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|branch| self.check(root, branch, instance, at).is_ok())
                        .count();
                    if passing != 1 {
                        return Err(format!("{at}: {passing} oneOf branches match"));
                    }
                }
                "if" => {
                    if self.check(root, value, instance, at).is_ok()
                        && let Some(then) = schema.get("then")
                    {
                        self.check(root, then, instance, at)?;
                    }
                }
                "then" => {}
                other => panic!("keyword {other} is not supported by the test validator"),
            }
        }
        Ok(())
    }
}

fn has_type(instance: &Value, name: &str) -> bool {
    match name {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        "integer" => instance.is_i64() || instance.is_u64(),
        "number" => instance.is_number(),
        other => panic!("unknown type {other}"),
    }
}

fn all(text: &str, allowed: impl Fn(char) -> bool) -> bool {
    text.chars().all(allowed)
}

fn lower_digit_dash(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
}

fn lower_digit_dash_underscore(c: char) -> bool {
    lower_digit_dash(c) || c == '_'
}

/// `[a-z]` seguito da `rest`, con la lunghezza di `rest` nei limiti.
fn lower_then(text: &str, rest: impl Fn(char) -> bool, min: usize, max: usize) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let tail = chars.as_str();
    first.is_ascii_lowercase() && all(tail, rest) && (min..=max).contains(&tail.chars().count())
}

fn number_without_leading_zero(text: &str) -> bool {
    !text.is_empty() && all(text, |c| c.is_ascii_digit()) && (text == "0" || !text.starts_with('0'))
}

fn semver(text: &str) -> bool {
    let (core, suffix) = match text.find(['-', '+']) {
        Some(index) => (&text[..index], Some(&text[index + 1..])),
        None => (text, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| number_without_leading_zero(part))
        && suffix.is_none_or(|suffix| {
            !suffix.is_empty()
                && all(suffix, |c| {
                    c.is_ascii_alphanumeric() || c == '.' || c == '-'
                })
        })
}

fn contract_id(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("plenora-") else {
        return false;
    };
    rest.match_indices("-v").any(|(index, _)| {
        let (name, version) = (&rest[..index], &rest[index + 2..]);
        !name.is_empty()
            && all(name, lower_digit_dash)
            && !version.is_empty()
            && all(version, |c| c.is_ascii_digit())
            && !version.starts_with('0')
    })
}

fn media_type_token(text: &str) -> bool {
    !text.is_empty()
        && all(text, |c| {
            c.is_ascii_alphanumeric() || "!#$&^_.+-".contains(c)
        })
}

fn matches_pattern(pattern: &str, text: &str) -> bool {
    match pattern {
        r"^plenora-[a-z][a-z0-9-]{1,62}$" => text
            .strip_prefix("plenora-")
            .is_some_and(|rest| lower_then(rest, lower_digit_dash, 1, 62)),
        r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)([-+][0-9A-Za-z.-]+)?$" => semver(text),
        r"^plenora-[a-z0-9-]+-v[1-9][0-9]*$" => contract_id(text),
        r"^[a-z][a-z0-9-]{0,63}$" => lower_then(text, lower_digit_dash, 0, 63),
        r"^[A-Z][A-Z0-9_]{1,63}$" => {
            let mut chars = text.chars();
            chars.next().is_some_and(|c| c.is_ascii_uppercase())
                && all(chars.as_str(), |c| {
                    c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'
                })
                && (1..=63).contains(&chars.as_str().len())
        }
        r"^[a-z][a-z0-9_-]{0,63}$" => lower_then(text, lower_digit_dash_underscore, 0, 63),
        r"^[a-z][a-z0-9_-]*(\.[a-z][a-z0-9_-]*)+$" => {
            let segments: Vec<&str> = text.split('.').collect();
            segments.len() >= 2
                && segments
                    .iter()
                    .all(|segment| lower_then(segment, lower_digit_dash_underscore, 0, usize::MAX))
        }
        r"^[A-Za-z0-9!#$&^_.+-]+/[A-Za-z0-9!#$&^_.+-]+$" => text
            .split_once('/')
            .is_some_and(|(kind, subtype)| media_type_token(kind) && media_type_token(subtype)),
        other => panic!("pattern {other} is not implemented by the test validator"),
    }
}

#[test]
fn the_validator_rejects_what_the_schemas_reject() {
    let registry = Registry::load();
    let envelope = "https://schemas.plenora.dev/cli-envelope-v2.schema.json";
    let ok = serde_json::json!({
        "status": "ok", "protocol_version": 2, "component": "plenora-rest-tools",
        "component_version": "0.3.0", "contract": "plenora-rest-version-result-v1",
        "command": "version", "result": {}
    });
    registry.validate(envelope, &ok).unwrap();
    let error = serde_json::json!({
        "status": "error", "protocol_version": 2, "component": "plenora-rest-tools",
        "component_version": "0.3.0-rc.1", "contract": "plenora-error-v1",
        "command": "unknown",
        "error": {"category": "io", "phase": "read", "remote_effect": "none",
                  "retry": {"kind": "never"}, "code": "FILE_IO", "message": "m", "details": {}}
    });
    registry.validate(envelope, &error).unwrap();

    let mutations: [(&str, Value); 12] = [
        ("/status", "maybe".into()),
        ("/protocol_version", 1.into()),
        ("/component", "rest-tools".into()),
        ("/component_version", "01.2.3".into()),
        ("/contract", "plenora-error".into()),
        ("/command", "Unknown".into()),
        ("/error/category", "not_a_category".into()),
        ("/error/code", "lower".into()),
        ("/error/message", "".into()),
        ("/error/retry", serde_json::json!({"kind": "after"})),
        ("/error/remote_effect", "unknown".into()),
        ("/error/extra", true.into()),
    ];
    for (pointer, replacement) in mutations {
        let mut mutated = error.clone();
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        let target = if parent.is_empty() {
            &mut mutated
        } else {
            mutated.pointer_mut(parent).unwrap()
        };
        let mut replacement = replacement;
        if pointer == "/error/remote_effect" {
            // `unknown` con retry `safe` è vietato; con `never` è ammesso.
            target["retry"] = serde_json::json!({"kind": "safe"});
            replacement = "unknown".into();
        }
        target[key] = replacement;
        assert!(
            registry.validate(envelope, &mutated).is_err(),
            "{pointer} must be rejected"
        );
    }
    let mut both = ok.clone();
    both["error"] = error["error"].clone();
    assert!(registry.validate(envelope, &both).is_err());
}
