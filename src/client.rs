use crate::llm::ServiceInfo;
use crate::server::types::{ErrorResponse, HealthResponse, TranslateRequest};
use axum::http::StatusCode;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use http_body_util::BodyExt;
use std::path::PathBuf;

/// Endpoint address for the translation daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix(PathBuf),
    Tcp(String, u16),
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(p) => write!(f, "unix:{}", p.display()),
            Self::Tcp(host, port) => write!(f, "http://{host}:{port}"),
        }
    }
}

impl Endpoint {
    /// Parses an endpoint string into Unix socket path or TCP address.
    pub fn parse(s: &str) -> Result<Self, String> {
        let trimmed = s.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return Err("Endpoint cannot be empty".to_string());
        }

        if let Some(path) = trimmed.strip_prefix("unix:") {
            let p = PathBuf::from(path);
            if p.as_os_str().is_empty() {
                return Err("Unix socket path cannot be empty".to_string());
            }
            return Ok(Self::Unix(p));
        }

        if trimmed.starts_with('/') || trimmed.starts_with("./") || trimmed.ends_with(".sock") {
            return Ok(Self::Unix(PathBuf::from(trimmed)));
        }

        let without_proto = trimmed
            .strip_prefix("http://")
            .or_else(|| trimmed.strip_prefix("https://"))
            .unwrap_or(trimmed);

        let (host, port_str) = without_proto.split_once(':').unwrap_or((without_proto, "16742"));
        let port: u16 = port_str
            .parse()
            .map_err(|_| format!("Invalid port in endpoint: '{port_str}'"))?;

        Ok(Self::Tcp(host.to_string(), port))
    }
}

/// Resolved daemon endpoint with optional fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointResolution {
    pub primary: Endpoint,
    pub fallback: Option<Endpoint>,
}

/// Client error types
#[derive(Debug)]
pub enum ClientError {
    /// Failed to connect to the daemon
    DaemonUnreachable {
        endpoint: String,
        source: String,
    },
    /// Daemon returned non-2xx HTTP status and error message
    HttpStatus {
        status: StatusCode,
        message: String,
    },
    /// SSE event stream error
    Stream(String),
    /// HTTP network transport error
    Request(String),
    /// JSON serialization/deserialization error
    Json(serde_json::Error),
}

impl ClientError {
    /// Returns true if the daemon could not be reached (connection error).
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::DaemonUnreachable { .. })
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DaemonUnreachable { endpoint, source } => {
                write!(
                    f,
                    "Error: Could not connect to konyac daemon at {endpoint} ({source}).\n\
                     Is the daemon running?\n\n\
                     To start the daemon:\n  konyac serve\n\
                     Or with systemd:\n  systemctl --user start konyac"
                )
            }
            Self::HttpStatus { status, message } => {
                write!(f, "Daemon returned error ({status}): {message}")
            }
            Self::Stream(msg) => write!(f, "Stream error: {msg}"),
            Self::Request(err) => write!(f, "HTTP error: {err}"),
            Self::Json(err) => write!(f, "JSON error: {err}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Resolves the daemon endpoint, prioritizing Unix Domain Socket when available.
pub fn resolve_endpoint(
    cli_endpoint: Option<&str>,
    server_cfg: &crate::config::RuntimeServerConfig,
) -> Result<EndpointResolution, String> {
    if let Some(ep) = cli_endpoint {
        let primary = Endpoint::parse(ep)?;
        return Ok(EndpointResolution {
            primary,
            fallback: None,
        });
    }

    #[cfg(unix)]
    if let Some(ref unix_path) = server_cfg.unix {
        let primary = Endpoint::Unix(unix_path.clone());
        let fallback = server_cfg
            .tcp
            .as_ref()
            .map(|t| Endpoint::Tcp(t.host.clone(), t.port));
        return Ok(EndpointResolution { primary, fallback });
    }

    if let Some(ref tcp) = server_cfg.tcp {
        return Ok(EndpointResolution {
            primary: Endpoint::Tcp(tcp.host.clone(), tcp.port),
            fallback: None,
        });
    }

    Err("No server endpoint configured (neither unix socket nor TCP enabled)".to_string())
}

enum ClientStream {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    Tcp(tokio::net::TcpStream),
}

macro_rules! delegate_stream {
    ($self:ident, $method:ident $(, $arg:expr)*) => {
        match $self.get_mut() {
            #[cfg(unix)]
            Self::Unix(s) => std::pin::Pin::new(s).$method($($arg),*),
            Self::Tcp(s) => std::pin::Pin::new(s).$method($($arg),*),
        }
    };
}

impl tokio::io::AsyncRead for ClientStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        delegate_stream!(self, poll_read, cx, buf)
    }
}

impl tokio::io::AsyncWrite for ClientStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        delegate_stream!(self, poll_write, cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        delegate_stream!(self, poll_flush, cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        delegate_stream!(self, poll_shutdown, cx)
    }
}

