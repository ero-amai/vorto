use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::watch;
use tokio::task::yield_now;
use tokio::time::{Duration, timeout};

use super::super::*;
use super::helpers::{TcpEchoServer, assert_tcp_round_trip, reserve_tcp_addr, tcp_tunnel_spec};
use crate::config::Protocol;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_tunnel_spawn_rolls_back_partial_start_on_udp_bind_failure() {
    let listen = reserve_tcp_addr();
    let udp_blocker = UdpSocket::bind(listen)
        .await
        .expect("test UDP blocker should bind");
    let target = reserve_tcp_addr();
    let spec = TunnelConfig {
        name: "dual-stack".to_string(),
        listen: listen.to_string(),
        target: target.to_string(),
        protocol: Protocol::Both,
        enabled: true,
    };

    let error = match ActiveTunnel::spawn(spec).await {
        Ok(_) => panic!("spawn should fail while UDP port is occupied"),
        Err(error) => error,
    };
    let error_text = error.to_string().to_ascii_lowercase();
    assert!(
        error_text.contains("address already in use") || error_text.contains("addrinuse"),
        "unexpected error: {error}"
    );

    let connect_result = timeout(Duration::from_millis(300), TcpStream::connect(listen))
        .await
        .expect("TCP connect attempt should complete");
    let error = connect_result.expect_err("TCP listener should have been rolled back");
    assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);

    drop(udp_blocker);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_restarts_finished_tunnels() {
    let backend = TcpEchoServer::spawn().await;
    let listen = reserve_tcp_addr();
    let spec = tcp_tunnel_spec("recycle", listen, backend.addr);

    let (stop_tx, _) = watch::channel(false);
    let finished_handle = tokio::spawn(async {});
    timeout(Duration::from_secs(1), async {
        while !finished_handle.is_finished() {
            yield_now().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("test task should finish quickly");

    let mut manager = TunnelManager::default();
    manager.active.insert(
        spec.name.clone(),
        ActiveTunnel {
            spec: spec.clone(),
            stop_tx,
            handles: vec![finished_handle],
        },
    );

    assert!(manager.reconcile(vec![spec.clone()]).await);
    assert_tcp_round_trip(listen, b"restart-check").await;

    manager.stop_all().await;
    backend.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_restarts_panicked_tunnels() {
    let backend = TcpEchoServer::spawn().await;
    let listen = reserve_tcp_addr();
    let spec = tcp_tunnel_spec("panic-recycle", listen, backend.addr);

    let (stop_tx, _) = watch::channel(false);
    let panicked_handle = tokio::spawn(async {
        panic!("simulated worker panic");
    });
    timeout(Duration::from_secs(1), async {
        while !panicked_handle.is_finished() {
            yield_now().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("panicked task should finish quickly");

    let mut manager = TunnelManager::default();
    manager.active.insert(
        spec.name.clone(),
        ActiveTunnel {
            spec: spec.clone(),
            stop_tx,
            handles: vec![panicked_handle],
        },
    );

    assert!(manager.reconcile(vec![spec.clone()]).await);
    assert_tcp_round_trip(listen, b"panic-recovered").await;

    manager.stop_all().await;
    backend.stop().await;
}
