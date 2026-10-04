//! Streamable HTTP smoke tests: bearer auth on loopback TCP, 0600 unix socket.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use tokio_util::sync::CancellationToken;

use lindesk_mcp::backend::{Backends, NoDaemon};
use lindesk_mcp::http::{ENDPOINT, bind_unix, router};
use lindesk_mcp::local::HostLocal;

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}"#;

fn backends() -> Backends {
    Backends { daemon: Arc::new(NoDaemon), local: Arc::new(HostLocal::new()) }
}

fn request(host: &str, auth: Option<&str>) -> Request<Full<Bytes>> {
    let mut b = Request::post(ENDPOINT)
        .header("host", host)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(t) = auth {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Full::new(Bytes::from_static(INIT.as_bytes()))).unwrap()
}

async fn send<S>(io: S, req: Request<Full<Bytes>>) -> (u16, String)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await.unwrap();
    tokio::spawn(conn);
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status().as_u16();
    // SSE responses stay open; read the first frame that carries data.
    let mut body = resp.into_body();
    let mut text = String::new();
    while let Some(frame) = tokio::time::timeout(std::time::Duration::from_secs(10), body.frame()).await.unwrap() {
        if let Ok(data) = frame.unwrap().into_data() {
            text.push_str(&String::from_utf8_lossy(&data));
            if text.contains("serverInfo") || status != 200 {
                break;
            }
        }
    }
    (status, text)
}

#[tokio::test]
async fn tcp_requires_the_bearer_token() {
    let token = "0123456789abcdef0123456789abcdef".to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let app = router(backends(), Some(token.clone()), cancel.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let host = addr.to_string();

    let (status, body) = send(tokio::net::TcpStream::connect(addr).await.unwrap(), request(&host, None)).await;
    assert_eq!(status, 401, "{body}");
    let (status, _) = send(tokio::net::TcpStream::connect(addr).await.unwrap(), request(&host, Some("wrong"))).await;
    assert_eq!(status, 401);
    let (status, body) = send(tokio::net::TcpStream::connect(addr).await.unwrap(), request(&host, Some(&token))).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("lindesk-mcp"), "{body}");

    // A browser Origin is refused even with the token.
    let mut req = request(&host, Some(&token));
    req.headers_mut().insert("origin", "http://evil.example".parse().unwrap());
    let (status, _) = send(tokio::net::TcpStream::connect(addr).await.unwrap(), req).await;
    assert_eq!(status, 403);
    cancel.cancel();
}

#[tokio::test]
async fn unix_socket_is_private_and_works() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lindesk/mcp.sock");
    let listener = bind_unix(&path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    // A live socket is never stolen.
    assert!(bind_unix(&path).is_err());

    let cancel = CancellationToken::new();
    let app = router(backends(), None, cancel.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (status, body) = send(tokio::net::UnixStream::connect(&path).await.unwrap(), request("localhost", None)).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("serverInfo"), "{body}");
    cancel.cancel();
}
