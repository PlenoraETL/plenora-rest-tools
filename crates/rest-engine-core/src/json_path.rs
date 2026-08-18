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

fn parse(path: &str) -> Result<Vec<Segment>, ()> {
    let path = path.trim();
    if path.is_empty() || path == "$" {
        return Ok(Vec::new());
    }

    let chars: Vec<char> = path.strip_prefix('$').unwrap_or(path).chars().collect();
    let mut segments = Vec::new();
    let mut index = 0;

    while index < chars.len() {
        if chars[index] == '.' {
            index += 1;
            continue;
        }

        if chars[index] == '[' {
            let close = chars[index + 1..]
                .iter()
                .position(|character| *character == ']')
                .map(|offset| index + 1 + offset)
                .ok_or(())?;
            let raw: String = chars[index + 1..close].iter().collect();
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
            index = close + 1;
            continue;
        }

        let start = index;
        while index < chars.len() && chars[index] != '.' && chars[index] != '[' {
            index += 1;
        }
        if start == index {
            return Err(());
        }
        segments.push(Segment::Key(chars[start..index].iter().collect()));
    }

    Ok(segments)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::get;

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
}
