use crate::api::{ErrorCode, HealthResponse, KonyacError, SSE_DONE, SSE_ERROR, TranslateRequest};
use crate::config::{ServerConfig, tcp_authorization};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{StatusCode, Url};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_TIMEOUT: Duration = Duration::from_secs(135);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Endpoint {
    Unix(PathBuf),
    Tcp(Url),
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
            Self::Tcp(url) => write!(f, "{url}"),
        }
    }
}

impl Endpoint {
    fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if let Some(path) = value.strip_prefix("unix:") {
            if path.is_empty() {
                return Err("Unix socket path cannot be empty".into());
            }
            return Ok(Self::Unix(PathBuf::from(path)));
        }

        let raw = if value.contains("://") {
            value.to_owned()
        } else {
            format!("http://{value}")
        };
        let url = Url::parse(&raw).map_err(|e| format!("Invalid daemon endpoint: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("Daemon endpoint scheme must be http, https, or unix".into());
        }
        if !url.has_host()
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("Daemon endpoint must contain only scheme, host, and optional port".into());
        }
        Ok(Self::Tcp(url))
    }
}

#[derive(Debug)]
pub enum ClientError {
    DaemonUnreachable {
        endpoint: String,
        source: String,
    },
    HttpStatus {
        status: StatusCode,
        error: KonyacError,
    },
    DaemonStream(KonyacError),
    Stream(String),
    Output(std::io::Error),
    Request(reqwest::Error),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DaemonUnreachable { endpoint, source } => write!(
                f,
                "Could not connect to konyac daemon at {endpoint} ({source})."
            ),
            Self::HttpStatus { status, error } => {
                write!(f, "Daemon returned error ({status}): {error}")
            }
            Self::DaemonStream(error) => write!(f, "Stream error: {error}"),
            Self::Stream(message) => write!(f, "Stream error: {message}"),
            Self::Output(err) => write!(f, "Could not write translation output: {err}"),
            Self::Request(err) => write!(f, "HTTP error: {err}"),
        }
    }
}

impl std::error::Error for ClientError {}

fn decode_daemon_error(payload: String) -> KonyacError {
    serde_json::from_str(&payload).unwrap_or_else(|_| KonyacError::new(ErrorCode::Unknown, payload))
}

type ClientInitError = Box<dyn std::error::Error>;

fn resolve_endpoints(server: &ServerConfig) -> Result<(Endpoint, Option<Endpoint>), String> {
    let tcp = server.tcp.as_deref().map(Endpoint::parse).transpose()?;
    if let Some(path) = &server.unix {
        return Ok((Endpoint::Unix(path.clone()), tcp));
    }
    tcp.map(|endpoint| (endpoint, None))
        .ok_or_else(|| "No supported daemon endpoint is configured".into())
}

async fn send_to(
    endpoint: &Endpoint,
    authorization: Option<&HeaderValue>,
    path: &str,
    payload: Option<&TranslateRequest>,
) -> Result<reqwest::Response, ClientError> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(STREAM_TIMEOUT);

    let mut url = match endpoint {
        Endpoint::Unix(socket_path) => {
            builder = builder.unix_socket(socket_path.clone());
            Url::parse("http://localhost").expect("static localhost URL")
        }
        Endpoint::Tcp(base_url) => {
            if let Some(auth) = authorization {
                let mut headers = HeaderMap::new();
                headers.insert(AUTHORIZATION, auth.clone());
                builder = builder.default_headers(headers);
            }
            base_url.clone()
        }
    };
    url.set_path(path);

    let client = builder.build().map_err(ClientError::Request)?;
    let request = match payload {
        Some(payload) => client
            .post(url)
            .header(ACCEPT, "text/event-stream")
            .json(payload),
        None => client.get(url).timeout(CONNECT_TIMEOUT),
    };
    request.send().await.map_err(|e| {
        if e.is_connect() {
            ClientError::DaemonUnreachable {
                endpoint: endpoint.to_string(),
                source: e.to_string(),
            }
        } else {
            ClientError::Request(e)
        }
    })
}

pub struct KonyacClient {
    primary: Endpoint,
    fallback: Option<Endpoint>,
    authorization: Option<HeaderValue>,
}

impl KonyacClient {
    fn new(
        primary: Endpoint,
        fallback: Option<Endpoint>,
        authorization: Option<HeaderValue>,
    ) -> Self {
        Self {
            primary,
            fallback,
            authorization,
        }
    }

