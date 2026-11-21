//! The HTTP accept loop.
//!
//! `axum::serve` sets no header read timeout, so a client that opens a TCP
//! connection and never finishes its request holds a socket and a task
//! forever, without counting against any connection limit (those apply only
//! once a WebSocket upgrade reaches the handler). This loop drives hyper
//! directly so the request head, and every idle keep-alive wait after it, has
//! a deadline. Only HTTP/1.1 is served: WebSocket upgrades and health checks
//! need nothing else, and reverse proxies speak HTTP/1.1 to their backends.

use axum::extract::ConnectInfo;
use axum::Router;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::Request;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower::ServiceExt;

/// Serves `router` on `listener` until `shutdown` turns true, then stops
/// accepting and returns. Requests in flight are finished in the background;
/// WebSocket sessions watch `shutdown` themselves.
///
/// Each connection must deliver its request head within `header_timeout`.
/// Handlers see the TCP peer as `ConnectInfo<SocketAddr>`.
pub async fn serve(listener: TcpListener, router: Router, header_timeout: Duration, mut shutdown: watch::Receiver<bool>) {
    loop {
        let (tcp, addr) = tokio::select! {
            result = listener.accept() => match result {
                Ok(accepted) => accepted,
                Err(error) => {
                    // Out of file descriptors and the like: back off instead
                    // of spinning. A peer that reset before accept is routine.
                    if !matches!(error.kind(), std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused) {
                        log::warn!("accept failed: {error}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    continue;
                }
            },
            () = stopped(&mut shutdown) => break,
        };
        // Nagle would delay small input and control frames behind video.
        let _ = tcp.set_nodelay(true);
        let service = TowerToHyperService::new(router.clone().map_request(move |mut request: Request<Incoming>| {
            request.extensions_mut().insert(ConnectInfo(addr));
            request
        }));
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut builder = http1::Builder::new();
            builder.timer(TokioTimer::new()).header_read_timeout(header_timeout);
            let connection = builder.serve_connection(TokioIo::new(tcp), service).with_upgrades();
            tokio::pin!(connection);
            tokio::select! {
                _ = connection.as_mut() => return,
                () = stopped(&mut shutdown) => connection.as_mut().graceful_shutdown(),
            }
            let _ = connection.await;
        });
    }
}

/// Resolves once `shutdown` is true. A dropped sender means nobody can ask
/// for shutdown any more, so that waits forever rather than stopping.
pub(crate) async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    if shutdown.wait_for(|stop| *stop).await.is_err() { std::future::pending::<()>().await; }
}
