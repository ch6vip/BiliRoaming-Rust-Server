use actix_files::Files;
use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::http::header::ContentType;
use actix_web::{get, middleware, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use async_channel::{Receiver, Sender};
use biliroaming_rust_server::mods::background_tasks::*;
use biliroaming_rust_server::mods::config::{init_biliconfig, prepare_before_start};
use biliroaming_rust_server::mods::config::{load_sslconfig, update_biliconfig};
use biliroaming_rust_server::mods::handler::{
    errorurl_reg, handle_api_access_key_request, handle_cn_season_request, handle_playurl_request,
    handle_search_request, handle_th_season_request, handle_th_subtitle_request,
};
use biliroaming_rust_server::mods::middleware::compress::ChangeCompressPriority;
use biliroaming_rust_server::mods::rate_limit::BiliUserToken;
use biliroaming_rust_server::mods::types::{BackgroundTaskType, BiliConfig, BiliRuntime};
use deadpool_redis::{Config, Pool, Runtime};
use futures::join;
use lazy_static::lazy_static;
use log::{error, info};
use std::sync::Arc;
use std::time::Duration;

#[get("/")]
async fn hello() -> impl Responder {
    match tokio::fs::read_to_string("./web/index.html").await {
        Ok(value) => {
            return HttpResponse::Ok()
                .content_type(ContentType::html())
                .body(value);
        }
        Err(_) => {
            return HttpResponse::Ok()
                .content_type(ContentType::html())
                .body(r#"<html><head><meta charset="utf-8"><title>200 OK</title></head><body><div style="margin:0px auto;text-align:center;"><h1>BiliRoaming-Rust-Server</h1><p>[online] 200 OK</p><br>Powered by <a href="https://github.com/pchpub/BiliRoaming-Rust-Server">BiliRoaming-Rust-Server</a></div></body></html>"#)
        }
    }
}

async fn web_default(req: HttpRequest) -> impl Responder {
    let path = format!("{}", req.path());
    let res_type = if let Some(value) = errorurl_reg(&path).await {
        value
    } else {
        return HttpResponse::Ok()
            .content_type(ContentType::json())
            .insert_header(("From", "biliroaming-rust-server"))
            .insert_header(("Access-Control-Allow-Origin", "https://www.bilibili.com"))
            .insert_header(("Access-Control-Allow-Credentials", "true"))
            .insert_header(("Access-Control-Allow-Methods", "GET"))
            .body("{\"code\":-404,\"message\":\"请检查填入的服务器地址是否有效\"}");
    };
    match res_type {
        1 => handle_playurl_request(&req, true, false).await,
        2 => handle_playurl_request(&req, false, false).await,
        3 => handle_playurl_request(&req, true, true).await,
        4 => handle_search_request(&req, true, false).await,
        5 => handle_search_request(&req, false, false).await,
        6 => handle_search_request(&req, true, true).await,
        7 => handle_th_season_request(&req, true, true).await,
        8 => handle_th_subtitle_request(&req, false, true).await,
        _ => {
            println!("[Error] 未预期的行为 match res_type");
            HttpResponse::Ok()
                .content_type(ContentType::json())
                .insert_header(("From", "biliroaming-rust-server"))
                .insert_header(("Access-Control-Allow-Origin", "https://www.bilibili.com"))
                .insert_header(("Access-Control-Allow-Credentials", "true"))
                .insert_header(("Access-Control-Allow-Methods", "GET"))
                .body("{\"code\":-500,\"message\":\"未预期的行为\"}")
        }
    }
}

#[get("/donate")]
async fn donate(req: HttpRequest) -> impl Responder {
    let (_, config, _) = req
        .app_data::<(Pool, BiliConfig, Arc<Sender<BackgroundTaskType>>)>()
        .unwrap();
    return HttpResponse::Found()
        .insert_header(("Location", &config.donate_url[..]))
        .body("");
}

#[get("/pgc/player/api/playurl")]
async fn zhplayurl_app(req: HttpRequest) -> impl Responder {
    handle_playurl_request(&req, true, false).await
}

#[get("/pgc/player/web/playurl")]
async fn zhplayurl_web(req: HttpRequest) -> impl Responder {
    handle_playurl_request(&req, false, false).await
}

#[get("/intl/gateway/v2/ogv/playurl")]
async fn thplayurl_app(req: HttpRequest) -> impl Responder {
    handle_playurl_request(&req, true, true).await
}

#[get("/x/v2/search/type")]
async fn zhsearch_app(req: HttpRequest) -> impl Responder {
    handle_search_request(&req, true, false).await
}

#[get("/x/web-interface/search/type")]
async fn zhsearch_web(req: HttpRequest) -> impl Responder {
    handle_search_request(&req, false, false).await
}

#[get("/intl/gateway/v2/app/search/type")]
async fn thsearch_app(req: HttpRequest) -> impl Responder {
    handle_search_request(&req, true, true).await //emmmm 油猴脚本也用的这个
}

#[get("/intl/gateway/v2/ogv/view/app/season")]
async fn thseason_app(req: HttpRequest) -> impl Responder {
    handle_th_season_request(&req, true, true).await
}

#[get("/pgc/view/web/season")]
async fn cn_season(req: HttpRequest) -> impl Responder {
    handle_cn_season_request(&req, true, false).await
}

#[get("/intl/gateway/v2/app/subtitle")]
async fn thsubtitle_web(req: HttpRequest) -> impl Responder {
    handle_th_subtitle_request(&req, false, true).await
}

#[get("/api/accesskey")]
async fn api_accesskey(req: HttpRequest) -> impl Responder {
    handle_api_access_key_request(&req).await
}

/// Extract a safe redirect host from a client-supplied `Host`/`authority`.
///
/// The `Host` header is attacker controlled, so it must not be echoed verbatim
/// into `Location`: a value like `evil.example` or `a@b` turns this endpoint
/// into an open redirect. Only DNS-style names, IPv4 literals and bracketed
/// IPv6 literals are accepted; anything else is rejected and the caller
/// answers 400.
fn redirect_host(host: &str) -> Option<&str> {
    // Strip the optional `:port` suffix without allocating.
    let host = if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal: keep the brackets, drop everything after `]`.
        let end = rest.find(']')?;
        &host[..end + 2]
    } else {
        host.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return None;
    }
    // Reject characters that would change the authority or inject a header.
    let valid = host.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'[' | b']' | b':')
    });
    if !valid {
        return None;
    }
    // Require at least one dot for names, or a literal in brackets.
    if host.starts_with('[') || host.contains('.') {
        Some(host)
    } else {
        None
    }
}

