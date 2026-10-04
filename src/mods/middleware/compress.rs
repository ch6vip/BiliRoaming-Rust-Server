use actix_web::{
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    http::header::{self, AcceptEncoding, HeaderValue},
    Error, HttpMessage,
};
use futures::future::LocalBoxFuture;
use std::future::{ready, Ready};

pub struct ChangeCompressPriority;

impl<S, B> Transform<S, ServiceRequest> for ChangeCompressPriority
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = ChangeCompressPriorityMiddleware<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(ChangeCompressPriorityMiddleware { service }))
    }
}

pub struct ChangeCompressPriorityMiddleware<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for ChangeCompressPriorityMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, mut req: ServiceRequest) -> Self::Future {
        // Every value here is attacker controlled, so a malformed
        // `Accept-Encoding` must degrade to "unknown weight" instead of
        // unwrapping and taking down the worker.
        if req.get_header::<AcceptEncoding>().is_some() {
            let raw = req
                .headers()
                .get(header::ACCEPT_ENCODING)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if let Some(raw) = raw {
                let mut weighted: Vec<(String, i32)> = raw
                    .split(',')
                    .filter_map(|item| {
                        let mut parts = item.split(';').map(str::trim);
                        let token = parts.next().filter(|token| !token.is_empty())?;
                        let weight = match parts.next() {
                            // No `;q=` parameter: use the fixed server ranking.
                            None => match token {
                                "br" => 5,
                                "zstd" => 4,
                                "gzip" => 3,
                                "deflate" => 2,
                                "identity" | "*" => 1,
                                _ => 0,
                            },
                            // `q=`/`Q=` value; anything unparsable is ignored.
                            Some(q) => q
                                .strip_prefix("q=")
                                .or_else(|| q.strip_prefix("Q="))
                                .and_then(|value| value.parse::<f32>().ok())
                                .map(|value| (value * 10.0) as i32)
                                .unwrap_or(0),
                        };
                        Some((token.to_owned(), weight))
                    })
                    .filter(|(_, weight)| *weight != 0)
                    .collect();
                // Stable ascending sort then reverse keeps the original
                // tie-breaking (last listed entry wins) for equal weights.
                weighted.sort_by_key(|(_, weight)| *weight);
                weighted.reverse();

                let headers = req.headers_mut();
                match weighted.first().map(|(token, _)| token.as_str()) {
                    Some("*") => {
                        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
                    }
                    Some(token) => {
                        if let Ok(value) = HeaderValue::from_str(token) {
                            headers.insert(header::ACCEPT_ENCODING, value);
                        }
                    }
                    None => {
                        headers.remove(header::ACCEPT_ENCODING);
                    }
                }
            }
        }
        let fut = self.service.call(req);
        Box::pin(async move {
            let res = fut.await?;
            Ok(res)
        })
    }
}
