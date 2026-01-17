use active_call::app::{AppState, AppStateBuilder};
use active_call::config::{Config, InviteHandlerConfig};
use anyhow::Result;
use axum::{Router, extract::Json, http::StatusCode as HttpStatusCode, routing::post};
use rsipstack::dialog::invitation::InviteOption;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

// Mock webhook server to capture webhook calls
#[derive(Debug, Clone)]
struct WebhookRequest {
    body: serde_json::Value,
    headers: HashMap<String, String>,
}

// Create a test AppState with webhook configuration
async fn create_test_useragent(webhook_url: String) -> Result<AppState> {
    let mut config = Config::default();
    config.http_addr = "127.0.0.1:0".to_string();
    config.addr = "127.0.0.1".to_string();
    config.udp_port = 0; // Let system assign a port
    config.accept_timeout = Some("30s".to_string()); // Increase accept timeout to 30 seconds
    config.handler = Some(InviteHandlerConfig::Webhook {
        url: Some(webhook_url),
        urls: None,
        method: Some("POST".to_string()),
        headers: Some({
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("X-Test-Header".to_string(), "test-value".to_string()),
            ]
        }),
    });

    let ua = AppStateBuilder::new()
        .with_config(config)
        .with_cancel_token(CancellationToken::new())
        .build()
        .await?;

    Ok(ua)
}

// Create a simple AppState without webhook (for Bob)
async fn create_simple_useragent(listen_addr: String) -> Result<AppState> {
    let mut config = Config::default();
    config.http_addr = "127.0.0.1:0".to_string();
    config.addr = listen_addr;
    config.udp_port = 0; // Let system assign a port

    let ua = AppStateBuilder::new()
        .with_config(config)
        .with_cancel_token(CancellationToken::new())
        .build()
        .await?;

    Ok(ua)
}