    pub fn from_options(
        config_path: Option<&Path>,
        cli_endpoint: Option<&str>,
    ) -> Result<Self, ClientInitError> {
        let (primary, fallback) = match cli_endpoint {
            Some(endpoint) => (Endpoint::parse(endpoint)?, None),
            None => resolve_endpoints(&ServerConfig::load_for_client(config_path)?)?,
        };
        let authorization = if matches!(primary, Endpoint::Tcp(_)) || fallback.is_some() {
            tcp_authorization()?
        } else {
            None
        };
        Ok(Self::new(primary, fallback, authorization))
    }

    async fn request(
        &self,
        path: &str,
        payload: Option<&TranslateRequest>,
    ) -> Result<reqwest::Response, ClientError> {
        let auth = self.authorization.as_ref();
        let response = match send_to(&self.primary, auth, path, payload).await {
            Err(ClientError::DaemonUnreachable { .. }) if let Some(fallback) = &self.fallback => {
                send_to(fallback, auth, path, payload).await?
            }
            result => result?,
        };

        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.map_err(ClientError::Request)?;
        Err(ClientError::HttpStatus {
            status,
            error: decode_daemon_error(body),
        })
    }

    pub async fn health(&self) -> Result<HealthResponse, ClientError> {
        self.request("/health", None)
            .await?
            .json()
            .await
            .map_err(ClientError::Request)
    }