async fn http2https_handler(req: HttpRequest) -> impl Responder {
    let https_port = req.app_data::<u16>().unwrap();
    let uri = req.uri();
    let raw_host = req
        .headers()
        .get("Host")
        .or_else(|| req.headers().get("authority"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Some(host) = redirect_host(raw_host) else {
        error!("无法获取合法 host, 拒绝重定向");
        return HttpResponse::BadRequest()
            .content_type(ContentType::json())
            .body(r#"{"code":-400,"message":"无效的 Host 头"}"#);
    };

    let path_and_query = if let Some(value) = uri.path_and_query() {
        value.as_str()
    } else {
        "/"
    };

    HttpResponse::MovedPermanently() // 301 redirect
        .insert_header((
            "Location",
            format!("https://{}:{}{}", host, https_port, path_and_query),
        ))
        .body("")
}

lazy_static! {
    pub static ref SERVER_CONFIG: BiliConfig = init_biliconfig();
    pub static ref REDIS_POOL: Pool = Config::from_url(&SERVER_CONFIG.redis)
        .create_pool(Some(Runtime::Tokio1))
        .unwrap();
    pub static ref CHANNEL: (Sender<BackgroundTaskType>, Receiver<BackgroundTaskType>) =
        async_channel::bounded(120);
    pub static ref BILISENDER: Arc<Sender<BackgroundTaskType>> = Arc::new(CHANNEL.0.clone());
}

fn main() -> std::io::Result<()> {
    // init log
    use chrono::Local;
    use std::io::Write;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let env = env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info");
    env_logger::Builder::from_env(env)
        .format(|buf, record| {
            // env_logger 0.11 removed `Formatter::default_styled_level`; the
            // replacement is `default_level_style`, which returns an
            // `anstyle::Style` that renders as the SGR start code and, with the
            // alternate flag, as the matching reset code.
            let level_style = buf.default_level_style(record.level());
            let level = record.level();
            writeln!(
                buf,
                "[{}][{}{:>5}{:#}] {}",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                level_style,
                level,
                level_style,
                &record.args()
            )
        })
        .init();

    info!("你好喵~");
    ctrlc::set_handler(move || {
        //目前来看这个已经没用了,但以防万一卡死,还是留着好了
        error!("已关闭 biliroaming_rust_server");
        std::process::exit(0);
    })
    .unwrap();
    // //init server_config => BiliConfig
    //fs::write("config.example.yml", serde_yaml::to_string(&config).unwrap()).unwrap(); //Debug 方便生成示例配置
    {
        // check before load configuration
        if let Ok(is_updated) = rt.block_on(update_biliconfig()) {
            if is_updated {
                info!("配置文件自动更新成功");
            }
        } else {
            error!("配置文件更新失败");
        }
    }
    let server_config: BiliConfig = SERVER_CONFIG.clone();
    let woker_num = server_config.worker_num;
    let http_port = server_config.http_port.clone();
    let https_port = server_config.https_port.clone();
    let bilisender = Arc::clone(&*BILISENDER);
    {
        let bili_runtime = BiliRuntime::new(&*SERVER_CONFIG, &*REDIS_POOL, &*BILISENDER);
        rt.block_on(prepare_before_start(bili_runtime));
    }
    let web_background = run_background_queue(&CHANNEL.1, |task| async move {
        let runtime = BiliRuntime::new(&SERVER_CONFIG, &REDIS_POOL, &BILISENDER);
        if let Err(error) = background_task_run(task, &runtime).await {
            error!("{error}");
        }
    });

    let rate_limit_per_second = if server_config.rate_limit_per_second == 0 {
        1
    } else {
        server_config.rate_limit_per_second
    };
    let rate_limit_burst = if server_config.rate_limit_burst == 0 {
        // 并发数
        1919810
    } else {
        server_config.rate_limit_burst
    };
    let rate_limit_conf = GovernorConfigBuilder::default()
        .seconds_per_request(rate_limit_per_second)
        .burst_size(rate_limit_burst)
        .key_extractor(BiliUserToken)
        .finish()
        .unwrap();

    let use_https = server_config.https_support;
    let ssl_config = if use_https {
        Some(load_sslconfig().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("TLS configuration failed: {error}"),
            )
        })?)
    } else {
        None
    };

    if use_https && SERVER_CONFIG.http2https_support {
        let web_main = HttpServer::new(move || {
            let rediscfg = Config::from_url(&server_config.redis);
            let pool = rediscfg.create_pool(Some(Runtime::Tokio1)).unwrap();
            App::new()
                .app_data((pool, server_config.clone(), bilisender.clone()))
                .wrap(Governor::new(&rate_limit_conf))
                .wrap(middleware::Compress::default())
                .wrap(ChangeCompressPriority)
                .service(hello)
                .service(zhplayurl_app)
                .service(zhplayurl_web)
                .service(thplayurl_app)
                .service(zhsearch_app)
                .service(zhsearch_web)
                .service(thsearch_app)
                .service(thseason_app)
                .service(cn_season)
                .service(thsubtitle_web)
                .service(api_accesskey)
                .service(donate)
                .service(Files::new("/", "./web/").index_file("index.html"))
                .default_service(web::route().to(web_default))
        })
        .bind_rustls_0_23(("0.0.0.0", https_port), ssl_config.unwrap())
        .unwrap()
        .workers(woker_num)
        .keep_alive(Duration::from_secs(20))
        .run();

        let http2https = HttpServer::new(move || {
            App::new()
                .app_data(https_port)
                .default_service(web::route().to(http2https_handler))
        })
        .bind(("0.0.0.0", http_port))
        .unwrap()
        .workers(woker_num)
        .keep_alive(Duration::from_secs(20))
        .run();

        rt.block_on(async { join!(web_background, web_main, http2https).1 })
    } else if use_https {
        let web_main = HttpServer::new(move || {
            let rediscfg = Config::from_url(&server_config.redis);
            let pool = rediscfg.create_pool(Some(Runtime::Tokio1)).unwrap();
            App::new()
                .app_data((pool, server_config.clone(), bilisender.clone()))
                .wrap(Governor::new(&rate_limit_conf))
                .wrap(middleware::Compress::default())
                .wrap(ChangeCompressPriority)
                .service(hello)
                .service(zhplayurl_app)
                .service(zhplayurl_web)
                .service(thplayurl_app)
                .service(zhsearch_app)
                .service(zhsearch_web)
                .service(thsearch_app)
                .service(thseason_app)
                .service(cn_season)
                .service(thsubtitle_web)
                .service(api_accesskey)
                .service(donate)
                .service(Files::new("/", "./web/").index_file("index.html"))
                .default_service(web::route().to(web_default))
        })
        .bind_rustls_0_23(("0.0.0.0", https_port), ssl_config.unwrap())
        .unwrap()
        .workers(woker_num)
        .keep_alive(Duration::from_secs(20))
        .run();

        rt.block_on(async { join!(web_background, web_main).1 })
    } else {
        let web_main = HttpServer::new(move || {
            let rediscfg = Config::from_url(&server_config.redis);
            let pool = rediscfg.create_pool(Some(Runtime::Tokio1)).unwrap();
            App::new()
                .app_data((pool, server_config.clone(), bilisender.clone()))
                .wrap(Governor::new(&rate_limit_conf))
                .service(hello)
                .service(zhplayurl_app)
                .service(zhplayurl_web)
                .service(thplayurl_app)
                .service(zhsearch_app)
                .service(zhsearch_web)
                .service(thsearch_app)
                .service(thseason_app)
                .service(cn_season)
                .service(thsubtitle_web)
                .service(api_accesskey)
                .service(donate)
                .service(Files::new("/", "./web/").index_file("index.html"))
                .default_service(web::route().to(web_default))
        })
        .bind(("0.0.0.0", http_port))
        .unwrap()
        .workers(woker_num)
        .keep_alive(Duration::from_secs(20))
        .run();

        rt.block_on(async { join!(web_background, web_main).1 })
    }
}

