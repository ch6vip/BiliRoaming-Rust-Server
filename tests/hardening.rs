use actix_governor::{Governor, GovernorConfigBuilder, KeyExtractor};
use actix_web::{body::to_bytes, test::TestRequest, web, App, HttpResponse};
use biliroaming_rust_server::mods::{
    background_tasks::update_cached_playurl_background,
    cache::{ep_availability, get_cached_ep_area, get_cached_playurl, update_area_cache},
    config::{prepare_before_start, read_private_key, sslconfig_from_readers},
    handler::{
        blocked_content, handle_api_access_key_request, handle_playurl_request,
        handle_search_request, parse_search_remake, valid_query_sign,
    },
    push::send_report,
    rate_limit::BiliUserToken,
    types::*,
    user_info::resign_user_info,
};
use deadpool_redis::{Config, Pool, Runtime};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

type AppData = (
    Pool,
    BiliConfig,
    Arc<async_channel::Sender<BackgroundTaskType>>,
);

fn config() -> BiliConfig {
    serde_json::from_str(include_str!("../config.example.json")).unwrap()
}

fn app_data(pool: Pool, config: BiliConfig) -> AppData {
    let (tx, _) = async_channel::bounded(4);
    (pool, config, Arc::new(tx))
}

struct MockRedis {
    pool: Pool,
    values: Arc<Mutex<HashMap<String, String>>>,
    commands: Arc<Mutex<Vec<Vec<String>>>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockRedis {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pool = Config::from_url(format!("redis://{}", listener.local_addr().unwrap()))
            .create_pool(Some(Runtime::Tokio1))
            .unwrap();
        let values = Arc::new(Mutex::new(HashMap::<String, String>::new()));
        let commands = Arc::new(Mutex::new(Vec::new()));
        let (store, requests) = (values.clone(), commands.clone());
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (store, requests) = (store.clone(), requests.clone());
                connections.spawn(async move {
                    let mut stream = BufReader::new(stream);
                    loop {
                        let mut line = String::new();
                        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                            break;
                        }
                        let count: usize = line.trim().strip_prefix('*').unwrap().parse().unwrap();
                        let mut args = Vec::new();
                        for _ in 0..count {
                            line.clear();
                            stream.read_line(&mut line).await.unwrap();
                            let len: usize =
                                line.trim().strip_prefix('$').unwrap().parse().unwrap();
                            let mut bytes = vec![0; len + 2];
                            stream.read_exact(&mut bytes).await.unwrap();
                            args.push(String::from_utf8(bytes[..len].to_vec()).unwrap());
                        }
                        requests.lock().unwrap().push(args.clone());
                        let reply = {
                            let mut store = store.lock().unwrap();
                            match args[0].as_str() {
                                "GET" => store
                                    .get(&args[1])
                                    .map(|v| format!("${}\r\n{v}\r\n", v.len()))
                                    .unwrap_or_else(|| "$-1\r\n".into()),
                                "SETEX" => {
                                    store.insert(args[1].clone(), args[3].clone());
                                    "+OK\r\n".into()
                                }
                                "SET" => {
                                    store.insert(args[1].clone(), args[2].clone());
                                    "+OK\r\n".into()
                                }
                                "PING" => "+PONG\r\n".into(),
                                _ => "+OK\r\n".into(),
                            }
                        };
                        if stream.get_mut().write_all(reply.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        Self {
            pool,
            values,
            commands,
            task,
        }
    }
}

impl Drop for MockRedis {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn response_json(response: HttpResponse) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body()).await.unwrap()).unwrap()
}

#[actix_web::test]
async fn accesskey_api_requires_region_enablement_and_nonempty_secret() {
    let redis = MockRedis::new().await;
    redis
        .values
        .lock()
        .unwrap()
        .insert("a11102".into(), "a".repeat(32));
    for (enabled, configured, supplied, area, expected) in [
        (false, "", "", "1", -404),
        (false, "secret", "secret", "1", -404),
        (true, "", "", "1", -412),
        (true, "secret", "wrong", "1", -412),
        (true, "secret", "secret", "2", -404),
        (true, "secret", "secret", "0", -404),
        (true, "secret", "secret", "invalid", -10403),
    ] {
        let mut cfg = config();
        cfg.api_sign = configured.into();
        cfg.api_assesskey_open.insert("1".into(), enabled);
        cfg.resign_from_existed_key = true;
        let req = TestRequest::with_uri(&format!("/api/accesskey?area_num={area}&sign={supplied}"))
            .app_data(app_data(redis.pool.clone(), cfg))
            .to_http_request();
        assert_eq!(
            response_json(handle_api_access_key_request(&req).await).await["code"],
            expected
        );
    }
    assert!(
        redis.commands.lock().unwrap().is_empty(),
        "Rejected requests must not read credentials"
    );
    let mut cfg = config();
    cfg.api_sign = "secret".into();
    cfg.api_assesskey_open.insert("1".into(), true);
    cfg.resign_from_existed_key = true;
    let req = TestRequest::with_uri("/api/accesskey?area_num=1&sign=secret")
        .app_data(app_data(redis.pool.clone(), cfg))
        .to_http_request();
    let response = handle_api_access_key_request(&req).await;
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    assert_eq!(response_json(response).await["access_key"], "a".repeat(32));
}

#[actix_web::test]
async fn malformed_requests_return_json_without_touching_redis() {
    struct Capture;
    static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static LOGGER: Capture = Capture;
    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            LOGS.lock().unwrap().push(record.args().to_string());
        }
        fn flush(&self) {}
    }
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Debug);
    let redis = MockRedis::new().await;
    let req = TestRequest::with_uri("/x/v2/search/type?area=hk")
        .insert_header(("user-agent", "test"))
        .app_data(app_data(redis.pool.clone(), config()))
        .to_http_request();
    assert_ne!(
        response_json(handle_search_request(&req, true, false).await).await["code"],
        0
    );
    let canary = "0123456789abcdef0123456789abcdef";
    for key in [
        "%E4%B8%AD".repeat(11),
        "a".repeat(33),
        "z".repeat(32),
        "short".into(),
        format!("{canary}-suffix"),
    ] {
        let req = TestRequest::with_uri(&format!(
            "/pgc/player/web/playurl?area=hk&ep_id=1&access_key={key}"
        ))
        .insert_header(("user-agent", "test"))
        .app_data(app_data(redis.pool.clone(), config()))
        .to_http_request();
        assert_ne!(
            response_json(handle_playurl_request(&req, false, false).await).await["code"],
            0
        );
    }
    assert!(redis.commands.lock().unwrap().is_empty());
    assert!(
        !LOGS
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(canary)),
        "Malformed credentials must not enter logs"
    );
}

