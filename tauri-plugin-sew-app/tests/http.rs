//! The HTTPS transport's request shape, against a loopback HTTP server: no Origin, no cookie,
//! no `Sec-Fetch-*`, the bearer only where given, JSON bodies, `Retry-After`, no redirects
//! followed, a capped answer. The byte requests of the API transport, and the gateway socket
//! (the bearer as `Authorization`, the server's close code, a refused upgrade's status).
#![cfg(feature = "http")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use serde_json::json;
use tauri_plugin_sew_app::http::{HttpTransport, MAX_BODY};
use tauri_plugin_sew_app::ws::WsConnector;
use tauri_plugin_sew_app::{
    ByteMethod, ByteRequest, ConnectError, LaneClient, LaneRequest, Method, Origin, Secret,
    SocketConnector, Transport, WsIn, WsOut,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
struct Seen {
    head: String,
    body: Vec<u8>,
}

/// Answers each connection with the next canned answer (raw HTTP), recording the request.
async fn server(answers: Vec<Vec<u8>>) -> (Origin, Arc<Mutex<Vec<Seen>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        for answer in answers {
            let (mut c, _) = l.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let n = c.read(&mut chunk).await.unwrap();
                assert!(n > 0, "the client closed early");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break p + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            while buf.len() < head_end + len {
                let n = c.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            s2.lock().unwrap().push(Seen {
                head,
                body: buf[head_end..head_end + len].to_vec(),
            });
            let _ = c.write_all(&answer).await;
            let _ = c.shutdown().await;
        }
    });
    (
        Origin::parse(&format!("http://127.0.0.1:{port}"), true).unwrap(),
        seen,
    )
}

