#![allow(dead_code)]
//! Replays recorded InnerTube / LRCLIB responses (tests/fixtures/*.json, recorded
//! once from the Python worker + ytmusicapi, see tests/README in the crate docs).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use geonix_yt::net::{BoxFut, Client, Error, Result, Transport};
use serde_json::Value;

pub fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(format!("{name}.json"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))).unwrap()
}

#[derive(Default)]
pub struct Replay {
    pub requests: Vec<Value>,
    pub http: Vec<Value>,
    pub used: Mutex<Vec<String>>,
    pub urls: Mutex<Vec<String>>,
    pub unmatched: Mutex<Vec<String>>,
}

impl Replay {
    pub fn from_fixture(f: &Value) -> Self {
        Self {
            requests: f["requests"].as_array().cloned().unwrap_or_default(),
            http: f["http"].as_array().cloned().unwrap_or_default(),
            ..Default::default()
        }
    }

    /// Fake for LRCLIB/HTTP only: `(url substring, status, body)`.
    pub fn http_only(entries: Vec<(&str, u16, Value)>) -> Self {
        Self {
            http: entries.into_iter().map(|(u, s, b)| serde_json::json!({"url": u, "status": s, "body": b, "substring": true})).collect(),
            ..Default::default()
        }
    }

    pub fn add_innertube(&mut self, endpoint: &str, response: Value) {
        self.requests.push(serde_json::json!({"endpoint": endpoint, "any": true, "response": response}));
    }
}

impl Transport for Replay {
    fn innertube<'a>(&'a self, endpoint: &'a str, client: Client, body: Value, params: &'a str) -> BoxFut<'a, Result<Value>> {
        Box::pin(async move {
            let client_name = match client {
                Client::Web => "WEB_REMIX",
                Client::Mobile => "ANDROID_MUSIC",
            };
            for r in &self.requests {
                let any = r.get("any").is_some();
                if r["endpoint"] == endpoint && (any || (r["body"] == body && r["params"] == params && r["client"] == client_name)) {
                    self.used.lock().unwrap().push(format!("{endpoint} {body}"));
                    return Ok(r["response"].clone());
                }
            }
            self.unmatched.lock().unwrap().push(format!("{endpoint} {client_name} {body} {params}"));
            Err(Error::Net(format!("connection refused: no fixture for {endpoint} {body} {params}")))
        })
    }

    fn get_json<'a>(&'a self, url: &'a str, _ua: &'a str, _timeout: Duration) -> BoxFut<'a, Result<Value>> {
        Box::pin(async move {
            self.urls.lock().unwrap().push(url.to_string());
            for h in &self.http {
                let u = h["url"].as_str().unwrap_or("");
                let hit = if h.get("substring").is_some() { url.contains(u) } else { url == u };
                if hit {
                    let status = h["status"].as_u64().unwrap_or(200) as u16;
                    if status != 200 {
                        return Err(Error::Http(status, format!("HTTP Error {status}")));
                    }
                    return Ok(h["body"].clone());
                }
            }
            self.unmatched.lock().unwrap().push(url.to_string());
            Err(Error::Net(format!("connection refused: no fixture for {url}")))
        })
    }

    fn get_bytes<'a>(&'a self, url: &'a str, _ua: &'a str, _timeout: Duration) -> BoxFut<'a, Result<Vec<u8>>> {
        Box::pin(async move { Err(Error::Net(format!("connection refused: offline test ({url})"))) })
    }
}

/// Compare JSON, allowing tiny float differences.
pub fn assert_json_eq(actual: &Value, expected: &Value, path: &str) {
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            assert!((a - b).abs() < 1e-6, "{path}: {a} != {b}");
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: length {} != {}", a.len(), b.len());
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                assert_json_eq(x, y, &format!("{path}[{i}]"));
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            let mut ka: Vec<_> = a.keys().collect();
            let mut kb: Vec<_> = b.keys().collect();
            ka.sort();
            kb.sort();
            assert_eq!(ka, kb, "{path}: keys differ");
            for (k, v) in a {
                assert_json_eq(v, &b[k], &format!("{path}.{k}"));
            }
        }
        _ => assert_eq!(actual, expected, "{path}"),
    }
}