#[test]
fn signature_validation_preserves_valid_requests_and_rejects_bad_boundaries() {
    let unsigned = "area=hk&keyword=%E4%B8%AD";
    let sign = biliroaming_rust_server::calc_md5!(format!("{unsigned}secret"));
    assert!(valid_query_sign(
        &format!("{unsigned}&sign={sign}"),
        "secret"
    ));
    for query in [
        "",
        "area=hk",
        "中",
        "area=hk&sign=short",
        "area=hk&sign=x&sign=y",
    ] {
        assert!(!valid_query_sign(query, "secret"));
    }
}

#[actix_web::test]
async fn rate_limit_cannot_be_reset_with_token_header_or_source_port() {
    let redis = MockRedis::new().await;
    let data = app_data(redis.pool.clone(), config());
    let governor = GovernorConfigBuilder::default()
        .seconds_per_request(60)
        .burst_size(1)
        .key_extractor(BiliUserToken)
        .finish()
        .unwrap();
    let app = actix_web::test::init_service(
        App::new()
            .app_data(data)
            .wrap(Governor::new(&governor))
            .default_service(web::to(|| async { HttpResponse::Ok().finish() })),
    )
    .await;
    for (index, query) in ["?access_key=one", "?access_key=two"].iter().enumerate() {
        let req = TestRequest::with_uri(&format!("/{query}"))
            .peer_addr(format!("192.0.2.1:{}", 1000 + index).parse().unwrap())
            .insert_header(("X-Real-IP", format!("198.51.100.{}", index + 1)))
            .to_request();
        let response = actix_web::test::call_service(&app, req).await;
        assert_eq!(
            response.status().as_u16(),
            if index == 0 { 200 } else { 429 }
        );
    }
    let mut cfg = config();
    cfg.trusted_proxies.push("127.0.0.1".parse().unwrap());
    let req = TestRequest::default()
        .peer_addr("127.0.0.1:3000".parse().unwrap())
        .insert_header(("X-Real-IP", "192.0.2.55"))
        .app_data(app_data(redis.pool.clone(), cfg))
        .to_srv_request();
    assert_eq!(BiliUserToken.extract(&req).unwrap(), "192.0.2.55");
}

