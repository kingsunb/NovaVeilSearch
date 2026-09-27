//! Verify the actual RFC 1929 authentication bytes, not just the resolved URL.
use nova_veil_search::providers::http::{account_alias, HttpClients};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn socks_server(
    authenticated: bool,
) -> (String, tokio::task::JoinHandle<(String, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await.unwrap().unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 5);
        let count = stream.read_u8().await.unwrap();
        let mut methods = vec![0; count as usize];
        stream.read_exact(&mut methods).await.unwrap();
        let (username, password) = if authenticated {
            assert!(methods.contains(&2), "client must offer username/password authentication");
            stream.write_all(&[5, 2]).await.unwrap();
            assert_eq!(stream.read_u8().await.unwrap(), 1);
            let len = stream.read_u8().await.unwrap();
            let mut username = vec![0; len as usize];
            stream.read_exact(&mut username).await.unwrap();
            let len = stream.read_u8().await.unwrap();
            let mut password = vec![0; len as usize];
            stream.read_exact(&mut password).await.unwrap();
            stream.write_all(&[1, 0]).await.unwrap();
            (String::from_utf8(username).unwrap(), String::from_utf8(password).unwrap())
        } else {
            assert!(methods.contains(&0));
            stream.write_all(&[5, 0]).await.unwrap();
            (String::new(), String::new())
        };
        let mut connect = [0; 4];
        stream.read_exact(&mut connect).await.unwrap();
        assert_eq!(&connect[..3], &[5, 1, 0]);
        let len = match connect[3] {
            1 => 4,
            3 => stream.read_u8().await.unwrap() as usize,
            4 => 16,
            other => panic!("unexpected SOCKS address type {other}"),
        };
        let mut target = vec![0; len + 2];
        stream.read_exact(&mut target).await.unwrap();
        stream.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80]).await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await.unwrap();
        (username, password)
    });
    (address, handle)
}

async fn check_proxy(scheme: &str, placeholder: &str, restricted: bool) {
    let mut aliases = Vec::new();
    for provider in ["tavily", "exa", "tinyfish", "firecrawl", "grok"] {
        let (address, server) = socks_server(true).await;
        let template = format!("{scheme}://Default%2540.{placeholder}:p%40ss%2540@{address}");
        let keys = [("tavily", Some("test-key")), ("exa", Some("test-key")),
            ("tinyfish", Some("test-key")), ("firecrawl", Some("test-key"))];
        let timeout = Duration::from_secs(3);
        let clients = if restricted {
            #[cfg(feature = "http")]
            { HttpClients::build_restricted(timeout, Some(&template), None, Some(&template), &keys, Some("test-key")) }
            #[cfg(not(feature = "http"))]
            { panic!("restricted build requires the http feature") }
        } else {
            HttpClients::build(timeout, Some(&template), None, Some(&template), &keys, Some("test-key"))
        };
        let client = if provider == "grok" {
            &clients.grok
        } else {
            clients.keyed_client_for(provider, Some("test-key"))
        };
        // The mock tunnel responds itself; no public network is contacted.
        let text = client.get("http://203.0.113.10/probe").send().await.unwrap()
            .text().await.unwrap();
        assert_eq!(text, "ok");
        let (username, password) = server.await.unwrap();
        assert_eq!(username, format!("Default%40.{}", account_alias(provider, "test-key")));
        assert_eq!(password, "p@ss%40");
        assert!(!aliases.contains(&username), "different providers need distinct aliases");
        aliases.push(username);
    }
}

#[tokio::test]
async fn socks5_templates_send_resolved_credentials() {
    for scheme in ["socks5", "socks5h"] {
        for placeholder in ["{account}", "%7Baccount%7D", "%7baccount%7d"] {
            check_proxy(scheme, placeholder, false).await;
        }
    }
}

#[cfg(feature = "http")]
#[tokio::test]
async fn docker_clients_send_resolved_credentials() {
    check_proxy("socks5h", "{account}", true).await;
    check_proxy("socks5", "%7Baccount%7D", true).await;
}

#[tokio::test]
async fn keyless_fixed_socks_proxy_keeps_anonymous_authentication() {
    let (address, server) = socks_server(false).await;
    let fixed = format!("socks5://{address}");
    let clients = HttpClients::build(Duration::from_secs(3), None, Some(&fixed), None, &[], None);
    let response = clients.keyless.get("http://203.0.113.10/probe").send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(server.await.unwrap(), (String::new(), String::new()));
}

#[tokio::test]
async fn each_key_of_a_keyring_routes_through_its_own_proxy_account() {
    // A comma-separated key list yields one proxy account per key: a request
    // signed with key K sends `Default.<alias(K)>`, whichever key that is.
    for key in ["test-key", "other-key"] {
        let (address, server) = socks_server(true).await;
        let template = format!("socks5h://Default.{{account}}:p%40ss@{address}");
        let keys = [("tavily", Some("test-key,other-key"))];
        let clients = HttpClients::build(Duration::from_secs(3), Some(&template), None, None, &keys, None);
        let client = clients.keyed_client_for("tavily", Some(key));
        let text = client.get("http://203.0.113.10/probe").send().await.unwrap()
            .text().await.unwrap();
        assert_eq!(text, "ok");
        let (username, password) = server.await.unwrap();
        assert_eq!(username, format!("Default.{}", account_alias("tavily", key)));
        assert_eq!(password, "p@ss");
    }
}