async fn send_request(
    endpoint: &Endpoint,
    method: axum::http::Method,
    uri: &str,
    body: Option<bytes::Bytes>,
) -> Result<hyper::Response<hyper::body::Incoming>, ClientError> {
    let stream = match endpoint {
        #[cfg(unix)]
        Endpoint::Unix(p) => ClientStream::Unix(tokio::net::UnixStream::connect(p).await.map_err(|e| {
            ClientError::DaemonUnreachable {
                endpoint: endpoint.to_string(),
                source: e.to_string(),
            }
        })?),
        #[cfg(not(unix))]
        Endpoint::Unix(_) => {
            return Err(ClientError::DaemonUnreachable {
                endpoint: endpoint.to_string(),
                source: "Unix domain sockets are not supported on this platform".into(),
            });
        }
        Endpoint::Tcp(host, port) => ClientStream::Tcp(
            tokio::net::TcpStream::connect((host.as_str(), *port))
                .await
                .map_err(|e| ClientError::DaemonUnreachable {
                    endpoint: endpoint.to_string(),
                    source: e.to_string(),
                })?,
        ),
    };

    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| ClientError::Request(e.to_string()))?;

    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut builder = hyper::Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::HOST, "localhost");

    let req = if let Some(bytes) = body {
        builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
        builder
            .body(http_body_util::Full::new(bytes))
            .map_err(|e| ClientError::Request(e.to_string()))?
    } else {
        builder
            .body(http_body_util::Full::default())
            .map_err(|e| ClientError::Request(e.to_string()))?
    };

    sender
        .send_request(req)
        .await
        .map_err(|e| ClientError::Request(e.to_string()))
}

async fn extract_error(res: hyper::Response<hyper::body::Incoming>) -> ClientError {
    let status = res.status();
    let bytes = match res.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return ClientError::Request(e.to_string()),
    };
    let message = serde_json::from_slice::<ErrorResponse>(&bytes)
        .map(|e| e.error)
        .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
    ClientError::HttpStatus { status, message }
}

/// Lightweight, stateless hyper-based daemon client.
pub struct KonyacClient {
    primary: Endpoint,
    fallback: Option<Endpoint>,
}

impl KonyacClient {
    /// Creates a client with primary endpoint and optional fallback.
    pub fn new(primary: Endpoint, fallback: Option<Endpoint>) -> Self {
        Self { primary, fallback }
    }

    /// Creates a client resolved from the server configuration and optional CLI endpoint override.
    pub fn from_config(
        server_cfg: &crate::config::ServerConfig,
        cli_endpoint: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let runtime_cfg = server_cfg.clone().into_runtime()?;
        let resolution = resolve_endpoint(cli_endpoint, &runtime_cfg)?;
        Ok(Self::new(resolution.primary, resolution.fallback))
    }

    async fn fetch_json<T: serde::de::DeserializeOwned>(
        ep: &Endpoint,
        path: &str,
    ) -> Result<T, ClientError> {
        let res = send_request(ep, axum::http::Method::GET, path, None).await?;
        if !res.status().is_success() {
            return Err(extract_error(res).await);
        }

        let bytes = res
            .into_body()
            .collect()
            .await
            .map_err(|e| ClientError::Request(e.to_string()))?
            .to_bytes();

        serde_json::from_slice(&bytes).map_err(ClientError::Json)
    }

