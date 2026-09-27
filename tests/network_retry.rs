//! Exercise real disconnects/body failures and capture stderr in a subprocess.
use nova_veil_search::error::NovaVeilSearchError;
use nova_veil_search::providers::http::{
    get_html, get_json_with_header_auth, post_json, post_json_optional_auth,
    post_json_with_header_auth, post_json_with_status, post_raw_json,
};
use reqwest::Client;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
enum Reply {
    Disconnect,
    Timeout,
    Body(&'static str, &'static str, bool),
    Status(u16),
}

const OK: Reply = Reply::Body("application/json", r#"{"ok":true}"#, false);

struct Mock {
    url: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) { self.task.abort(); }
}

async fn mock(replies: Vec<Reply>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/probe?api_key=query-secret", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let task = tokio::spawn(async move {
        for reply in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                let head = String::from_utf8_lossy(&request);
                let length = head.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                }).unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                request.extend_from_slice(&body);
                seen.lock().unwrap().push(request);
                let response = match reply {
                    Reply::Disconnect => return,
                    Reply::Timeout => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        return;
                    }
                    Reply::Body(content_type, body, truncated) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len() + if truncated { 100 } else { 0 }
                    ),
                    Reply::Status(status) => {
                        let body = r#"{"error":"header-secret body-secret query-secret"}"#;
                        format!("HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                    }
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    Mock { url, requests, task }
}

fn client(timeout: Duration) -> Client {
    Client::builder().no_proxy().timeout(timeout).build().unwrap()
}

#[tokio::test]
async fn network_error_retries_three_times_and_replays_the_request() {
    let server = mock(vec![Reply::Disconnect, Reply::Disconnect, Reply::Disconnect, OK]).await;
    let result = post_json(&client(Duration::from_secs(2)), &server.url, "header-secret",
        &json!({"token":"body-secret"}), "RetryProbe").await.unwrap();
    assert_eq!(result["ok"], true);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests.windows(2).all(|pair| pair[0] == pair[1]), "retry must preserve headers and body");
}

#[tokio::test]
async fn network_error_stops_after_four_total_attempts() {
    let server = mock(vec![Reply::Disconnect; 5]).await;
    let failure = post_json_with_status(&client(Duration::from_secs(2)), &server.url, "test-key",
        &json!({}), "RetryProbe").await.err().unwrap();
    assert_eq!(server.requests.lock().unwrap().len(), 4);
    assert_eq!(failure.status, None);
    let detail = failure.error.to_string();
    assert!(detail.contains("after 4 attempts"), "{detail}");
    assert!(detail.contains("connection"), "underlying connection cause missing: {detail}");
    assert!(!detail.contains("query-secret"));
}

#[tokio::test]
async fn timeouts_keep_their_error_class_after_three_retries() {
    let server = mock(vec![Reply::Timeout; 5]).await;
    let error = post_json(&client(Duration::from_millis(80)), &server.url, "test-key",
        &json!({}), "TimeoutProbe").await.unwrap_err();
    assert_eq!(server.requests.lock().unwrap().len(), 4);
    assert!(matches!(error, NovaVeilSearchError::Timeout(_)), "{error}");
}

#[tokio::test]
async fn body_disconnect_retries_and_discards_partial_json_and_sse() {
    let http = client(Duration::from_secs(2));
    for reply in [
        Reply::Body("application/json", r#"{"partial":"unusable"}"#, true),
        Reply::Body("text/event-stream", "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"unusable\"}\n\n", true),
    ] {
        let server = mock(vec![reply, OK]).await;
        let result = post_json(&http, &server.url, "test-key", &json!({}), "BodyProbe").await.unwrap();
        assert_eq!(result, json!({"ok":true}));
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn http_errors_and_invalid_json_are_not_network_retried() {
    let http = client(Duration::from_secs(2));
    for status in [400, 401, 403, 407, 429, 500, 503] {
        let server = mock(vec![Reply::Status(status), OK]).await;
        let failure = post_json_with_status(&http, &server.url, "test-key", &json!({}), "StatusProbe")
            .await.err().unwrap();
        assert_eq!(failure.status, Some(status));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
    let server = mock(vec![Reply::Body("application/json", "invalid", false), OK]).await;
    let error = post_json(&http, &server.url, "test-key", &json!({}), "ParseProbe").await.unwrap_err();
    assert!(matches!(error, NovaVeilSearchError::Parse(_)));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn header_optional_raw_html_and_specialist_requests_share_retries() {
    let http = client(Duration::from_secs(2));
    for helper in 0..7 {
        let server = mock(vec![Reply::Disconnect, OK]).await;
        match helper {
            0 => { post_json_with_header_auth(&http, &server.url, ("x-api-key", "test-key"), &json!({}), "HeaderProbe").await.unwrap(); }
            1 => { post_json_optional_auth(&http, &server.url, None, &json!({}), "OptionalProbe").await.unwrap(); }
            2 => { get_json_with_header_auth(&http, &server.url, &[], ("x-api-key", "test-key"), "GetProbe").await.unwrap(); }
            3 => { post_raw_json(&http, &server.url, &[], &json!({}), "RawProbe").await.unwrap(); }
            4 => { get_html(&http, &server.url, &[], "HtmlProbe").await.unwrap(); }
            5 => { nova_veil_search::sources::get_json(&http, &server.url, &[], "SpecialistProbe").await.unwrap(); }
            _ => { nova_veil_search::sources::get_text(&http, &server.url, &[], "SpecialistProbe").await.unwrap(); }
        }
        assert_eq!(server.requests.lock().unwrap().len(), 2, "helper {helper}");
    }
}

// Run by the log tests in an isolated child process to observe real stderr.
#[test]
fn log_probe_child() {
    let Ok(url) = std::env::var("NOVA_TEST_RETRY_URL") else { return; };
    let success = std::env::var("NOVA_TEST_RETRY_SUCCESS").unwrap() == "true";
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let result = post_json(&client(Duration::from_secs(2)), &url, "header-secret",
            &json!({"token":"body-secret"}), "LogProbe").await;
        assert_eq!(result.is_ok(), success);
    });
}

async fn capture_log(replies: Vec<Reply>, success: bool, count: usize) -> String {
    let server = mock(replies).await;
    let url = server.url.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "log_probe_child", "--nocapture"])
            .env("NOVA_TEST_RETRY_URL", url)
            .env("NOVA_TEST_RETRY_SUCCESS", success.to_string())
            .output().unwrap()
    }).await.unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(server.requests.lock().unwrap().len(), count);
    String::from_utf8(output.stderr).unwrap()
}

#[tokio::test]
async fn success_including_retry_success_is_silent() {
    assert!(capture_log(vec![OK], true, 1).await.is_empty());
    assert!(capture_log(vec![Reply::Disconnect, Reply::Disconnect, Reply::Disconnect, OK], true, 4).await.is_empty());
}

#[tokio::test]
async fn exhausted_failure_logs_once_with_attempt_count_and_without_secrets() {
    let log = capture_log(vec![Reply::Disconnect; 5], false, 4).await;
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.contains("ERROR provider=\"LogProbe\""), "{log}");
    assert!(log.contains("attempts=4 retries=3"), "{log}");
    for secret in ["header-secret", "body-secret", "query-secret"] { assert!(!log.contains(secret), "{log}"); }
    let log = capture_log(vec![Reply::Status(401)], false, 1).await;
    assert!(log.contains("HTTP 401"), "{log}");
    for secret in ["header-secret", "body-secret", "query-secret"] { assert!(!log.contains(secret), "{log}"); }
}
