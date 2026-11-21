use axum::serve::ListenerExt;
use tokio::net::TcpListener;
use tokio::sync::watch;

#[tokio::main]
async fn main() {
    // PORT is injected by the host platform (e.g. Coolify); default 8080.
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    // `relay healthcheck`: for the container HEALTHCHECK, so the image needs
    // no curl/wget. Exits 0 when /health answers 200.
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(if healthcheck(&port) { 0 } else { 1 });
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let settings = relay::Settings::from_env().unwrap_or_else(|error| fail(error));
    let addr = format!("0.0.0.0:{}", settings.port());
    let listener = TcpListener::bind(&addr).await.unwrap_or_else(|error| fail(format!("could not listen on {addr}: {error}")));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (app, handle) = relay::relay(&settings, shutdown_rx).unwrap_or_else(|error| fail(error));
    log::info!("relay listening on {addr}");
    // Nagle would delay small input and control frames behind video.
    let listener = listener.tap_io(|tcp| { let _ = tcp.set_nodelay(true); });
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown_signal(shutdown_tx))
        .await
        .unwrap();
    if let Err(error) = handle.persist() {
        log::warn!("could not save relay state on shutdown: {error}");
    }
}

/// Exits with a clear message on a startup problem, before anything is served.
fn fail(message: impl std::fmt::Display) -> ! {
    log::error!("{message}");
    std::process::exit(2);
}

fn healthcheck(port: &str) -> bool {
    use std::io::{Read, Write};
    use std::time::Duration;
    let Ok(addr) = format!("127.0.0.1:{port}").parse() else { return false };
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if stream.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut head = [0u8; 64];
    let read = stream.read(&mut head).unwrap_or(0);
    is_http_ok(&head[..read])
}

fn is_http_ok(response: &[u8]) -> bool {
    response.starts_with(b"HTTP/1.1 200") || response.starts_with(b"HTTP/1.0 200")
}

async fn shutdown_signal(shutdown_tx: watch::Sender<bool>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    { tokio::signal::ctrl_c().await.expect("install ctrl-c handler"); }
    let _ = shutdown_tx.send(true);
}

#[cfg(test)]
mod tests {
    use super::is_http_ok;

    #[test]
    fn healthcheck_accepts_only_200() {
        assert!(is_http_ok(b"HTTP/1.1 200 OK\r\n"));
        assert!(!is_http_ok(b"HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(!is_http_ok(b""));
    }
}