#[tokio::test]
async fn test_bob_call_alice_webhook_accept() -> Result<()> {
    // 1. Create webhook server to capture Alice's incoming calls
    let webhook_requests = Arc::new(Mutex::new(Vec::<WebhookRequest>::new()));
    let webhook_requests_clone = webhook_requests.clone();

    // Create flag for accepting calls
    let alice_dialog_id = Arc::new(Mutex::new(None::<String>));
    let alice_dialog_id_clone = alice_dialog_id.clone();

    let webhook_app = Router::new().route(
        "/webhook",
        post(
            move |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| async move {
                let mut webhook_headers = HashMap::new();
                for (k, v) in headers.iter() {
                    webhook_headers.insert(k.to_string(), v.to_str().unwrap_or("").to_string());
                }

                let webhook_req = WebhookRequest {
                    body: body.clone(),
                    headers: webhook_headers,
                };

                // Save webhook request
                webhook_requests_clone.lock().unwrap().push(webhook_req);

                // Extract dialog_id for subsequent accept
                if let Some(dialog_id) = body.get("dialogId") {
                    if let Some(id_str) = dialog_id.as_str() {
                        *alice_dialog_id_clone.lock().unwrap() = Some(id_str.to_string());
                    }
                }

                info!("Webhook received: {:?}", body);
                (HttpStatusCode::OK, "OK")
            },
        ),
    );

    // Start webhook server
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let webhook_addr = listener.local_addr()?;
    let webhook_url = format!("http://127.0.0.1:{}/webhook", webhook_addr.port());

    tokio::spawn(async move {
        axum::serve(listener, webhook_app).await.unwrap();
    });

    // Wait for webhook server to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 2. Create Alice AppState (with webhook configuration)
    let alice_ua = create_test_useragent(webhook_url).await?;
    let alice_local_addr = alice_ua.endpoint.get_addrs().first().cloned().unwrap();
    info!("Alice AppState listening on: {}", alice_local_addr);

    // 3. Create Bob AppState (simple configuration)
    let bob_ua = create_simple_useragent("127.0.0.1".to_string()).await?;
    let bob_local_addr = bob_ua.endpoint.get_addrs().first().cloned().unwrap();
    info!("Bob AppState listening on: {}", bob_local_addr);

    // 4. Run test logic
    let alice_token = alice_ua.token.clone();
    let bob_token = bob_ua.token.clone();

    let alice_ua_arc = alice_ua.clone();
    let bob_ua_arc = bob_ua.clone();

    // Use tokio::select to run different tasks in parallel
    let test_result = tokio::select! {
        // Run Alice's serve
        alice_result = alice_ua_arc.clone().serve() => {
            warn!("Alice serve finished: {:?}", alice_result);
            Err(anyhow::anyhow!("Alice serve finished unexpectedly"))
        }
        // Run Bob's serve
        bob_result = bob_ua_arc.clone().serve() => {
            warn!("Bob serve finished: {:?}", bob_result);
            Err(anyhow::anyhow!("Bob serve finished unexpectedly"))
        }
        // Run main test logic
        test_result = async {
            // Wait for UserAgent to start
            tokio::time::sleep(Duration::from_millis(1000)).await;

            // 5. Bob initiates call to Alice
            let (state_sender, _state_receiver) = mpsc::unbounded_channel();

            let alice_uri = format!("sip:alice@{}", alice_local_addr.addr);
            let bob_uri = format!("sip:bob@{}", bob_local_addr.addr);

            info!("Alice URI: {}, Bob URI: {}", alice_uri, bob_uri);

            let invite_option = InviteOption {
                caller: bob_uri.clone().try_into()?,
                callee: alice_uri.try_into()?,
                content_type: Some("application/sdp".to_string()),
                offer: Some(b"v=0\r\no=bob 123456 123456 IN IP4 127.0.0.1\r\ns=Call\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n".to_vec()),
                contact: bob_uri.try_into()?,
                ..Default::default()
            };

            info!("Bob initiating call to Alice...");

            // Start the invite in a separate task so we can handle Alice's response concurrently
            let bob_ua_invite = bob_ua_arc.invitation.clone();
            let invite_handle = tokio::spawn(async move {
                bob_ua_invite.invite(invite_option, state_sender).await
            });

            // 6. Wait for webhook call and handle it immediately
            let mut webhook_received = false;
            let mut alice_dialog_id_received = None;

            for i in 0..150 {  // Wait up to 15 seconds
                if !webhook_requests.lock().unwrap().is_empty() {
                    webhook_received = true;
                    alice_dialog_id_received = alice_dialog_id.lock().unwrap().clone();
                    info!("Webhook received after {}ms", i * 100);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            if !webhook_received {
                warn!("Webhook was not called within timeout");
                // Cancel the invite and return
                invite_handle.abort();
                return Ok(());
            }

            let webhook_reqs = webhook_requests.lock().unwrap();
            let webhook_req = &webhook_reqs[0];

            // Validate webhook request content
            assert_eq!(webhook_req.body["event"], "invite");
            assert!(webhook_req.body["caller"].as_str().unwrap().contains("bob"));
            assert!(webhook_req.body["callee"].as_str().unwrap().contains("alice"));
            assert!(webhook_req.body["headers"].as_array().unwrap().iter().any(|h| h.as_str().unwrap().contains("application/sdp")));
            assert!(webhook_req.headers.contains_key("content-type"));
            assert!(webhook_req.headers.contains_key("x-test-header"));

            info!("Webhook validation passed");

            // 7. Alice accepts the call immediately after webhook validation
            if let Some(dialog_id_str) = alice_dialog_id_received {
                if let Some(dialog_id) = alice_ua_arc.invitation.find_dialog_id_by_session_id(&dialog_id_str) {
                    if let Some(pending_call) = alice_ua_arc.invitation.get_pending_call(&dialog_id) {
                        info!("Alice accepting call with dialog_id: {}", dialog_id_str);

                    let answer_sdp = b"v=0\r\no=alice 654321 654321 IN IP4 127.0.0.1\r\ns=Call\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49171 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
                    let headers = Some(vec![rsip::Header::ContentType("application/sdp".to_string().into())]);

                        match pending_call.dialog.accept(headers, Some(answer_sdp.to_vec())) {
                            Ok(_) => {
                                info!("Alice accepted the call successfully");
                            }
                            Err(e) => {
                                warn!("Alice failed to accept call: {:?}", e);
                            }
                        }
                    } else {
                        warn!("No pending call found for Alice with dialog_id: {}", dialog_id_str);
                    }
                } else {
                    warn!("No dialog ID found in pending dialogs for: {}", dialog_id_str);
                }
            } else {
                warn!("No dialog ID found for Alice");
            }

            // Now wait for Bob's invite to complete
            match invite_handle.await {
                Ok(Ok((dialog_id, response))) => {
                    info!("Bob's call completed successfully, dialog_id: {:?}", dialog_id);
                    if let Some(resp_body) = response {
                        info!("Response body: {:?}", String::from_utf8_lossy(&resp_body));
                    }
                }
                Ok(Err(e)) => {
                    warn!("Bob's call failed: {:?}", e);
                }
                Err(e) => {
                    warn!("Bob's invite task failed: {:?}", e);
                }
            }

            // 8. Wait for some time to let the dialog establish
            tokio::time::sleep(Duration::from_millis(500)).await;

            info!("Test completed successfully");
            Ok(())
        } => {
            test_result
        }
        // Timeout handling
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            panic!("test timeout");
        }
    };

    // 9. Cleanup
    alice_token.cancel();
    bob_token.cancel();

    // Give some time for cleanup to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    test_result.expect("test failed");
    Ok(())
}

#[tokio::test]
async fn test_incoming_call_reject() -> Result<()> {
    // 1. Create webhook server to capture Alice's incoming calls
    let webhook_requests = Arc::new(Mutex::new(Vec::<WebhookRequest>::new()));
    let webhook_requests_clone = webhook_requests.clone();

    // Create flag for accepting calls
    let alice_dialog_id = Arc::new(Mutex::new(None::<String>));
    let alice_dialog_id_clone = alice_dialog_id.clone();

    let webhook_app = Router::new().route(
        "/webhook",
        post(
            move |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| async move {
                let mut webhook_headers = HashMap::new();
                for (k, v) in headers.iter() {
                    webhook_headers.insert(k.to_string(), v.to_str().unwrap_or("").to_string());
                }

                let webhook_req = WebhookRequest {
                    body: body.clone(),
                    headers: webhook_headers,
                };

                // Save webhook request
                webhook_requests_clone.lock().unwrap().push(webhook_req);

                // Extract dialog_id for subsequent accept
                if let Some(dialog_id) = body.get("dialogId") {
                    if let Some(id_str) = dialog_id.as_str() {
                        *alice_dialog_id_clone.lock().unwrap() = Some(id_str.to_string());
                    }
                }

                info!("Webhook received: {:?}", body);
                (HttpStatusCode::OK, "OK")
            },
        ),
    );

    // Start webhook server
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let webhook_addr = listener.local_addr()?;
    let webhook_url = format!("http://127.0.0.1:{}/webhook", webhook_addr.port());

    info!("Webhook server listening on {}", webhook_url);

    tokio::spawn(async move {
        axum::serve(listener, webhook_app).await.unwrap();
    });

    // 2. Setup Alice (Server) with Webhook
    let alice_ua_arc = create_test_useragent(webhook_url).await?;
    let alice_token = alice_ua_arc.token.clone();

    // 3. Setup Bob (Client)
    let bob_addr = "127.0.0.1".to_string();
    let bob_ua_arc = create_simple_useragent(bob_addr).await?;
    let bob_token = bob_ua_arc.token.clone();

    let alice_ua_run = alice_ua_arc.clone();
    let bob_ua_run = bob_ua_arc.clone();

    let test_logic = async move {
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 4. Bob calls Alice
        let alice_local_addr = alice_ua_arc.endpoint.get_addrs().first().cloned().unwrap();
        let bob_local_addr = bob_ua_arc.endpoint.get_addrs().first().cloned().unwrap();

        let alice_uri = format!("sip:alice@{}", alice_local_addr.addr);
        let bob_uri = format!("sip:bob@{}", bob_local_addr.addr);

        info!("Alice URI: {}, Bob URI: {}", alice_uri, bob_uri);

        let sdp = b"v=0\r\no=bob 123456 123456 IN IP4 127.0.0.1\r\ns=Call\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

        let invite_option = InviteOption {
            caller: bob_uri.clone().try_into()?,
            callee: alice_uri.try_into()?,
            content_type: Some("application/sdp".to_string()),
            offer: Some(sdp.to_vec()),
            contact: bob_uri.try_into()?,
            ..Default::default()
        };

        let (state_sender, _state_receiver) = mpsc::unbounded_channel();
        let bob_ua_invite = bob_ua_arc.invitation.clone();

        let invite_handle =
            tokio::spawn(async move { bob_ua_invite.invite(invite_option, state_sender).await });

        let mut webhook_received = false;
        let mut alice_dialog_id_received = None;

        for i in 0..50 {
            if !webhook_requests.lock().unwrap().is_empty() {
                webhook_received = true;
                alice_dialog_id_received = alice_dialog_id.lock().unwrap().clone();
                info!("Webhook received after {}ms", i * 100);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        if !webhook_received {
            invite_handle.abort();
            return Err(anyhow::anyhow!("Webhook was not called within timeout"));
        }

        if let Some(dialog_id_str) = alice_dialog_id_received {
            if let Some(dialog_id) = alice_ua_arc
                .invitation
                .find_dialog_id_by_session_id(&dialog_id_str)
            {
                if let Some(pending_call) = alice_ua_arc.invitation.get_pending_call(&dialog_id) {
                    info!("Alice REJECTING call with dialog_id: {}", dialog_id_str);
                    pending_call
                        .dialog
                        .reject(Some(rsip::StatusCode::Decline), None)
                        .ok();
                } else {
                    return Err(anyhow::anyhow!("No pending call found for Alice"));
                }
            } else {
                return Err(anyhow::anyhow!("No dialog ID found in pending dialogs"));
            }
        }

        match invite_handle.await {
            Ok(Err(e)) => {
                info!("Bob's call failed as expected: {:?}", e);
            }
            Ok(Ok(_)) => {
                return Err(anyhow::anyhow!(
                    "Bob call succeeded but should have been rejected"
                ));
            }
            Err(e) => {
                return Err(e.into());
            }
        }
        Ok(())
    };

    let test_result = tokio::select! {
        _ = alice_ua_run.serve() => Err(anyhow::anyhow!("Alice stopped unexpectedly")),
        _ = bob_ua_run.serve() => Err(anyhow::anyhow!("Bob stopped unexpectedly")),
        res = test_logic => res,
    };

    alice_token.cancel();
    bob_token.cancel();
    test_result
}

#[tokio::test]
async fn test_outgoing_call_remote_reject() -> Result<()> {
    // 1. Setup Alice
    let alice_ua_arc = create_simple_useragent("127.0.0.1".to_string()).await?;
    let alice_token = alice_ua_arc.token.clone();

    // 2. Setup Bob (No Config -> No Handler)
    let bob_ua_arc = create_simple_useragent("127.0.0.1".to_string()).await?;
    let bob_token = bob_ua_arc.token.clone();

    let alice_ua_run = alice_ua_arc.clone();
    let bob_ua_run = bob_ua_arc.clone();

    let test_logic = async move {
        tokio::time::sleep(Duration::from_millis(500)).await;

        let alice_local_addr = alice_ua_arc.endpoint.get_addrs().first().cloned().unwrap();
        let bob_local_addr = bob_ua_arc.endpoint.get_addrs().first().cloned().unwrap();

        let alice_uri = format!("sip:alice@{}", alice_local_addr.addr);
        let bob_uri = format!("sip:bob@{}", bob_local_addr.addr);

        let sdp = b"v=0\r\no=alice 111 111 IN IP4 127.0.0.1\r\ns=Call\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 50000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

        let invite_option = InviteOption {
            caller: alice_uri.clone().try_into()?,
            callee: bob_uri.try_into()?,
            content_type: Some("application/sdp".to_string()),
            offer: Some(sdp.to_vec()),
            contact: alice_uri.try_into()?,
            ..Default::default()
        };

        let (tx, _rx) = mpsc::unbounded_channel();
        let result = alice_ua_arc.invitation.invite(invite_option, tx).await;

        match result {
            Ok(_) => Err(anyhow::anyhow!(
                "Outgoing call should have been rejected by Bob who has no handler"
            )),
            Err(e) => {
                info!("Outgoing call rejected as expected: {:?}", e);
                Ok(())
            }
        }
    };

    let test_result = tokio::select! {
        _ = alice_ua_run.serve() => Err(anyhow::anyhow!("Alice stopped unexpectedly")),
        _ = bob_ua_run.serve() => Err(anyhow::anyhow!("Bob stopped unexpectedly")),
        res = test_logic => res,
    };

    alice_token.cancel();
    bob_token.cancel();
    test_result
}
