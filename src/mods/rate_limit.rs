use actix_governor::{KeyExtractor, SimpleKeyExtractionError};
use actix_web::{dev::ServiceRequest, http::header::ContentType};
// use governor::clock::{Clock, DefaultClock};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Eq, PartialEq)]
pub struct BiliUserToken;

impl KeyExtractor for BiliUserToken {
    type Key = String;
    type KeyExtractionError = SimpleKeyExtractionError<&'static str>;

    fn extract(&self, req: &ServiceRequest) -> Result<Self::Key, Self::KeyExtractionError> {
        let config = req
            .app_data::<(
                deadpool_redis::Pool,
                super::types::BiliConfig,
                std::sync::Arc<async_channel::Sender<super::types::BackgroundTaskType>>,
            )>()
            .map(|data| &data.1);
        let ip = config
            .and_then(|config| client_ip(req.request(), config))
            .or_else(|| req.peer_addr().map(|addr| addr.ip()));
        Ok(ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "unknown-peer".into()))
    }

    fn exceed_rate_limit_response(
        &self,
        negative: &actix_governor::governor::NotUntil<
            actix_governor::governor::clock::QuantaInstant,
        >,
        mut response: actix_web::HttpResponseBuilder,
    ) -> actix_web::HttpResponse {
        let wait_time = negative
            .wait_time_from(actix_governor::governor::clock::Clock::now(
                &actix_governor::governor::clock::DefaultClock::default(),
            ))
            .as_secs();
        response.content_type(ContentType::json()).body(format!(
            r#"{{"code":-429,"message":"请求过快,请{wait_time}s后重试"}}"#
        ))
    }
}

/// Only explicitly trusted peers may provide a single X-Real-IP address.
pub fn client_ip(
    req: &actix_web::HttpRequest,
    config: &super::types::BiliConfig,
) -> Option<std::net::IpAddr> {
    let peer = req.peer_addr()?.ip();
    if config.trusted_proxies.contains(&peer) {
        if let Some(ip) = req
            .headers()
            .get("X-Real-IP")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
        {
            return Some(ip);
        }
    }
    Some(peer)
}