#[actix_web::test]
async fn redis_failure_is_a_cache_miss_and_writes_are_best_effort() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let pool = Config::from_url(format!("redis://{addr}"))
        .create_pool(Some(Runtime::Tokio1))
        .unwrap();
    let (pool, cfg, sender) = app_data(pool, config());
    let runtime = BiliRuntime::new(&cfg, &pool, &sender);
    assert!(runtime.redis_get("missing").await.is_none());
    runtime.redis_set("key", "value", 1).await;
    runtime
        .update_cache(&CacheType::ThSeason("1"), "data", 1)
        .await;
}

#[actix_web::test]
async fn modern_membership_errors_do_not_poison_region_cache() {
    let redis = MockRedis::new().await;
    let (pool, cfg, sender) = app_data(redis.pool.clone(), config());
    let runtime = BiliRuntime::new(&cfg, &pool, &sender);
    let params = PlayurlParams {
        ep_id: "42",
        area_num: 2,
        ..Default::default()
    };
    update_area_cache(&json!({"code":6002105}), &params, &runtime).await;
    assert_eq!(redis.values.lock().unwrap().get("e421402").unwrap(), "2022");
    assert!(redis
        .commands
        .lock()
        .unwrap()
        .iter()
        .any(|c| c[0] == "SETEX" && c[2] == "3600"));
    update_area_cache(&json!({"code":-412}), &params, &runtime).await;
    assert_eq!(redis.values.lock().unwrap().get("e421402").unwrap(), "2022");
    assert_eq!(ep_availability(&json!({"code":6002003})), Some(false));
    assert_eq!(ep_availability(&json!({})), None);
    assert!(matches!(
        get_cached_ep_area(&params, &runtime).await,
        Ok(Some(Area::Hk))
    ));
    use biliroaming_rust_server::mods::background_tasks::status_area_availability;
    assert_eq!(
        status_area_availability(&json!({"code":0,"result":{"area_limit":0}})),
        Some(true)
    );
    assert_eq!(
        status_area_availability(&json!({"code":0,"result":{"area_limit":1}})),
        Some(false)
    );
    assert_eq!(status_area_availability(&json!({"code":0})), None);
    assert_eq!(status_area_availability(&json!({"code":-412})), None);
}

#[actix_web::test]
async fn corrupt_playurl_cache_is_ignored() {
    let redis = MockRedis::new().await;
    let (pool, cfg, sender) = app_data(redis.pool.clone(), config());
    let runtime = BiliRuntime::new(&cfg, &pool, &sender);
    let params = PlayurlParams {
        ep_id: "corrupt",
        ..Default::default()
    };
    let key = CacheType::Playurl(&params).gen_key().remove(0);
    for value in ["", "short", "not-a-timestamp", "中中中中中"] {
        redis
            .values
            .lock()
            .unwrap()
            .insert(key.clone(), value.into());
        assert!(get_cached_playurl(&params, &runtime).await.is_err());
    }
}