    pub async fn translate(
        &self,
        req: &TranslateRequest,
        mut on_chunk: impl FnMut(&str) -> std::io::Result<()>,
    ) -> Result<(), ClientError> {
        let response = self.request("/translate", Some(req)).await?;
        let mut stream = response.bytes_stream().eventsource();

        while let Some(event) = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
            .await
            .map_err(|_| {
                ClientError::Stream("Timed out waiting for daemon translation data".into())
            })?
        {
            let event = event.map_err(|e| ClientError::Stream(e.to_string()))?;
            match event.event.as_str() {
                SSE_DONE => return Ok(()),
                SSE_ERROR => {
                    return Err(ClientError::DaemonStream(decode_daemon_error(event.data)));
                }
                "message" => on_chunk(&event.data).map_err(ClientError::Output)?,
                _ => {}
            }
        }

        Err(ClientError::Stream(
            "Daemon stream ended before a completion event".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ServiceInfo;
    use axum::Json;
    use axum::Router;
    use axum::response::IntoResponse;
    use axum::response::sse::{Event, Sse};
    use axum::routing::{get, post};
    use std::convert::Infallible;

    #[test]
    fn test_resolve_endpoint() {
        let server_cfg = ServerConfig {
            tcp: Some("127.0.0.1:16742".into()),
            unix: Some(PathBuf::from("/tmp/konyac_test.sock")),
        };

        assert_eq!(
            Endpoint::parse("127.0.0.1:18000").unwrap().to_string(),
            "http://127.0.0.1:18000/"
        );
        assert_eq!(
            resolve_endpoints(&server_cfg).unwrap(),
            (
                Endpoint::Unix(PathBuf::from("/tmp/konyac_test.sock")),
                Some(Endpoint::parse("http://127.0.0.1:16742").unwrap()),
            )
        );
        assert_eq!(
            Endpoint::parse("http://[::1]:16742").unwrap().to_string(),
            "http://[::1]:16742/"
        );
        assert_eq!(
            Endpoint::parse("https://localhost:443")
                .unwrap()
                .to_string(),
            "https://localhost/"
        );
        assert_eq!(
            Endpoint::parse("unix:/tmp/k.sock").unwrap().to_string(),
            "unix:/tmp/k.sock"
        );
        for invalid in [
            "unix:",
            "ftp://localhost",
            "http://localhost/path",
            "http://localhost:invalid",
        ] {
            assert!(Endpoint::parse(invalid).is_err(), "{invalid}");
        }
    }

    fn mock_app() -> Router {
        Router::new()
            .route(
                "/health",
                get(|| async {
                    Json(HealthResponse {
                        status: "ok".into(),
                        version: "0.1.0".into(),
                        target_lang: "zh-CN".into(),
                        services: vec![ServiceInfo {
                            name: "mock".into(),
                            protocol: "openai".into(),
                            models: vec!["m1".into()],
                            thinking: None,
                        }],
                    })
                }),
            )
            .route(
                "/translate",
                post(|Json(req): Json<TranslateRequest>| async move {
                    if req.text == "bad" {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({"error":"bad text"})),
                        )
                            .into_response();
                    }
                    let mut events = vec![Ok::<_, Infallible>(Event::default().data("result"))];
                    match req.text.as_str() {
                        "truncated" => {}
                        "output limit" => events.push(Ok(Event::default()
                            .event(SSE_ERROR)
                            .json_data(KonyacError::new(
                                ErrorCode::OutputTruncated,
                                "model output limit",
                            ))
                            .unwrap())),
                        _ => events.extend([
                            Ok(Event::default().data("[DONE]")),
                            Ok(Event::default().data("end")),
                            Ok(Event::default().event(SSE_DONE).data("[DONE]")),
                        ]),
                    }
                    let stream = futures_util::stream::iter(events);
                    Sse::new(stream).into_response()
                }),
            )
    }

    async fn spawn_mock() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, mock_app()).await.unwrap() });
        addr
    }

    #[tokio::test]
    async fn test_client_tcp_and_errors() {
        let addr = spawn_mock().await;
        let client = KonyacClient::new(Endpoint::parse(&addr.to_string()).unwrap(), None, None);
        let health = client.health().await.unwrap();
        assert_eq!(health.status, "ok");
        assert_eq!(health.target_lang, "zh-CN");
        assert_eq!(health.services.len(), 1);

        let mut out = String::new();
        let req = TranslateRequest {
            text: "hi".into(),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            serde_json::json!({ "text": "hi" })
        );
        client
            .translate(&req, |c| {
                out.push_str(c);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(out, "result[DONE]end");

        let truncated_req = TranslateRequest {
            text: "truncated".into(),
            ..req.clone()
        };
        let truncated = client
            .translate(&truncated_req, |_| Ok(()))
            .await
            .unwrap_err();
        assert!(
            matches!(truncated, ClientError::Stream(error) if error.contains("before a completion event"))
        );

        let output_limit = TranslateRequest {
            text: "output limit".into(),
            ..req.clone()
        };
        out.clear();
        let error = client
            .translate(&output_limit, |chunk| {
                out.push_str(chunk);
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(out, "result");
        assert!(
            matches!(&error, ClientError::DaemonStream(error) if error.code == ErrorCode::OutputTruncated)
        );
        assert_eq!(error.to_string(), "Stream error: model output limit");

        let bad_req = TranslateRequest {
            text: "bad".into(),
            ..req
        };
        let error = client.translate(&bad_req, |_| Ok(())).await.unwrap_err();
        assert!(
            matches!(error, ClientError::HttpStatus { status: StatusCode::BAD_REQUEST, error }
            if error.code == ErrorCode::Unknown && error.message == "bad text")
        );
    }

    #[test]
    fn decodes_legacy_and_future_error_payloads() {
        for payload in [
            "old error",
            r#"{"error":"old error"}"#,
            r#"{"code":"future_daemon_code","error":"old error"}"#,
        ] {
            let error = decode_daemon_error(payload.into());
            assert_eq!(error, KonyacError::new(ErrorCode::Unknown, "old error"));
        }
    }

    #[tokio::test]
    async fn test_client_unix_and_fallback() {
        let addr = spawn_mock().await;

        let dead_sock =
            std::env::temp_dir().join(format!("konyac_dead_{}.sock", std::process::id()));
        let client = KonyacClient::new(
            Endpoint::Unix(dead_sock),
            Some(Endpoint::parse(&addr.to_string()).unwrap()),
            None,
        );
        assert_eq!(client.health().await.unwrap().status, "ok");

        let live_sock =
            std::env::temp_dir().join(format!("konyac_live_{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&live_sock);
        let u_listener = tokio::net::UnixListener::bind(&live_sock).unwrap();
        tokio::spawn(async move {
            axum::serve(u_listener, mock_app()).await.unwrap();
        });
        let u_client = KonyacClient::new(Endpoint::Unix(live_sock.clone()), None, None);
        assert_eq!(u_client.health().await.unwrap().status, "ok");
        let _ = std::fs::remove_file(&live_sock);
    }

    #[tokio::test]
    async fn test_client_unreachable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let endpoint = Endpoint::parse(&format!("127.0.0.1:{port}")).unwrap();
        let client = KonyacClient::new(endpoint, None, None);
        let err = client.health().await.unwrap_err();
        assert!(matches!(err, ClientError::DaemonUnreachable { .. }));
    }
}
