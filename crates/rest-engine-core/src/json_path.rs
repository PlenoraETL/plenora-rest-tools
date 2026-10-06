use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
enum Segment {
    Key(String),
    Index(isize),
    Filter { key: String, value: String },
}

pub(crate) fn get<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    for segment in parse(path).ok()? {
        current = match segment {
            Segment::Key(key) => current.get(&key)?,
            Segment::Index(index) => {
                let items = current.as_array()?;
                let index = if index < 0 {
                    items.len().checked_sub(index.unsigned_abs())?
                } else {
                    usize::try_from(index).ok()?
                };
                items.get(index)?
            }
            Segment::Filter { key, value } => current.as_array()?.iter().find(|item| {
                item.get(&key).is_some_and(|candidate| {
                    candidate.as_str().map_or_else(
                        || {
                            serde_json::from_str::<Value>(&value)
                                .as_ref()
                                .is_ok_and(|expected| expected == candidate)
                        },
                        |text| text == value,
                    )
                })
            })?,
        };
    }
    Some(current)
}

/// Whether `path` is a well-formed JSON path for [`get`].
///
/// `get` reads a malformed path as "nothing there", which on a response
/// would turn a configuration mistake into a missing value. Every path in a
/// request is therefore checked with this function before execution, so that
/// at run time "absent" only ever means absent.
pub(crate) fn is_valid(path: &str) -> bool {
    parse(path).is_ok()
}

fn parse(path: &str) -> Result<Vec<Segment>, ()> {
    let path = path.trim();
    if path.is_empty() || path == "$" {
        return Ok(Vec::new());
    }

    let mut rest = path.strip_prefix('$').unwrap_or(path);
    let mut segments = Vec::new();

    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            rest = after;
            continue;
        }

        if let Some(after) = rest.strip_prefix('[') {
            let (raw, tail) = after.split_once(']').ok_or(())?;
            let raw = raw.trim();
            if let Ok(array_index) = raw.parse::<isize>() {
                segments.push(Segment::Index(array_index));
            } else if let Some((key, value)) = raw.split_once('=') {
                let key = key.trim();
                let value = value
                    .trim()
                    .trim_matches(|character| character == '"' || character == '\'');
                if key.is_empty() || value.is_empty() {
                    return Err(());
                }
                segments.push(Segment::Filter {
                    key: key.to_owned(),
                    value: value.to_owned(),
                });
            } else {
                let key = raw
                    .strip_prefix('"')
                    .and_then(|value| value.strip_suffix('"'))
                    .or_else(|| {
                        raw.strip_prefix('\'')
                            .and_then(|value| value.strip_suffix('\''))
                    })
                    .ok_or(())?;
                segments.push(Segment::Key(key.to_owned()));
            }
            rest = tail;
            continue;
        }

        let end = rest.find(['.', '[']).unwrap_or(rest.len());
        let (key, tail) = rest.split_at_checked(end).ok_or(())?;
        if key.is_empty() {
            return Err(());
        }
        segments.push(Segment::Key(key.to_owned()));
        rest = tail;
    }

    Ok(segments)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{get, is_valid};

    #[test]
    fn supports_dot_and_bracket_paths() {
        let value = json!({
            "data": {
                "items": [
                    {"name": "Ada", "kind": "person"},
                    {"name": "Grace", "kind": "admin"}
                ]
            }
        });
        assert_eq!(get(&value, "$.data.items[0].name"), Some(&json!("Ada")));
        assert_eq!(get(&value, "data['items'][0].name"), Some(&json!("Ada")));
        assert_eq!(get(&value, "data.items[-1].name"), Some(&json!("Grace")));
        assert_eq!(
            get(&value, "data.items[kind=admin].name"),
            Some(&json!("Grace"))
        );
        assert_eq!(
            get(&value, "data.items[kind='person'].name"),
            Some(&json!("Ada"))
        );
        assert_eq!(get(&value, "$"), Some(&value));
        assert_eq!(get(&value, "missing"), None);
    }

    #[test]
    fn malformed_paths_are_not_valid() {
        for path in ["a[", "a[0", "[]", "[ ]", "a[=b]", "a[b=]", "a[unquoted]"] {
            assert!(!is_valid(path), "{path:?} must be refused");
            assert_eq!(get(&json!({"a": 1}), path), None);
        }
        for path in [
            "", "$", "a", "$.a.b", "a[0]", "a[-1]", "a['b']", "a[k=v]", "a..b",
        ] {
            assert!(is_valid(path), "{path:?} must be accepted");
        }
    }
}