fn answer(status: &str, extra: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn transport() -> HttpTransport {
    HttpTransport::new("sew-app-test/0", None).unwrap()
}

#[tokio::test]
async fn requests_carry_only_what_the_lane_takes() {
    let (origin, seen) = server(vec![
        answer("200 OK", "", r#"{"ok":true,"lane":1}"#),
        answer("200 OK", "", r#"{"ok":true}"#),
        answer(
            "429 Too Many Requests",
            "Retry-After: 3\r\n",
            r#"{"ok":false,"code":"rate_limited"}"#,
        ),
        answer(
            "302 Found",
            "Location: https://evil.example/steal\r\n",
            "{}",
        ),
    ])
    .await;
    let t = transport();
    let r = t
        .send(
            &origin,
            LaneRequest {
                method: Method::Get,
                path: "/sew/native/info".into(),
                bearer: None,
                body: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    let bearer = Secret::new(format!("sewdv_{}", "a".repeat(43)));
    let r = t
        .send(
            &origin,
            LaneRequest {
                method: Method::Post,
                path: "/sew/native/signout".into(),
                bearer: Some(bearer.clone()),
                body: Some(json!({})),
            },
        )
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    let r = t
        .send(
            &origin,
            LaneRequest {
                method: Method::Post,
                path: "/sew/native/handoff".into(),
                bearer: Some(bearer.clone()),
                body: None,
            },
        )
        .await
        .unwrap();
    assert_eq!((r.status, r.retry_after_ms), (429, Some(3000)));
    let r = t
        .send(
            &origin,
            LaneRequest {
                method: Method::Get,
                path: "/sew/native/whoami".into(),
                bearer: Some(bearer.clone()),
                body: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(r.status, 302, "a redirect is never followed");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4, "the redirect target was never asked");
    for s in &seen {
        let h = s.head.to_ascii_lowercase();
        for banned in ["\norigin:", "\ncookie:", "\nsec-fetch-", "\nreferer:"] {
            assert!(
                !h.contains(banned),
                "a lane request carried {banned}: {}",
                s.head
            );
        }
        assert!(h.contains("\nuser-agent: sew-app-test/0"));
    }
    assert!(seen[0].head.starts_with("GET /sew/native/info "));
    assert!(!seen[0].head.to_ascii_lowercase().contains("authorization"));
    assert!(seen[1].head.starts_with("POST /sew/native/signout "));
    assert!(
        seen[1]
            .head
            .contains(&format!("authorization: Bearer {}", bearer.expose()))
            || seen[1]
                .head
                .contains(&format!("Authorization: Bearer {}", bearer.expose()))
    );
    assert!(
        seen[1]
            .head
            .to_ascii_lowercase()
            .contains("content-type: application/json")
    );
    assert_eq!(seen[1].body, b"{}");
    assert_eq!(seen[2].body, b"{}", "a POST without a body sends {{}}");
}

/// The host's sign-in door over HTTPS: the ping, then the password posted with a
/// Content-Length and no Origin; K2's refusal read with its code, `Retry-After`, and the
/// `changeToken` kept apart from the error.
#[tokio::test]
async fn the_host_door_gets_a_content_length_and_no_origin() {
    let (origin, seen) = server(vec![
        answer("200 OK", "", r#"{"k2":true}"#),
        answer(
            "403 Forbidden",
            "",
            r#"{"ok":false,"changeToken":"k2inv_tok","error":{"code":"password_change_required","hint":"h"}}"#,
        ),
        answer(
            "429 Too Many Requests",
            "Retry-After: 7\r\n",
            r#"{"ok":false,"error":{"code":"rate_limited","retryAfterSecs":7}}"#,
        ),
        answer("200 OK", "", r#"{"ok":true,"code":"k2lgn_abc"}"#),
    ])
    .await;
    let lane = LaneClient::new(Arc::new(transport()));
    assert!(lane.host_door_ping(&origin).await.unwrap());
    let pw = Secret::new("door-test-pw");
    let ch = Secret::new("c".repeat(43));
    let e = lane
        .host_door_sign_in(&origin, "bob", &pw, &ch)
        .await
        .unwrap_err();
    assert_eq!(
        e.change_token.as_ref().map(Secret::expose),
        Some("k2inv_tok")
    );
    let err = e.failure.to_error();
    assert_eq!(err.code, "password_change_required");
    assert!(!serde_json::to_string(&err).unwrap().contains("k2inv_tok"));
    let e = lane
        .host_door_sign_in(&origin, "bob", &pw, &ch)
        .await
        .unwrap_err();
    assert_eq!(e.failure.to_error().code, "rate_limited");
    assert_eq!(e.failure.refusal().unwrap().retry_after_ms, Some(7000));
    let code = lane
        .host_door_sign_in(&origin, "bob", &pw, &ch)
        .await
        .unwrap();
    assert_eq!(code.expose(), "k2lgn_abc");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4);
    assert!(seen[0].head.starts_with("GET /.k2/auth/ping "));
    for s in &seen[1..] {
        assert!(s.head.starts_with("POST /.k2/auth/sign-in "), "{}", s.head);
        let h = s.head.to_ascii_lowercase();
        for banned in [
            "\norigin:",
            "\ncookie:",
            "\nsec-fetch-",
            "\nreferer:",
            "\nauthorization:",
        ] {
            assert!(!h.contains(banned), "the door request carried {banned}");
        }
        assert!(
            h.contains(&format!("\ncontent-length: {}\r", s.body.len())),
            "a Content-Length: {}",
            s.head
        );
        assert!(h.contains("\ncontent-type: application/json"));
        let body: serde_json::Value = serde_json::from_slice(&s.body).unwrap();
        assert_eq!(
            body,
            json!({ "username": "bob", "password": "door-test-pw", "challenge": "c".repeat(43) })
        );
    }
}

#[tokio::test]
async fn an_oversized_answer_and_a_dead_port_are_errors_without_a_body() {
    let big = "x".repeat(MAX_BODY + 10);
    let (origin, _) = server(vec![answer("200 OK", "", &big)]).await;
    let e = transport()
        .send(
            &origin,
            LaneRequest {
                method: Method::Get,
                path: "/sew/native/info".into(),
                bearer: None,
                body: None,
            },
        )
        .await
        .unwrap_err();
    assert!(e.0.contains("too large"), "{e}");
    // Nothing listens on this port any more.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let dead = Origin::parse(&format!("http://127.0.0.1:{port}"), true).unwrap();
    let e = transport()
        .send(
            &dead,
            LaneRequest {
                method: Method::Get,
                path: "/sew/native/info".into(),
                bearer: None,
                body: None,
            },
        )
        .await
        .unwrap_err();
    assert!(e.0.starts_with(&format!("127.0.0.1:{port}:")), "{e}");
}

#[tokio::test]
async fn byte_requests_carry_their_headers_and_keep_the_answers() {
    let (origin, seen) = server(vec![
        b"HTTP/1.1 206 Partial Content\r\nContent-Type: image/png\r\nContent-Range: bytes 0-3/10\r\nSet-Cookie: a=1\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd"
            .to_vec(),
        answer("200 OK", "X-RateLimit-Remaining: 7\r\n", r#"{"ok":true}"#),
    ])
    .await;
    let t = transport();
    let r = t
        .send_bytes(
            &origin,
            ByteRequest {
                method: ByteMethod::Get,
                path: "/sew/native/api/v1/files/9?exp=1&sig=2".into(),
                bearer: Some(Secret::new(format!("sewdv_{}", "a".repeat(43)))),
                headers: vec![
                    ("range".into(), "bytes=0-3".into()),
                    // Never sent, whatever a caller passes.
                    ("cookie".into(), "x=1".into()),
                    ("origin".into(), "https://evil.example".into()),
                    ("sec-fetch-site".into(), "same-origin".into()),
                ],
                body: None,
                max_answer: 1024,
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await
        .unwrap();
    assert_eq!(r.status, 206);
    assert_eq!(r.body, b"abcd");
    assert_eq!(r.header("content-range"), Some("bytes 0-3/10"));
    assert_eq!(
        r.header("set-cookie"),
        Some("a=1"),
        "the transport keeps all; the core filters"
    );
    let up = t
        .send_bytes(
            &origin,
            ByteRequest {
                method: ByteMethod::Post,
                path: "/sew/native/api/v1/uploads/9/data".into(),
                bearer: Some(Secret::new(format!("sewdv_{}", "a".repeat(43)))),
                headers: vec![("content-type".into(), "image/png".into())],
                body: Some(vec![1, 2, 3, 4, 5]),
                max_answer: 1024,
                timeout: std::time::Duration::from_secs(5),
            },
        )
        .await
        .unwrap();
    assert_eq!(up.header("x-ratelimit-remaining"), Some("7"));
    let seen = seen.lock().unwrap().clone();
    let get = seen[0].head.to_ascii_lowercase();
    assert!(get.starts_with("get /sew/native/api/v1/files/9?exp=1&sig=2 http/1.1"));
    assert!(get.contains("range: bytes=0-3"));
    assert!(get.contains(&format!("authorization: bearer sewdv_{}", "a".repeat(43))));
    for h in ["cookie:", "origin:", "sec-fetch"] {
        assert!(!get.contains(h), "{h}");
    }
    let post = seen[1].head.to_ascii_lowercase();
    assert!(post.contains("content-type: image/png"));
    assert!(post.contains("content-length: 5"));
    assert_eq!(seen[1].body, vec![1, 2, 3, 4, 5]);
}

// The handshake callback's `Err` type is tungstenite's own `Response`.
#[allow(clippy::result_large_err)]
#[tokio::test]
async fn the_gateway_socket_carries_the_bearer_and_its_close() {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;

    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let h2 = heads.clone();
    tokio::spawn(async move {
        // The first socket: HELLO, echo one frame, then the server closes 4011.
        let (c, _) = l.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_hdr_async(c, |req: &Request, resp: Response| {
            h2.lock().unwrap().push(format!(
                "{} {:?}",
                req.uri(),
                req.headers()
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", v.to_str().unwrap_or_default()))
                    .collect::<Vec<_>>()
            ));
            Ok(resp)
        })
        .await
        .unwrap();
        ws.send(Message::text(r#"{"op":8}"#)).await.unwrap();
        let got = ws.next().await.unwrap().unwrap();
        ws.send(Message::text(format!("echo {}", got.to_text().unwrap())))
            .await
            .unwrap();
        ws.send(Message::Close(Some(CloseFrame {
            code: 4011u16.into(),
            reason: "session revoked".into(),
        })))
        .await
        .unwrap();
        let _ = ws.next().await;
        // The second: a refused upgrade, the lane's 429.
        let (mut c, _) = l.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = c.read(&mut buf).await.unwrap();
        let body = r#"{"ok":false,"code":"rate_limited","detail":null}"#;
        let _ = c
            .write_all(
                format!(
                    "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 2\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
    });
    let c = WsConnector::new("sew-app-test/0", None).unwrap();
    let url = url::Url::parse(&format!(
        "ws://127.0.0.1:{port}/sew/native/api/v1/gateway?v=1&encoding=json"
    ))
    .unwrap();
    let bearer = Secret::new(format!("sewdv_{}", "b".repeat(43)));
    let mut pipe = c.connect(&url, &bearer).await.unwrap();
    assert_eq!(
        pipe.rx.recv().await.unwrap(),
        WsIn::Text(r#"{"op":8}"#.into())
    );
    pipe.tx.send(WsOut::Text("hi".into())).unwrap();
    assert_eq!(pipe.rx.recv().await.unwrap(), WsIn::Text("echo hi".into()));
    assert_eq!(
        pipe.rx.recv().await.unwrap(),
        WsIn::Closed {
            code: Some(4011),
            reason: "session revoked".into()
        }
    );
    let head = heads.lock().unwrap()[0].to_ascii_lowercase();
    assert!(head.starts_with("/sew/native/api/v1/gateway?v=1&encoding=json"));
    assert!(head.contains(&format!("authorization: bearer sewdv_{}", "b".repeat(43))));
    assert!(head.contains("user-agent: sew-app-test/0"));
    assert!(!head.contains("origin:"));
    assert!(!head.contains("cookie:"));

    match c.connect(&url, &bearer).await {
        Err(ConnectError::Refused {
            status,
            retry_after_ms,
            body,
        }) => {
            assert_eq!((status, retry_after_ms), (429, Some(2000)));
            // The body is what came with the head (the lane sends both at once).
            if !body.is_empty() {
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(v["code"], "rate_limited");
            }
        }
        other => panic!("{other:?}"),
    }
}