#[actix_web::test]
async fn refresh_deduplication_releases_when_queue_drops_task() {
    let redis = MockRedis::new().await;
    let cfg = config();
    let (tx, rx) = async_channel::bounded(1);
    let tx = Arc::new(tx);
    let runtime = BiliRuntime::new(&cfg, &redis.pool, &tx);
    let params = PlayurlParams {
        ep_id: "dedup-test",
        ..Default::default()
    };
    update_cached_playurl_background(&params, &runtime).await;
    update_cached_playurl_background(&params, &runtime).await;
    assert_eq!(rx.len(), 1);
    let task = rx.recv().await.unwrap();
    update_cached_playurl_background(&params, &runtime).await;
    assert!(rx.is_empty());
    drop(task);
    update_cached_playurl_background(&params, &runtime).await;
    assert_eq!(rx.len(), 1);
    drop(rx.recv().await.unwrap());
    tx.try_send(BackgroundTaskType::Health(HealthTask::HealthCheck))
        .ok()
        .unwrap();
    update_cached_playurl_background(&params, &runtime).await;
    drop(rx.recv().await.unwrap());
    update_cached_playurl_background(&params, &runtime).await;
    assert_eq!(rx.len(), 1, "Full queue must release the dedup guard");
}

#[actix_web::test]
async fn retained_config_credentials_do_not_overwrite_refreshed_tokens() {
    let redis = MockRedis::new().await;
    let mut cfg = config();
    cfg.cn_resign_info.access_key = "old-token".into();
    cfg.cn_resign_info.refresh_token = "old-refresh".into();
    let serialized = serde_json::to_string(&cfg).unwrap();
    let cfg: BiliConfig = serde_json::from_str(&serialized).unwrap();
    assert_eq!(cfg.cn_resign_info.refresh_token, "old-refresh");
    redis
        .values
        .lock()
        .unwrap()
        .insert("a11101".into(), "refreshed-token-data".into());
    let (pool, cfg, sender) = app_data(redis.pool.clone(), cfg);
    prepare_before_start(BiliRuntime::new(&cfg, &pool, &sender)).await;
    assert_eq!(
        redis.values.lock().unwrap().get("a11101").unwrap(),
        "refreshed-token-data"
    );
}

#[test]
fn pem_reader_supports_all_key_containers_and_errors_without_a_key() {
    for label in ["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"] {
        let pem = format!("-----BEGIN {label}-----\nAQID\n-----END {label}-----\n");
        assert_eq!(
            read_private_key(&mut pem.as_bytes()).unwrap().secret_der(),
            vec![1, 2, 3]
        );
    }
    assert!(read_private_key(&mut &b"not a key"[..]).is_err());
    assert!(sslconfig_from_readers(&mut &b""[..], &mut &b""[..]).is_err());
}

/// Serve exactly one request on a temporary listener and return the raw request text.
///
/// The response is flushed and the write half is shut down before the socket is dropped.
/// Dropping a socket that still has unread inbound bytes makes the OS send RST instead of
/// FIN, which surfaces to the client as a spurious connection error (the previous version
/// closed the socket right after `write_all`, which occasionally failed the caller).
async fn mock_http(body: String) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut request = String::new();
        let mut length = 0;
        let mut chunked = false;
        loop {
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            request.push_str(&line);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                let value = value.trim();
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.parse().unwrap_or(0);
                } else if name.eq_ignore_ascii_case("transfer-encoding")
                    && value.eq_ignore_ascii_case("chunked")
                {
                    chunked = true;
                }
            }
        }
        if chunked {
            loop {
                let mut size_line = String::new();
                stream.read_line(&mut size_line).await.unwrap();
                request.push_str(&size_line);
                let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
                if size == 0 {
                    // Consume the trailing CRLF after the last chunk.
                    let mut trailer = String::new();
                    stream.read_line(&mut trailer).await.unwrap();
                    request.push_str(&trailer);
                    break;
                }
                let mut chunk = vec![0; size + 2];
                stream.read_exact(&mut chunk).await.unwrap();
                request.push_str(&String::from_utf8_lossy(&chunk));
            }
        } else {
            let mut bytes = vec![0; length];
            stream.read_exact(&mut bytes).await.unwrap();
            request.push_str(&String::from_utf8_lossy(&bytes));
        }
        stream
            .get_mut()
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.get_mut().flush().await.unwrap();
        // Signal EOF explicitly, then drain anything the client still had in flight so the
        // socket is never closed with unread data.
        stream.get_mut().shutdown().await.unwrap();
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest).await;
        request
    });
    (url, task)
}

