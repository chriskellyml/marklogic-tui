use super::tests::{file, named_file, test_client, viewer};
use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn read_form(stream: &mut tokio::net::TcpStream) -> HashMap<String, String> {
    read_request(stream).await.1
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, HashMap<String, String>) {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        headers.push(stream.read_u8().await.unwrap());
    }
    let length: usize = String::from_utf8_lossy(&headers)
        .lines()
        .find_map(|line| {
            line.to_lowercase()
                .strip_prefix("content-length: ")
                .map(str::to_owned)
        })
        .unwrap()
        .parse()
        .unwrap();
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    (
        String::from_utf8(headers).unwrap(),
        url::form_urlencoded::parse(&body).into_owned().collect(),
    )
}

async fn reply(stream: &mut tokio::net::TcpStream, value: Value) {
    let body = value.to_string();
    stream
        .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
        .await
        .unwrap();
}

#[test]
fn changing_log_authentication_discards_old_collector_clients() {
    for change in ["password", "auth_type", "tls_verification"] {
        let mut logs = viewer();
        let old = test_client();
        let mut server = old.server.clone();
        match change {
            "password" => server.password = "new-secret".into(),
            "auth_type" => server.auth_type = crate::client::AuthType::Basic,
            "tls_verification" => server.insecure = !server.insecure,
            _ => unreachable!(),
        }
        let updated = MarkLogicClient::new(server).unwrap();
        logs.client = Some(old.clone());
        logs.endpoints = vec![Endpoint {
            host: "one".into(),
            client: old,
        }];
        logs.files.push(file("one"));
        logs.begin_activity("Updating logs", 1);
        let old_tx = logs.tx.clone();
        logs.note_service_failure("Log request failed: Appserver Disabled (HTTP 503)");
        logs.reconcile_client(Some(&updated));
        assert!(
            logs.client.is_none(),
            "{change}: the old authenticated client must be retired"
        );
        assert!(logs.endpoints.is_empty());
        assert!(!logs.busy);
        assert!(
            logs.automatic_collection_allowed(),
            "the old connection's failure must not block the new connection"
        );
        assert!(
            old_tx
                .send(CollectorEvent::Done { auto_tail: false })
                .is_err()
        );
        assert_eq!(
            logs.files.len(),
            1,
            "the local catalog must survive credential changes"
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
}

#[test]
fn query_and_resumed_log_download_use_the_updated_credentials() {
    let runtime = Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = runtime.spawn(async move {
        for step in 0..4 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, form) = read_request(&mut stream).await;
            let auth = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization").then_some(value.trim())
            });
            assert_eq!(auth, Some("Basic dGVzdGVyOm5ldy1zZWNyZXQ="), "all requests must use the updated password");
            if step == 0 {
                assert_eq!(form["xquery"], "1");
                let body = "--boundary\r\nContent-Type: text/plain\r\n\r\n1\r\n--boundary--\r\n";
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: multipart/mixed; boundary=boundary\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                continue;
            }
            let response = if step == 1 {
                assert_eq!(form["xquery"], DISCOVER);
                json!({"now":"2026-10-14T09:00:00Z","hosts":["one"]})
            } else {
                assert_eq!(form["xquery"], if step == 2 { INVENTORY } else { COLLECT });
                json!({"host":"one","directory":"/Logs","inventory":false,"read_bytes":4,"cluster_bytes":0,
                    "files":[{"host":"one","path":"/Logs/ErrorLog.txt","name":"ErrorLog.txt","size":4,"modified":"2026-10-14T09:00:00Z"}],
                    "chunks":if step == 2 { json!([]) } else { json!([{"host":"one","path":"/Logs/ErrorLog.txt","start":0,"end":4,"length":4,"reset":true,"full":true,"text":"new\n"}]) }})
            };
            reply(&mut stream, response).await;
        }
    });
    let mut config = test_client().server;
    config.port = port;
    config.auth_type = crate::client::AuthType::Basic;
    let old = MarkLogicClient::new(config.clone()).unwrap();
    config.password = "new-secret".into();
    let updated = MarkLogicClient::new(config).unwrap();
    assert_eq!(
        runtime.block_on(updated.xquery_query("1")).unwrap(),
        vec!["1"]
    );
    let mut logs = viewer();
    logs.identity = format!(
        "{}\0{}\0{}\0",
        old.server.name,
        old.server.base_url(),
        old.server.username
    );
    logs.client = Some(old.clone());
    logs.endpoints = vec![Endpoint {
        host: "one".into(),
        client: old,
    }];
    let mut source = file("one");
    source.size = 4;
    logs.files.push(source.clone());
    logs.commit(source.clone(), true, "old\n", |_| {}).unwrap();
    logs.history_checked = Some(logs.now().date());
    let cache_root = logs.cache_root.clone();
    let old_tx = logs.tx.clone();
    logs.note_service_failure("Log request failed: Appserver Disabled (HTTP 503)");
    // Exercise switching back to Logs directly, even before the main loop's
    // separate reconcile step has run.
    logs.resume(Some(updated), crate::AppMode::Normal, &runtime);
    assert!(
        logs.busy,
        "returning to Logs must reconnect with the working query client's credentials"
    );
    assert!(
        old_tx
            .send(CollectorEvent::Done { auto_tail: false })
            .is_err()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while logs.busy {
        assert!(Instant::now() < deadline);
        logs.tick(&runtime, false);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(logs.cache_root, cache_root);
    assert_eq!(logs.snapshots[&source.key()].text, "old\n");
    logs.download(&runtime);
    while logs.busy {
        assert!(Instant::now() < deadline);
        logs.tick(&runtime, false);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(logs.errors.is_empty(), "{:?}", logs.errors);
    assert_eq!(logs.snapshots[&source.key()].text, "new\n");
    runtime.block_on(server).unwrap();
    logs.cancel();
    fs::remove_dir_all(logs.cache_root).unwrap();
}

#[test]
fn manual_download_rechecks_unverified_and_previously_empty_logs() {
    let mut app = crate::tests::test_app();
    app.mode = crate::AppMode::MarkLogicLogs;
    let listener = app
        .rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = app.rt.spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let form = read_form(&mut stream).await;
            assert_eq!(form["xquery"], COLLECT);
            let vars: Value = serde_json::from_str(&form["vars"]).unwrap();
            let requests: Vec<Value> = serde_json::from_str(vars["requests"].as_str().unwrap()).unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["path"], "/Logs/ErrorLog_1.txt");
            assert_eq!(requests[0]["full"], true);
            reply(&mut stream, json!({
                "host":"one", "directory":"/Logs", "inventory":false,
                "read_bytes":4, "cluster_bytes":0,
                "files":[{"host":"one", "path":"/Logs/ErrorLog_1.txt", "name":"ErrorLog_1.txt", "size":4, "modified":"2026-10-13T12:00:00Z"}],
                "chunks":[{"host":"one", "path":"/Logs/ErrorLog_1.txt", "start":0, "end":4, "length":4, "full":true, "reset":true, "text":"new\n"}]
            })).await;
        }
    });
    let mut logs = viewer();
    let mut source = named_file("one", "ErrorLog_1.txt", "2026-10-13T12:00:00Z");
    source.size = 4;
    logs.files.push(source.clone());
    logs.commit(source.clone(), true, "old\n", |_| {}).unwrap();
    let mut restarted = viewer();
    fs::remove_dir_all(&restarted.cache_root).unwrap();
    restarted.cache_root = logs.cache_root.clone();
    logs = restarted;
    logs.load_cache().unwrap();
    assert!(
        !logs.files[0].present,
        "startup leaves saved metadata unverified"
    );
    let mut client = test_client();
    client.server.port = port;
    client.server.auth_type = crate::client::AuthType::Basic;
    logs.client = Some(client.clone());
    logs.endpoints = vec![Endpoint {
        host: "one".into(),
        client,
    }];
    logs.history_checked = Some(logs.now().date());
    logs.select_grid_cell("----", 1, 0);
    app.server_logs = logs;
    for previously_empty in [false, true] {
        if previously_empty {
            app.server_logs.files[0].present = true;
            app.server_logs.files[0].size = 0;
        }
        crate::events::handle_server_logs_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(
            app.server_logs.busy,
            "a manual download must reach the server: {}",
            app.server_logs.message
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.server_logs.busy {
            assert!(Instant::now() < deadline);
            app.server_logs.tick(&app.rt, true);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(app.server_logs.snapshots[&source.key()].text, "new\n");
        assert_eq!(app.server_logs.snapshots[&source.key()].full_size, Some(4));
    }
    app.rt.block_on(server).unwrap();
    app.server_logs.cancel();
    fs::remove_dir_all(app.server_logs.cache_root).unwrap();
}

#[test]
fn manual_download_discovery_failure_is_shown_without_a_shortcut() {
    let runtime = Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = runtime.spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(read_form(&mut stream).await["xquery"], DISCOVER);
        let body = json!({"errorResponse":{"statusCode":503,"status":"Service Unavailable", "message":"maintenance in progress"}}).to_string();
        stream.write_all(format!("HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let mut logs = viewer();
    let mut saved = file("one");
    saved.present = false;
    logs.files.push(saved);
    let mut client = test_client();
    client.server.port = port;
    client.server.auth_type = crate::client::AuthType::Basic;
    logs.client = Some(client);
    logs.download(&runtime);
    let deadline = Instant::now() + Duration::from_secs(5);
    while logs.busy {
        assert!(Instant::now() < deadline);
        logs.tick(&runtime, false);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(logs.errors_open);
    assert!(
        logs.errors
            .values()
            .any(|error| error.contains("maintenance in progress"))
    );
    assert!(logs.manual_downloads_queued.contains("ErrorLog.txt"));
    runtime.block_on(server).unwrap();
    logs.cancel();
    fs::remove_dir_all(logs.cache_root).unwrap();
}

#[test]
fn local_retained_archives_are_never_sent_as_remote_file_paths() {
    let mut logs = viewer();
    let remote = file("one");
    let mut retained = remote.clone();
    retained.path.push_str("#retained-archive.gz");
    retained.present = false;
    logs.retained_archive_days
        .insert(retained.key(), BTreeSet::from([logs.now().date()]));
    logs.files = vec![remote, retained];
    logs.endpoints = vec![Endpoint {
        host: "one".into(),
        client: test_client(),
    }];
    logs.modes
        .insert("ErrorLog.txt".into(), TrackingMode::OnDemand);
    let requests = logs.on_demand_requests(None);
    assert_eq!(
        requests["one"].len(),
        1,
        "retained paths exist only on local disk"
    );
    logs.modes.insert("ErrorLog.txt".into(), TrackingMode::Live);
    let plans = logs.plan(None);
    assert_eq!(plans[0].timed.len(), 1);
    fs::remove_dir_all(logs.cache_root).unwrap();
}

#[test]
fn automatic_catchup_limits_simultaneous_requests() {
    let runtime = Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let active = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicU64::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (count, max_count, permits) = (active.clone(), peak.clone(), release.clone());
    let server = runtime.spawn(async move {
        let mut jobs = JoinSet::new();
        for _ in 0..6 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (count, max_count, permits) = (count.clone(), max_count.clone(), permits.clone());
            jobs.spawn(async move {
                let form = read_form(&mut stream).await;
                let vars: Value = serde_json::from_str(&form["vars"]).unwrap();
                let current = count.fetch_add(1, Ordering::SeqCst) + 1;
                max_count.fetch_max(current, Ordering::SeqCst);
                let _permit = permits.acquire().await.unwrap();
                let response = if form["xquery"] == SCAN {
                    json!({"host":vars["expected"], "size":0, "modified":"2026-10-14T09:00:00Z", "lines":[]})
                } else {
                    assert_eq!(form["xquery"], COLLECT);
                    json!({"host":vars["expected"], "directory":"/Logs", "inventory":false,
                        "read_bytes":0, "cluster_bytes":0, "files":[], "chunks":[]})
                };
                reply(&mut stream, response).await;
                count.fetch_sub(1, Ordering::SeqCst);
            });
        }
        while let Some(result) = jobs.join_next().await { result.unwrap(); }
    });
    let mut logs = viewer();
    let mut client = test_client();
    client.server.port = port;
    client.server.auth_type = crate::client::AuthType::Basic;
    for (index, host) in ["one", "two", "three", "four", "five", "six"]
        .into_iter()
        .enumerate()
    {
        logs.files.push(if index % 2 == 0 {
            file(host)
        } else {
            named_file(host, "9000_AccessLog.txt", "2026-10-14T09:00:00Z")
        });
        logs.endpoints.push(Endpoint {
            host: host.into(),
            client: client.clone(),
        });
    }
    logs.modes.insert("ErrorLog.txt".into(), TrackingMode::Live);
    logs.modes
        .insert("9000_AccessLog.txt".into(), TrackingMode::Live);
    logs.sync(&runtime, None);
    let deadline = Instant::now() + Duration::from_secs(5);
    while active.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    // The first two responses are blocked while all remaining streams queue.
    runtime.block_on(async { tokio::time::sleep(Duration::from_millis(100)).await });
    let observed_peak = peak.load(Ordering::SeqCst);
    release.add_permits(6);
    while logs.busy {
        assert!(Instant::now() < deadline);
        logs.tick(&runtime, false);
        std::thread::sleep(Duration::from_millis(1));
    }
    runtime.block_on(server).unwrap();
    logs.cancel();
    fs::remove_dir_all(logs.cache_root).unwrap();
    assert!(
        observed_peak <= MAX_CONCURRENT_DOWNLOADS as u64,
        "started {observed_peak} simultaneous requests"
    );
}
