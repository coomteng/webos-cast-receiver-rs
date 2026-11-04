use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use webos_cast_receiver::config::Config;
use webos_cast_receiver::server::{build, VERSION};
use webos_cast_receiver::ssdp::Ssdp;
use webos_cast_receiver::logln;

#[tokio::main]
async fn main() {
    let cfg = Config::from_env();
    let bind_ip: IpAddr = if cfg.bind_addr.is_empty() { Ipv4Addr::UNSPECIFIED.into() } else { cfg.bind_addr.parse().expect("BIND_ADDR must be an IPv4 address") };
    let (app, router) = build(cfg);
    let cfg = &app.cfg;
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(bind_ip, cfg.port)).await.expect("bind HTTP port");
    logln!("[http] {}  friendlyName=\"{}\" uuid={} proxy={} v{}", cfg.base, cfg.friendly_name, cfg.uuid, cfg.proxy_mode.as_str(), VERSION);

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let ssdp = Arc::new(Ssdp {
        uuid: cfg.uuid.clone(),
        location: format!("{}/dlna/device.xml", cfg.base),
        bind_addr: match bind_ip { IpAddr::V4(v4) if !v4.is_unspecified() => Some(v4), _ => None },
    });
    let mut rx1 = stop_rx.clone();
    let ssdp_task = tokio::spawn(ssdp.run(async move { let _ = rx1.wait_for(|s| *s).await; }));

    let mut rx2 = stop_rx.clone();
    let serve = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move { let _ = rx2.wait_for(|s| *s).await; });

    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
        }
        #[cfg(not(unix))]
        { let _ = ctrl_c.await; }
        logln!("[main] shutting down");
        let _ = stop_tx.send(true);
    });

    if let Err(e) = serve.await {
        logln!("[http] server error: {e}");
    }
    let _ = tokio::time::timeout(std::time::Duration::from_millis(800), ssdp_task).await;
}