#[actix_web::test]
async fn custom_notification_posts_configured_body() {
    let redis = MockRedis::new().await;
    let (url, request) = mock_http("{}".into()).await;
    let mut report: ReportConfig = serde_json::from_value(json!({"Custom": {
        "method":"Post", "url":url, "content":"text=expected-content", "proxy_open":false, "proxy_url":""
    }})).unwrap();
    report.init().unwrap();
    send_report(
        &redis.pool,
        &report,
        &HealthReportType::Others(HealthData::default()),
    )
    .await
    .unwrap();
    let request = request.await.unwrap();
    assert!(request.starts_with("POST / HTTP/1.1"));
    assert!(request.ends_with("text=expected-content"), "{request}");
}

#[actix_web::test]
async fn resign_uses_requested_region_api() {
    let redis = MockRedis::new().await;
    let token = "b".repeat(32);
    let (url, request) =
        mock_http(json!({"code":0,"access_key":token,"expire_time":4102444800u64}).to_string())
            .await;
    let mut cfg = config();
    cfg.resign_open.insert("2".into(), true);
    cfg.resign_from_api_open.insert("2".into(), true);
    cfg.resign_api.insert("2".into(), url);
    cfg.resign_api_sign.insert("2".into(), "test-secret".into());
    let user = UserInfo::new(0, &token, 123, 4102444800000);
    let key = CacheType::UserInfo(&token, 123).gen_key().remove(0);
    redis.values.lock().unwrap().insert(key, user.to_json());
    let (pool, cfg, sender) = app_data(redis.pool.clone(), cfg);
    let runtime = BiliRuntime::new(&cfg, &pool, &sender);
    let mut params = PlayurlParams {
        area_num: 2,
        area: "hk",
        access_key: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ..Default::default()
    };
    let result = resign_user_info(true, &mut params, &runtime)
        .await
        .ok()
        .flatten()
        .unwrap();
    assert_eq!(result, (true, token));
    assert!(request
        .await
        .unwrap()
        .starts_with("GET /?area_num=2&sign=test-secret HTTP/1.1"));
}

