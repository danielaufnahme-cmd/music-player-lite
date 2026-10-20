//! The music provider: YouTube Music search, playlist info, downloads,
//! recommendations, lyrics and stream URLs. In the lite build this runs
//! in-process (`geonix_yt::Yt`) instead of a Python child process; the
//! [`Provider`] trait keeps the old JSON command surface so tests can use a
//! mock and call sites stay unchanged.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::error::ApiError;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Progress {
    #[serde(default)]
    pub percent: f64,
    #[serde(default)]
    pub speed: Option<String>,
}

pub type ProgressFn = Box<dyn Fn(Value) + Send + Sync>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One JSON command in, one JSON result (or an error message) out, with
/// optional progress updates in between. Same commands and payloads as the
/// old Python worker: search, playlist_info, download, recommend, song_info,
/// stream_url, lyrics, ping.
pub trait Provider: Send + Sync + 'static {
    fn handle<'a>(&'a self, cmd: &'a str, params: Value, progress: ProgressFn) -> BoxFuture<'a, Result<Value, String>>;
}

impl Provider for geonix_yt::Yt {
    fn handle<'a>(&'a self, cmd: &'a str, params: Value, progress: ProgressFn) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(geonix_yt::Yt::handle(self, cmd, params, progress))
    }
}

/// Cheap, cloneable handle to the provider.
#[derive(Clone)]
pub struct Music {
    inner: Arc<dyn Provider>,
}

impl Music {
    pub fn new(provider: impl Provider) -> Self {
        Self { inner: Arc::new(provider) }
    }

    /// The in-process provider never "stops"; kept for `/api/health`.
    pub fn is_running(&self) -> bool {
        true
    }

    /// Run a command (`{"cmd": "...", ...params}`) and wait for its result.
    pub async fn request(&self, cmd: Value, timeout: Duration) -> Result<Value, ApiError> {
        self.request_with_progress(cmd, None, timeout).await
    }

    pub async fn request_with_progress(
        &self,
        mut cmd: Value,
        progress: Option<mpsc::UnboundedSender<Progress>>,
        timeout: Duration,
    ) -> Result<Value, ApiError> {
        let obj = cmd.as_object_mut().ok_or_else(|| ApiError::internal("provider command must be an object"))?;
        let name = match obj.remove("cmd") {
            Some(Value::String(s)) => s,
            _ => return Err(ApiError::internal("provider command needs a \"cmd\"")),
        };
        let on_progress: ProgressFn = match progress {
            Some(tx) => Box::new(move |v: Value| {
                if let Ok(p) = serde_json::from_value::<Progress>(v) {
                    let _ = tx.send(p);
                }
            }),
            None => Box::new(|_| {}),
        };
        // Dropping the future on timeout cancels the work (and kills any
        // yt-dlp child the provider spawned with kill_on_drop).
        match tokio::time::timeout(timeout, self.inner.handle(&name, cmd, on_progress)).await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(message)) => Err(ApiError::Unavailable(message)),
            Err(_) => Err(ApiError::Unavailable("music provider timed out".into())),
        }
    }
}

/// Test double: answers from a closure, records nothing else.
#[cfg(test)]
pub mod mock {
    use super::*;

    type Handler = dyn Fn(&str, Value, &ProgressFn) -> Result<Value, String> + Send + Sync;

    pub struct MockProvider(pub Box<Handler>);

    impl MockProvider {
        pub fn new(f: impl Fn(&str, Value, &ProgressFn) -> Result<Value, String> + Send + Sync + 'static) -> Self {
            Self(Box::new(f))
        }

        /// Fails every command, like a provider with no network.
        pub fn offline() -> Self {
            Self::new(|cmd, _, _| Err(format!("offline: {cmd}")))
        }
    }

    impl Provider for MockProvider {
        fn handle<'a>(&'a self, cmd: &'a str, params: Value, progress: ProgressFn) -> BoxFuture<'a, Result<Value, String>> {
            Box::pin(async move { (self.0)(cmd, params, &progress) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockProvider;
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn request_strips_cmd_and_forwards_progress() {
        let music = Music::new(MockProvider::new(|cmd, params, progress| {
            assert_eq!(cmd, "download");
            assert!(params.get("cmd").is_none());
            assert_eq!(params["video_id"], "abc");
            progress(json!({"percent": 42.5, "speed": "1.2MiB/s"}));
            progress(json!("garbage"));
            Ok(json!({"ok": true}))
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out =
            music.request_with_progress(json!({"cmd": "download", "video_id": "abc"}), Some(tx), Duration::from_secs(5)).await.unwrap();
        assert_eq!(out, json!({"ok": true}));
        let p = rx.try_recv().unwrap();
        assert_eq!(p.percent, 42.5);
        assert_eq!(p.speed.as_deref(), Some("1.2MiB/s"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn errors_and_bad_commands_are_reported() {
        let music = Music::new(MockProvider::new(|_, _, _| Err("Video unavailable".into())));
        let e = music.request(json!({"cmd": "song_info", "video_id": "x"}), Duration::from_secs(5)).await;
        assert!(matches!(e, Err(ApiError::Unavailable(m)) if m == "Video unavailable"));
        assert!(music.request(json!({"video_id": "x"}), Duration::from_secs(5)).await.is_err());
        assert!(music.request(json!("search"), Duration::from_secs(5)).await.is_err());
    }

    struct Slow;
    impl Provider for Slow {
        fn handle<'a>(&'a self, _: &'a str, _: Value, _: ProgressFn) -> BoxFuture<'a, Result<Value, String>> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(Value::Null)
            })
        }
    }

    #[tokio::test]
    async fn timeouts_fail_the_request() {
        let music = Music::new(Slow);
        let e = music.request(json!({"cmd": "ping"}), Duration::from_millis(20)).await;
        assert!(matches!(e, Err(ApiError::Unavailable(m)) if m.contains("timed out")));
    }
}
