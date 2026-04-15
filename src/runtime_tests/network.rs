use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::{Duration, sleep};

use super::super::*;
use super::helpers::{
    TcpDropFirstServer, TcpEchoServer, UdpEchoServer, assert_socket_closes_without_forwarding,
    assert_tcp_round_trip, assert_udp_round_trip, reserve_tcp_addr, reserve_udp_addr,
    tcp_tunnel_spec, udp_tunnel_spec,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_tunnels_keep_serving_other_modes_after_peer_failures() {
    let auto_backend = TcpEchoServer::spawn().await;
    let throughput_backend = TcpEchoServer::spawn().await;
    let latency_backend = TcpEchoServer::spawn().await;
    let broken_target = reserve_tcp_addr();

    let auto_listen = reserve_tcp_addr();
    let throughput_listen = reserve_tcp_addr();
    let latency_listen = reserve_tcp_addr();
    let broken_listen = reserve_tcp_addr();

    let mut manager = TunnelManager::default();
    let specs = vec![
        tcp_tunnel_spec("auto", auto_listen, auto_backend.addr, TcpMode::Auto),
        tcp_tunnel_spec(
            "throughput",
            throughput_listen,
            throughput_backend.addr,
            TcpMode::Throughput,
        ),
        tcp_tunnel_spec(
            "latency",
            latency_listen,
            latency_backend.addr,
            TcpMode::Latency,
        ),
        tcp_tunnel_spec("broken", broken_listen, broken_target, TcpMode::Throughput),
    ];

    assert!(manager.reconcile(specs).await);

    let auto_task = tokio::spawn(assert_tcp_round_trip(auto_listen, b"auto"));
    let throughput_task = tokio::spawn(assert_tcp_round_trip(
        throughput_listen,
        b"throughput-payload",
    ));
    let latency_task = tokio::spawn(assert_tcp_round_trip(latency_listen, b"latency"));
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
    latency_task
        .await
        .expect("latency tunnel task should succeed");
    broken_task
        .await
        .expect("broken tunnel task should succeed");

    assert_tcp_round_trip(auto_listen, b"auto-still-works").await;
    assert_tcp_round_trip(throughput_listen, b"throughput-still-works").await;
    assert_tcp_round_trip(latency_listen, b"latency-still-works").await;

    manager.stop_all().await;
    auto_backend.stop().await;
    throughput_backend.stop().await;
    latency_backend.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_tunnel_survives_abrupt_client_disconnect() {
    let backend = TcpEchoServer::spawn().await;
    let listen = reserve_tcp_addr();

    let mut manager = TunnelManager::default();
    assert!(
        manager
            .reconcile(vec![tcp_tunnel_spec(
                "client-drop",
                listen,
                backend.addr,
                TcpMode::Latency,
            )])
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
            .reconcile(vec![tcp_tunnel_spec(
                "upstream-drop",
                listen,
                backend.addr,
                TcpMode::Throughput,
            )])
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