#[cfg(test)]
mod tests {
    use super::redirect_host;

    #[test]
    fn redirect_host_accepts_legitimate_hosts() {
        assert_eq!(redirect_host("example.com"), Some("example.com"));
        assert_eq!(redirect_host("example.com:2662"), Some("example.com"));
        assert_eq!(
            redirect_host("sub.example.com:8443"),
            Some("sub.example.com")
        );
        assert_eq!(redirect_host("127.0.0.1:8080"), Some("127.0.0.1"));
        assert_eq!(redirect_host("[2001:db8::1]:8443"), Some("[2001:db8::1]"));
        assert_eq!(redirect_host("[::1]"), Some("[::1]"));
    }

    /// 开放重定向回归：这些值在修复前都会被原样拼进 `Location`。
    #[test]
    fn redirect_host_rejects_open_redirect_payloads() {
        // 无点号的裸主机名（可解析到攻击者控制的搜索域/内网名）
        assert_eq!(redirect_host("evil"), None);
        // 会改变 authority 语义的字符
        assert_eq!(redirect_host("evil.com@attacker.com"), None);
        assert_eq!(redirect_host("evil.com/attacker"), None);
        assert_eq!(redirect_host("evil.com#@attacker"), None);
        assert_eq!(redirect_host("evil.com?a=b"), None);
        assert_eq!(redirect_host("evil.com\tX"), None);
        assert_eq!(redirect_host("evil.com X"), None);
        // 缺失/空值
        assert_eq!(redirect_host(""), None);
        assert_eq!(redirect_host(":"), None);
        // 未闭合的 IPv6 字面量
        assert_eq!(redirect_host("[2001:db8::1"), None);
    }
}