    async fn request_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, ClientError> {
        match Self::fetch_json(&self.primary, path).await {
            Ok(val) => Ok(val),
            Err(e) if e.is_unreachable() => {
                if let Some(ref fb) = self.fallback {
                    Self::fetch_json(fb, path).await
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Queries GET /health
    pub async fn health(&self) -> Result<HealthResponse, ClientError> {
        self.request_json("/health").await
    }

    /// Queries GET /services
    pub async fn services(&self) -> Result<Vec<ServiceInfo>, ClientError> {
        self.request_json("/services").await
    }

    async fn connect_translate(
        ep: &Endpoint,
        payload: bytes::Bytes,
    ) -> Result<hyper::body::Incoming, ClientError> {
        let res = send_request(ep, axum::http::Method::POST, "/translate", Some(payload)).await?;
        if !res.status().is_success() {
            return Err(extract_error(res).await);
        }
        Ok(res.into_body())
    }

    /// Queries POST /translate for SSE streaming translation
    pub async fn translate<F>(
        &self,
        req: &TranslateRequest,
        mut on_chunk: F,
    ) -> Result<(), ClientError>
    where
        F: FnMut(&str),
    {
        let payload = bytes::Bytes::from(serde_json::to_vec(req).map_err(ClientError::Json)?);

        let body = match Self::connect_translate(&self.primary, payload.clone()).await {
            Ok(b) => b,
            Err(e) if e.is_unreachable() => {
                if let Some(ref fb) = self.fallback {
                    Self::connect_translate(fb, payload).await?
                } else {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        };

        let mut stream = body
            .into_data_stream()
            .map(|chunk_res| chunk_res.map_err(std::io::Error::other))
            .eventsource();

        while let Some(event_res) = stream.next().await {
            let event = event_res.map_err(|e| ClientError::Stream(e.to_string()))?;
            if event.event == "error" {
                return Err(ClientError::Stream(event.data));
            }
            if event.data == "[DONE]" {
                break;
            }
            on_chunk(&event.data);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeServerConfig;
    use crate::config::server::Tcp;
    use axum::Json;
    use axum::Router;
    use axum::response::IntoResponse;
    use axum::response::sse::{Event, Sse};
    use axum::routing::{get, post};
    use std::convert::Infallible;

    #[test]
    fn test_resolve_endpoint() {
        let server_cfg = RuntimeServerConfig {
            tcp: Some(Tcp {
                host: "127.0.0.1".into(),
                port: 16742,
            }),
            unix: Some(PathBuf::from("/tmp/konyac_test.sock")),
        };

        assert_eq!(
            resolve_endpoint(Some("127.0.0.1:18000"), &server_cfg).unwrap(),
            EndpointResolution {
                primary: Endpoint::Tcp("127.0.0.1".into(), 18000),
                fallback: None,
            }
        );
        #[cfg(unix)]
        assert_eq!(
            resolve_endpoint(None, &server_cfg).unwrap(),
            EndpointResolution {
                primary: Endpoint::Unix(PathBuf::from("/tmp/konyac_test.sock")),
                fallback: Some(Endpoint::Tcp("127.0.0.1".into(), 16742)),
            }
        );
    }

    fn mock_app() -> Router {
        Router::new()
            .route(
                "/health",
                get(|| async {
                    Json(HealthResponse {
                        status: "ok".into(),
                        version: "0.1.0".into(),
                        services_count: 1,
                    })
                }),
            )
            .route(
                "/services",
                get(|| async {
                    Json(vec![ServiceInfo {
                        name: "mock".into(),
                        protocol: "openai".into(),
                        configured_models: vec!["m1".into()],
                    }])
                }),
            )
            .route(
                "/translate",
                post(|Json(req): Json<TranslateRequest>| async move {
                    if req.text == "bad" {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(ErrorResponse {
                                error: "bad text".into(),
                            }),
                        )
                            .into_response();
                    }
                    let stream = futures::stream::iter(vec![
                        Ok::<_, Infallible>(Event::default().data("result")),
                        Ok::<_, Infallible>(Event::default().data("[DONE]")),
                    ]);
                    Sse::new(stream).into_response()
                }),
            )
    }

    #[tokio::test]
    async fn test_client_tcp_and_errors() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, mock_app()).await.unwrap();
        });

        let client = KonyacClient::new(Endpoint::Tcp("127.0.0.1".into(), port), None);
        assert_eq!(client.health().await.unwrap().status, "ok");
        assert_eq!(client.services().await.unwrap().len(), 1);

        let mut out = String::new();
        let req = TranslateRequest {
            text: "hi".into(),
            target_lang: "zh".into(),
            prompt: None,
            history: None,
            service: "mock".into(),
            model: "m1".into(),
            json_schema: None,
        };
        client.translate(&req, |c| out.push_str(c)).await.unwrap();
        assert_eq!(out, "result");

        let bad_req = TranslateRequest {
            text: "bad".into(),
            ..req
        };
        assert!(client.translate(&bad_req, |_| {}).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_client_unix_and_fallback() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, mock_app()).await.unwrap();
        });

        let dead_sock =
            std::env::temp_dir().join(format!("konyac_dead_{}.sock", std::process::id()));
        let client = KonyacClient::new(
            Endpoint::Unix(dead_sock),
            Some(Endpoint::Tcp("127.0.0.1".into(), port)),
        );
        assert_eq!(client.health().await.unwrap().status, "ok");

        let live_sock =
            std::env::temp_dir().join(format!("konyac_live_{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&live_sock);
        let u_listener = tokio::net::UnixListener::bind(&live_sock).unwrap();
        tokio::spawn(async move {
            axum::serve(u_listener, mock_app()).await.unwrap();
        });
        let u_client = KonyacClient::new(Endpoint::Unix(live_sock.clone()), None);
        assert_eq!(u_client.health().await.unwrap().status, "ok");
        let _ = std::fs::remove_file(&live_sock);
    }

    #[tokio::test]
    async fn test_client_unreachable() {
        let client = KonyacClient::new(Endpoint::Tcp("127.0.0.1".into(), 59998), None);
        let err = client.health().await.unwrap_err();
        assert!(err.is_unreachable());
    }
}
