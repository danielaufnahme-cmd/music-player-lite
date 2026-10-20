//! Small helpers that make porting Python dict code to `serde_json::Value` painless.

use serde_json::{Map, Value};

/// Walk a dotted path ("contents.0.musicShelfRenderer"); numbers index arrays.
/// Like ytmusicapi's `nav(..., none_if_absent=True)`.
pub fn nav<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    if path.is_empty() {
        return Some(cur);
    }
    for key in path.split('.') {
        cur = match cur {
            Value::Object(map) => map.get(key)?,
            Value::Array(items) => items.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

pub fn nav_str<'a>(root: &'a Value, path: &str) -> Option<&'a str> {
    nav(root, path).and_then(Value::as_str)
}

pub fn nav_arr<'a>(root: &'a Value, path: &str) -> Option<&'a Vec<Value>> {
    nav(root, path).and_then(Value::as_array)
}

/// Python truthiness of a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `d.get(key)` with missing -> None, null kept as Some(Null).
pub fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object().and_then(|m| m.get(key))
}

/// The first truthy value of `d.get(k1) or d.get(k2) or ...`.
pub fn first_truthy<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().filter_map(|k| get(v, k)).find(|x| truthy(x))
}

/// Python `str(x)` for the simple JSON values we meet in requests.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

/// Python `str.splitlines()` (without keeping ends).
pub fn splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let is_break = matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}');
        if is_break {
            out.push(&text[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(j, '\n')) = chars.peek() {
                    chars.next();
                    end = j + 1;
                }
            }
            start = end;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Python's `urllib.parse.urlencode` (quote_plus) for string pairs.
pub fn urlencode(pairs: &[(&str, &str)]) -> String {
    fn quote_plus(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => out.push(b as char),
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
    pairs.iter().map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v))).collect::<Vec<_>>().join("&")
}

/// `os.path.expanduser` for a leading `~`.
pub fn expanduser(path: &str) -> std::path::PathBuf {
    let home = || std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(std::path::PathBuf::from);
    if path == "~" {
        if let Some(h) = home() {
            return h;
        }
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(h) = home() {
            return h.join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

/// Round half-to-even to `digits` decimals, like Python's `round(x, n)` in practice.
pub fn py_round(x: f64, digits: i32) -> f64 {
    let m = 10f64.powi(digits);
    let y = x * m;
    // Values like 12.35 are not exact in binary; Python rounds the true binary value,
    // so only treat as a tie when it is (nearly) exactly .5.
    let r = if ((y - y.trunc()).abs() - 0.5).abs() < 1e-9 { y.round_ties_even() } else { y.round() };
    r / m
}

pub fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nav_walks_objects_and_arrays() {
        let v = json!({"a": [{"b": 1}]});
        assert_eq!(nav(&v, "a.0.b"), Some(&json!(1)));
        assert_eq!(nav(&v, "a.1.b"), None);
        assert_eq!(nav(&v, "a.x"), None);
    }

    #[test]
    fn splitlines_like_python() {
        assert_eq!(splitlines("a\nb\r\nc\rd\n"), vec!["a", "b", "c", "d"]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
        assert_eq!(splitlines("a\n\nb"), vec!["a", "", "b"]);
    }

    #[test]
    fn urlencode_like_python() {
        assert_eq!(urlencode(&[("track_name", "Song (x) & y"), ("q", "é")]), "track_name=Song+%28x%29+%26+y&q=%C3%A9");
    }
}
