use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::dashboard::assets::StaticAssetResolver;
use crate::dashboard::router::{MachineDashboardRouter, RouterRequest};

pub struct ServerHandle {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

pub struct DashboardHTTPServer;

impl DashboardHTTPServer {
    /// Bind strictly to `127.0.0.1:<port>` (never `0.0.0.0` or IPv6 any-address).
    pub async fn start(
        port: u16,
        router: Arc<MachineDashboardRouter>,
        assets: Arc<StaticAssetResolver>,
    ) -> std::io::Result<ServerHandle> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let addr = listener.local_addr()?;
        let bound_port = addr.port();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        break;
                    }
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else {
                            continue;
                        };
                        let router = router.clone();
                        let assets = assets.clone();
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let service = service_fn(move |req: Request<Incoming>| {
                                let router = router.clone();
                                let assets = assets.clone();
                                async move {
                                    handle_http_request(req, router, assets, bound_port).await
                                }
                            });
                            let _ = http1::Builder::new()
                                .keep_alive(false)
                                .serve_connection(io, service)
                                .await;
                        });
                    }
                }
            }
        });

        Ok(ServerHandle {
            addr,
            shutdown_tx: Some(shutdown_tx),
            join: Some(join),
        })
    }
}

async fn handle_http_request(
    req: Request<Incoming>,
    router: Arc<MachineDashboardRouter>,
    assets: Arc<StaticAssetResolver>,
    bound_port: u16,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let method = req.method().as_str().to_owned();
    let uri = req.uri().clone();
    let path = uri.path().to_owned();
    let query = uri.query().unwrap_or("").to_owned();

    let mut headers = HashMap::new();
    for (name, value) in req.headers() {
        if let Ok(text) = value.to_str() {
            headers.insert(name.as_str().to_ascii_lowercase(), text.to_owned());
        }
    }

    if path.starts_with("/api/") || path == "/api" {
        let body_bytes = match req.into_body().collect().await {
            Ok(collected) => collected.to_bytes().to_vec(),
            Err(_) => Vec::new(),
        };
        let router_req = RouterRequest {
            method,
            path,
            query,
            headers,
            body: body_bytes,
            is_trusted_ipc: false,
            expected_port: Some(bound_port),
        };
        let resp = router.route(router_req).await;
        let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let http_resp = Response::builder()
            .status(status)
            .header("Content-Type", resp.content_type)
            .header("Cache-Control", "no-store")
            .body(Full::new(Bytes::from(resp.body)))
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())));
        return Ok(http_resp);
    }

    if method == "OPTIONS" {
        let http_resp = Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header("Content-Type", "application/json; charset=utf-8")
            .body(Full::new(Bytes::from_static(
                br#"{"error":"method_not_allowed","message":"OPTIONS is not supported."}"#,
            )))
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())));
        return Ok(http_resp);
    }

    let asset = assets.serve(&path);
    let status = StatusCode::from_u16(asset.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let http_resp = Response::builder()
        .status(status)
        .header("Content-Type", asset.content_type)
        .body(Full::new(Bytes::from(asset.body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())));
    Ok(http_resp)
}