#[actix_web::test]
async fn background_dispatch_bounds_concurrency_and_drains_after_a_panic() {
    use biliroaming_rust_server::mods::background_tasks::run_background_queue;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (tx, rx) = async_channel::bounded(32);
    for _ in 0..24 {
        tx.send(BackgroundTaskType::Health(HealthTask::HealthCheck))
            .await
            .ok()
            .unwrap();
    }
    drop(tx);
    let started = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (counter, done, gate) = (started.clone(), completed.clone(), release.clone());
    let runner = tokio::spawn(async move {
        run_background_queue(&rx, move |_| {
            let (counter, done, gate) = (counter.clone(), done.clone(), gate.clone());
            async move {
                let index = counter.fetch_add(1, Ordering::SeqCst);
                let permit = gate.acquire().await.unwrap();
                permit.forget();
                if index == 0 {
                    panic!("simulated background failure");
                }
                done.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while started.load(Ordering::SeqCst) < 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(started.load(Ordering::SeqCst), 8);
    release.add_permits(24);
    tokio::time::timeout(std::time::Duration::from_secs(3), runner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed.load(Ordering::SeqCst), 23);
}

/// A malformed `appsearch_remake` entry used to panic the worker via
/// `serde_json::from_str(..).unwrap()`. It must now report an error so the caller can
/// fall back to the untouched upstream response.
#[test]
fn malformed_search_remake_entry_is_reported_not_panicking() {
    let mut config = config();
    config
        .appsearch_remake
        .insert("bad".into(), "not valid json".into());
    config
        .appsearch_remake
        .insert("good".into(), "{\"title\":\"ok\"}".into());

    // Valid entry parses.
    let parsed = parse_search_remake(&config, true, "good").unwrap().unwrap();
    assert_eq!(parsed["title"], "ok");
    // Malformed entry surfaces an error instead of panicking.
    let error = parse_search_remake(&config, true, "bad").unwrap_err();
    assert!(error.contains("invalid appsearch_remake entry"), "{error}");
    // Unknown host is simply "not configured".
    assert!(parse_search_remake(&config, true, "absent")
        .unwrap()
        .is_none());
    // The web map is a separate namespace.
    assert!(parse_search_remake(&config, false, "bad")
        .unwrap()
        .is_none());
}

#[test]
fn content_blocklist_matches_configured_ids() {
    let mut config = config();
    config.block_bangumi_ep = vec![778998];
    config.block_bangumi_cid = vec![3629601];
    config.block_bangumi_avid = vec![928861104];
    config.block_bangumi_bvid = vec!["BV1Wz4y1t7g4".into()];

    let hit = |query: &str| blocked_content(&qstring::QString::from(query), &config);
    assert_eq!(hit("ep_id=778998").as_deref(), Some("ep_id=778998"));
    assert_eq!(hit("cid=3629601").as_deref(), Some("cid=3629601"));
    assert_eq!(hit("avid=928861104").as_deref(), Some("avid=928861104"));
    assert_eq!(
        hit("bvid=BV1Wz4y1t7g4").as_deref(),
        Some("bvid=BV1Wz4y1t7g4")
    );
    // Unlisted ids and malformed values pass through.
    assert!(hit("ep_id=1").is_none());
    assert!(hit("ep_id=not-a-number").is_none());
    assert!(hit("bvid=").is_none());
    assert!(hit("keyword=test").is_none());
}

/// Static refusals (malformed request, bad signature, stale client) may be cached briefly so a
/// client retry storm does not reach this process or the upstream API. Refusals that depend on
/// operator-controlled state must never be cached, or removing a blocklist entry would keep
/// rejecting for the cache lifetime.
#[actix_web::test]
async fn static_refusals_are_cacheable_but_state_dependent_ones_are_not() {
    fn cache_control(response: &HttpResponse) -> Option<String> {
        response
            .headers()
            .get("Cache-Control")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    let redis = MockRedis::new().await;

    // Malformed request: rejected before any state is consulted, so it is cacheable.
    let req = TestRequest::with_uri("/x/v2/search/type?area=hk")
        .insert_header(("user-agent", "test"))
        .app_data(app_data(redis.pool.clone(), config()))
        .to_http_request();
    let malformed = handle_search_request(&req, true, false).await;
    assert_eq!(
        cache_control(&malformed).as_deref(),
        Some("public, max-age=30")
    );

    // Content blocklist hit: depends on config, so it must stay uncached.
    // Use the web path: it skips the app-only signature gate, so the request reaches the
    // blocklist check instead of being refused earlier as an unsigned app request.
    let mut blocked_config = config();
    blocked_config.block_bangumi_ep = vec![778998];
    blocked_config.limit_biliroaming_version_open = false;
    let req = TestRequest::with_uri(
        "/pgc/player/web/playurl?area=hk&ep_id=778998&access_key=0123456789abcdef0123456789abcdef",
    )
    .insert_header(("user-agent", "test"))
    .app_data(app_data(redis.pool.clone(), blocked_config))
    .to_http_request();
    let blocked = handle_playurl_request(&req, false, false).await;
    assert_eq!(blocked.status(), actix_web::http::StatusCode::OK);
    assert_eq!(cache_control(&blocked), None);
    assert_eq!(response_json(blocked).await["code"], -10403);

    // Credential responses must stay no-store: they carry a live access key.
    let mut credential_config = config();
    credential_config.api_sign = "secret".into();
    credential_config
        .api_assesskey_open
        .insert("1".into(), true);
    credential_config.resign_from_existed_key = true;
    redis
        .values
        .lock()
        .unwrap()
        .insert("a11102".into(), "a".repeat(32));
    let req = TestRequest::with_uri("/api/accesskey?area_num=1&sign=secret")
        .app_data(app_data(redis.pool.clone(), credential_config))
        .to_http_request();
    let credential = handle_api_access_key_request(&req).await;
    assert_eq!(cache_control(&credential).as_deref(), Some("no-store"));
}
