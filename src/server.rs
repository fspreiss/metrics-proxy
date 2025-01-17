use crate::config::{self, HttpProxy, ListenerSpec};
use crate::proxy;
use axum::extract::State;
use axum::http;
use axum::http::StatusCode;
use axum::middleware::map_response;
use axum::{routing::get, Router};
use axum_otel_metrics::HttpMetricsLayer;
use hyper;
use hyper::body::Bytes;
use hyper::server::conn::AddrIncoming;
use hyper_rustls::TlsAcceptor;
use rustls;
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;
use tower_http;

#[derive(Debug)]
pub enum ServeErrorKind {
    HyperError(hyper::Error),
    RustlsError(rustls::Error),
}

impl fmt::Display for ServeErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                ServeErrorKind::HyperError(e) => format!("{e}"),
                ServeErrorKind::RustlsError(ef) => format!("{ef}"),
            }
        )
    }
}

#[derive(Debug)]
pub struct StartError {
    addr: SocketAddr,
    error: ServeErrorKind,
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "cannot listen on {}: {}", self.addr, self.error)
    }
}

enum ServerKind {
    PrometheusMetricsProxy(HttpProxy),
    PrometheusMetricsServer(ListenerSpec),
}

pub struct Server {
    config: ServerKind,
    metrics_collector: Option<HttpMetricsLayer>,
}

impl From<HttpProxy> for Server {
    fn from(config: HttpProxy) -> Self {
        Self::for_metrics_proxy(config)
    }
}

impl Server {
    #[must_use]
    /// Configures this `Server` to proxy one or more handlers
    /// to a backend endpoint each.
    pub fn for_metrics_proxy(config: HttpProxy) -> Self {
        Server {
            config: ServerKind::PrometheusMetricsProxy(config),
            metrics_collector: None,
        }
    }

    #[must_use]
    /// Configures this `Server` to serve Prometheus metrics
    /// collected during proxying.
    pub fn for_service_metrics(listen_on: ListenerSpec) -> Self {
        Server {
            config: ServerKind::PrometheusMetricsServer(listen_on),
            metrics_collector: None,
        }
    }

    #[must_use]
    /// Enables telemetry collection.
    pub fn with_telemetry(self, ml: HttpMetricsLayer) -> Self {
        Server {
            config: self.config,
            metrics_collector: Some(ml),
        }
    }

    /// Starts an HTTP or HTTPS server on the configured host and port,
    /// proxying requests to each one of the targets defined in the
    /// `handlers` of the `HttpProxy` config.
    ///
    /// # Errors
    /// * `StartError` is returned if the server fails to start.
    pub async fn serve(self) -> Result<(), StartError> {
        // Short helper to issue backend request.
        async fn handle_with_proxy(
            State(proxy): State<proxy::MetricsProxier>,
            headers: http::HeaderMap,
        ) -> (StatusCode, http::HeaderMap, Bytes) {
            proxy.handle(headers).await
        }

        // Short helper to map 408 from request response timeout layer to 504.
        async fn gateway_timeout<B>(
            mut response: axum::response::Response<B>,
        ) -> axum::response::Response<B> {
            if response.status() == http::StatusCode::REQUEST_TIMEOUT {
                *response.status_mut() = http::StatusCode::GATEWAY_TIMEOUT;
            }
            response
        }

        let listener = match &self.config {
            ServerKind::PrometheusMetricsProxy(config) => config.listen_on.clone(),
            ServerKind::PrometheusMetricsServer(listen_on) => listen_on.clone(),
        };

        let mut router: Router<_, _> = Router::new();
        let bodytimeout =
            tower_http::timeout::RequestBodyTimeoutLayer::new(listener.header_read_timeout);

        router = match self.config {
            ServerKind::PrometheusMetricsProxy(config) => {
                for (path, target) in config.handlers.clone() {
                    let cache_duration = target.clone().cache_duration;
                    let state = proxy::MetricsProxier::from(target);
                    let mut method_router = get(handle_with_proxy)
                        .with_state(state)
                        .layer(tower::ServiceBuilder::new().layer(bodytimeout.clone()));
                    if Duration::from(cache_duration) > Duration::new(0, 0) {
                        method_router = method_router
                            .layer(crate::cache::CacheLayer::new(cache_duration.into()));
                    }
                    router = router.route(path.as_str(), method_router);
                }
                router
            }
            ServerKind::PrometheusMetricsServer(_) => match self.metrics_collector.clone() {
                Some(pl) => router.merge(pl.routes()),
                None => router,
            },
        };

        // Second-to-last the timeout layer.
        // The timeout layer returns HTTP status code 408 if the backend
        // fails to respond on time.  When this happens, we map that code
        // to 503 Gateway Timeout.
        // (Contrast with backend down -- this usually requires a response
        // of 502 Bad Gateway, which is already issued by the client handler.)
        let timeout_handling_layer =
            tower_http::timeout::TimeoutLayer::new(listener.request_response_timeout);
        router = router
            .layer(timeout_handling_layer)
            .layer(map_response(gateway_timeout));

        // Then, finally, the telemetry layer.
        // Experimentally, if the telemetry layer does not go last, then
        // whatever errors the timeout layers bubble up, the telemetry
        // layer cannot register as an HTTP error.
        if let Some(pl) = self.metrics_collector {
            router = router.layer(pl);
        }

        let incoming = AddrIncoming::bind(&listener.sockaddr).map_err(|error| StartError {
            addr: listener.sockaddr,
            error: ServeErrorKind::HyperError(error),
        })?;

        match &listener.protocol {
            config::Protocol::Http => {
                hyper::Server::builder(incoming)
                    .http1_header_read_timeout(listener.header_read_timeout)
                    .serve(router.into_make_service())
                    .await
            }
            config::Protocol::Https { certificate, key } => {
                hyper::Server::builder(
                    TlsAcceptor::builder()
                        .with_single_cert(certificate.clone(), key.clone())
                        .map_err(|error| StartError {
                            addr: listener.sockaddr,
                            error: ServeErrorKind::RustlsError(error),
                        })?
                        .with_all_versions_alpn()
                        .with_incoming(incoming),
                )
                .http1_header_read_timeout(listener.header_read_timeout)
                .serve(router.into_make_service())
                .await
            }
        }
        .map_err(|error| StartError {
            addr: listener.sockaddr,
            error: ServeErrorKind::HyperError(error),
        })
    }
}
