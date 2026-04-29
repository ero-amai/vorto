use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::time::{Duration, sleep, timeout};

use super::super::*;
use super::helpers::{
    TcpDropFirstServer, TcpEchoServer, TcpHalfOpenServer, UdpEchoServer,
    assert_socket_closes_without_forwarding, assert_tcp_round_trip, assert_udp_round_trip,
    reserve_tcp_addr, reserve_udp_addr, tcp_tunnel_spec, udp_tunnel_spec,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_tunnels_keep_serving_other_modes_after_peer_failures() {
    let auto_backend = TcpEchoServer::spawn().await;
    let throughput_backend = TcpEchoServer::spawn().await;
    let second_throughput_backend = TcpEchoServer::spawn().await;
    let broken_target = reserve_tcp_addr();

    let auto_listen = reserve_tcp_addr();
    let throughput_listen = reserve_tcp_addr();
    let second_throughput_listen = reserve_tcp_addr();
    let broken_listen = reserve_tcp_addr();

    let mut manager = TunnelManager::default();
    let specs = vec![
        tcp_tunnel_spec("primary", auto_listen, auto_backend.addr),
        tcp_tunnel_spec("throughput", throughput_listen, throughput_backend.addr),
        tcp_tunnel_spec(
            "throughput-second",
            second_throughput_listen,
            second_throughput_backend.addr,
        ),
        tcp_tunnel_spec("broken", broken_listen, broken_target),
    ];

    assert!(manager.reconcile(specs).await);

    let auto_task = tokio::spawn(assert_tcp_round_trip(auto_listen, b"auto"));
    let throughput_task = tokio::spawn(assert_tcp_round_trip(
        throughput_listen,
        b"throughput-payload",
    ));
    let second_throughput_task = tokio::spawn(assert_tcp_round_trip(
        second_throughput_listen,
        b"throughput-second",
    ));
    let broken_task = tokio::spawn(async move {
        let mut stream = TcpStream::connect(broken_listen)
            .await
            .expect("broken tunnel should still accept a TCP connection");
        stream
            .write_all(b"drop-me")
            .await
            .expect("broken tunnel client should write");
        assert_socket_closes_without_forwarding(stream).await;
    });

    auto_task.await.expect("auto tunnel task should succeed");
    throughput_task
        .await
        .expect("throughput tunnel task should succeed");
    second_throughput_task
        .await
        .expect("second throughput tunnel task should succeed");
    broken_task
        .await
        .expect("broken tunnel task should succeed");

    assert_tcp_round_trip(auto_listen, b"auto-still-works").await;
    assert_tcp_round_trip(throughput_listen, b"throughput-still-works").await;
    assert_tcp_round_trip(second_throughput_listen, b"throughput-second-still-works").await;

    manager.stop_all().await;
    auto_backend.stop().await;
    throughput_backend.stop().await;
    second_throughput_backend.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_tunnel_survives_abrupt_client_disconnect() {
    let backend = TcpEchoServer::spawn().await;
    let listen = reserve_tcp_addr();

    let mut manager = TunnelManager::default();
    assert!(
        manager
            .reconcile(vec![tcp_tunnel_spec("client-drop", listen, backend.addr,)])
            .await
    );

    let mut stream = TcpStream::connect(listen)
        .await
        .expect("tunnel should accept a TCP connection");
    stream
        .write_all(&vec![7_u8; 64 * 1024])
        .await
        .expect("client should send payload before disconnecting");
    drop(stream);

    sleep(Duration::from_millis(100)).await;
    assert_tcp_round_trip(listen, b"after-client-drop").await;

    manager.stop_all().await;
    backend.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_tunnel_recovers_after_upstream_drops_one_connection() {
    let backend = TcpDropFirstServer::spawn().await;
    let listen = reserve_tcp_addr();

    let mut manager = TunnelManager::default();
    assert!(
        manager
            .reconcile(vec![
                tcp_tunnel_spec("upstream-drop", listen, backend.addr,)
            ])
            .await
    );

    let mut stream = TcpStream::connect(listen)
        .await
        .expect("tunnel should accept a TCP connection");
    stream
        .write_all(b"first-connection")
        .await
        .expect("client should write to first connection");
    assert_socket_closes_without_forwarding(stream).await;

    assert_tcp_round_trip(listen, b"after-upstream-drop").await;

    manager.stop_all().await;
    backend.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_tunnel_closes_excess_connections_until_capacity_is_available() {
    let backend = TcpEchoServer::spawn().await;
    let listen = reserve_tcp_addr();
    let spec = tcp_tunnel_spec("drop-over-capacity", listen, backend.addr);
    let listener = TcpListener::bind(listen)
        .await
        .expect("drop-over-capacity tunnel should bind a TCP listener");
    let limiter = Arc::new(Semaphore::new(1));
    let (stop_tx, stop_rx) = watch::channel(false);
    let tunnel = tokio::spawn(run_tcp_tunnel_with_limiter(
        spec, listener, stop_rx, limiter, 1,
    ));

    let first_stream = TcpStream::connect(listen)
        .await
        .expect("first client should connect while capacity is available");
    sleep(Duration::from_millis(50)).await;

    let mut excess_stream = TcpStream::connect(listen)
        .await
        .expect("excess client should complete the TCP handshake");
    let _ = excess_stream.write_all(b"excess").await;
    assert_socket_closes_without_forwarding(excess_stream).await;

    drop(first_stream);
    sleep(Duration::from_millis(100)).await;
    assert_tcp_round_trip(listen, b"after-capacity-release").await;

    let _ = stop_tx.send(true);
    tunnel
        .await
        .expect("drop-over-capacity tunnel task should join")
        .expect("drop-over-capacity tunnel should stop cleanly");
    backend.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_global_capacity_saturation_drops_excess_connections_without_blocking_recovery() {
    let blocking_backend = TcpHalfOpenServer::spawn(Duration::from_millis(250)).await;
    let echo_backend = TcpEchoServer::spawn().await;
    let blocking_listen = reserve_tcp_addr();
    let echo_listen = reserve_tcp_addr();
    let limiter = Arc::new(Semaphore::new(1));
    let (stop_tx, stop_rx) = watch::channel(false);

    let blocking_tunnel = tokio::spawn(run_tcp_tunnel_with_limiter(
        tcp_tunnel_spec("global-blocker", blocking_listen, blocking_backend.addr),
        TcpListener::bind(blocking_listen)
            .await
            .expect("blocking tunnel should bind a TCP listener"),
        stop_rx.clone(),
        limiter.clone(),
        1,
    ));
    let echo_tunnel = tokio::spawn(run_tcp_tunnel_with_limiter(
        tcp_tunnel_spec("global-echo", echo_listen, echo_backend.addr),
        TcpListener::bind(echo_listen)
            .await
            .expect("echo tunnel should bind a TCP listener"),
        stop_rx,
        limiter,
        1,
    ));

    let blocking_client = TcpStream::connect(blocking_listen)
        .await
        .expect("blocking client should connect while capacity is available");

    let mut saturated_echo_client = TcpStream::connect(echo_listen)
        .await
        .expect("echo client should complete the TCP handshake while saturated");
    let _ = saturated_echo_client.write_all(b"cross-tunnel").await;
    assert_socket_closes_without_forwarding(saturated_echo_client).await;

    drop(blocking_client);
    sleep(Duration::from_millis(350)).await;
    assert_tcp_round_trip(echo_listen, b"cross-tunnel-recovered").await;

    let _ = stop_tx.send(true);
    blocking_tunnel
        .await
        .expect("blocking tunnel task should join")
        .expect("blocking tunnel should stop cleanly");
    echo_tunnel
        .await
        .expect("echo tunnel task should join")
        .expect("echo tunnel should stop cleanly");
    blocking_backend.stop().await;
    echo_backend.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_tunnel_recovers_when_started_while_global_capacity_is_exhausted() {
    let backend = TcpEchoServer::spawn().await;
    let listen = reserve_tcp_addr();
    let limiter = Arc::new(Semaphore::new(1));
    let held_permit = limiter
        .clone()
        .acquire_owned()
        .await
        .expect("test should reserve the only TCP permit");
    let (stop_tx, stop_rx) = watch::channel(false);
    let tunnel = tokio::spawn(run_tcp_tunnel_with_limiter(
        tcp_tunnel_spec("startup-overload", listen, backend.addr),
        TcpListener::bind(listen)
            .await
            .expect("startup-overload tunnel should bind a TCP listener"),
        stop_rx,
        limiter,
        1,
    ));

    sleep(Duration::from_millis(100)).await;

    let mut overloaded_client = TcpStream::connect(listen)
        .await
        .expect("client should complete the TCP handshake");
    let _ = overloaded_client.write_all(b"startup-overload").await;
    assert_socket_closes_without_forwarding(overloaded_client).await;

    drop(held_permit);
    assert_tcp_round_trip(listen, b"startup-recovered").await;

    let _ = stop_tx.send(true);
    let _ = timeout(Duration::from_secs(1), tunnel).await;
    backend.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_tunnel_stops_promptly_after_rejecting_connection_without_capacity() {
    let backend = TcpEchoServer::spawn().await;
    let listen = reserve_tcp_addr();
    let limiter = Arc::new(Semaphore::new(1));
    let held_permit = limiter
        .clone()
        .acquire_owned()
        .await
        .expect("test should reserve the only TCP permit");
    let (stop_tx, stop_rx) = watch::channel(false);
    let tunnel = tokio::spawn(run_tcp_tunnel_with_limiter(
        tcp_tunnel_spec("stop-after-reject", listen, backend.addr),
        TcpListener::bind(listen)
            .await
            .expect("stop-after-reject tunnel should bind a TCP listener"),
        stop_rx,
        limiter,
        1,
    ));

    let mut stream = TcpStream::connect(listen)
        .await
        .expect("client should complete the TCP handshake before shutdown");
    stream
        .write_all(b"stop-me")
        .await
        .expect("client should write before tunnel shutdown");

    sleep(Duration::from_millis(100)).await;
    let _ = stop_tx.send(true);

    timeout(Duration::from_secs(1), tunnel)
        .await
        .expect("tunnel should stop promptly after rejecting over-capacity connections")
        .expect("blocked tunnel task should join")
        .expect("blocked tunnel should stop cleanly");

    drop(held_permit);
    assert_socket_closes_without_forwarding(stream).await;
    backend.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_tunnel_round_trips_datagrams() {
    let backend = UdpEchoServer::spawn().await;
    let listen = reserve_udp_addr();
    let spec = udp_tunnel_spec("udp", listen, backend.addr);

    let mut manager = TunnelManager::default();
    assert!(manager.reconcile(vec![spec]).await);

    assert_udp_round_trip(listen, b"udp-echo").await;
    assert_udp_round_trip(listen, b"udp-echo-second-pass").await;

    manager.stop_all().await;
    backend.stop().await;
}
