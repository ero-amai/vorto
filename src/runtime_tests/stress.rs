use std::process::Stdio;

use super::super::*;
use super::helpers::{
    IperfServer, TcpEchoServer, TcpHalfOpenServer, TcpResetServer,
    assert_socket_closes_without_forwarding, assert_tcp_round_trip, reserve_tcp_addr,
    tcp_tunnel_spec,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Duration, Instant, sleep, timeout};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires local iperf3 and is intended for manual tunnel stress runs"]
async fn iperf3_multi_tunnel_stress() {
    let version = tokio::process::Command::new("iperf3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if version.is_err() || !version.expect("version probe should finish").success() {
        eprintln!("Skipping iperf3 stress test because iperf3 is unavailable.");
        return;
    }

    let backends = [reserve_tcp_addr(), reserve_tcp_addr(), reserve_tcp_addr()];
    let listens = [reserve_tcp_addr(), reserve_tcp_addr(), reserve_tcp_addr()];
    let names = ["iperf-one", "iperf-two", "iperf-three"];

    let _servers = backends
        .iter()
        .map(|addr| IperfServer::spawn(addr.port()).expect("iperf3 server should spawn"))
        .collect::<Vec<_>>();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut manager = TunnelManager::default();
    let specs = (0..names.len())
        .map(|index| tcp_tunnel_spec(names[index], listens[index], backends[index]))
        .collect::<Vec<_>>();
    assert!(manager.reconcile(specs).await);

    let mut clients = Vec::new();
    for listen in listens {
        clients.push(tokio::spawn(async move {
            for round in 0..3 {
                let status = tokio::process::Command::new("iperf3")
                    .args([
                        "-c",
                        "127.0.0.1",
                        "-p",
                        &listen.port().to_string(),
                        "-t",
                        "3",
                        "-P",
                        "4",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .expect("iperf3 client should run");
                assert!(
                    status.success(),
                    "iperf3 client failed in round {} for tunnel port {}",
                    round,
                    listen.port()
                );
            }
        }));
    }

    for client in clients {
        client.await.expect("iperf3 client task should finish");
    }

    manager.stop_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual soak test for disconnect, reset, refuse, and half-open scenarios"]
async fn tcp_multi_tunnel_chaos_soak() {
    let soak_secs = std::env::var("VORTO_SOAK_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30);
    let hold_ms = std::env::var("VORTO_HALF_OPEN_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(2_000);

    let auto_backend = TcpEchoServer::spawn().await;
    let throughput_backend = TcpEchoServer::spawn().await;
    let second_throughput_backend = TcpEchoServer::spawn().await;
    let rst_backend = TcpResetServer::spawn().await;
    let half_open_backend = TcpHalfOpenServer::spawn(Duration::from_millis(hold_ms)).await;
    let refuse_target = reserve_tcp_addr();

    let auto_listen = reserve_tcp_addr();
    let throughput_listen = reserve_tcp_addr();
    let second_throughput_listen = reserve_tcp_addr();
    let rst_listen = reserve_tcp_addr();
    let half_open_listen = reserve_tcp_addr();
    let refuse_listen = reserve_tcp_addr();

    let specs = vec![
        tcp_tunnel_spec("soak-one", auto_listen, auto_backend.addr),
        tcp_tunnel_spec("soak-two", throughput_listen, throughput_backend.addr),
        tcp_tunnel_spec(
            "soak-three",
            second_throughput_listen,
            second_throughput_backend.addr,
        ),
        tcp_tunnel_spec("soak-rst", rst_listen, rst_backend.addr),
        tcp_tunnel_spec("soak-half-open", half_open_listen, half_open_backend.addr),
        tcp_tunnel_spec("soak-refuse", refuse_listen, refuse_target),
    ];

    let mut manager = TunnelManager::default();
    assert!(manager.reconcile(specs.clone()).await);

    let deadline = Instant::now() + Duration::from_secs(soak_secs);
    let mut iteration = 0_u64;

    while Instant::now() < deadline {
        iteration += 1;

        assert!(
            manager.reconcile(specs.clone()).await,
            "reconcile should keep tunnels healthy at iteration {iteration}"
        );

        let normal_auto = tokio::spawn(async move {
            let payload = format!("auto-{iteration}").into_bytes();
            assert_tcp_round_trip(auto_listen, &payload).await;
        });
        let normal_throughput = tokio::spawn(async move {
            let payload = format!("throughput-{iteration}").into_bytes();
            assert_tcp_round_trip(throughput_listen, &payload).await;
        });
        let normal_second_throughput = tokio::spawn(async move {
            let payload = format!("throughput-second-{iteration}").into_bytes();
            assert_tcp_round_trip(second_throughput_listen, &payload).await;
        });

        let abrupt_client_drop = tokio::spawn(async move {
            let mut stream = TcpStream::connect(auto_listen)
                .await
                .expect("soak client-drop tunnel should accept");
            stream
                .write_all(&vec![5_u8; 128 * 1024])
                .await
                .expect("soak client-drop tunnel should receive data");
            drop(stream);
        });

        let rst_case = tokio::spawn(async move {
            let mut stream = TcpStream::connect(rst_listen)
                .await
                .expect("RST tunnel should accept connections");
            stream
                .write_all(b"trigger-rst")
                .await
                .expect("RST tunnel client should write");
            assert_socket_closes_without_forwarding(stream).await;
        });

        let refuse_case = tokio::spawn(async move {
            let mut stream = TcpStream::connect(refuse_listen)
                .await
                .expect("refuse tunnel should accept client connections");
            stream
                .write_all(b"trigger-refuse")
                .await
                .expect("refuse tunnel client should write");
            assert_socket_closes_without_forwarding(stream).await;
        });

        let half_open_case = tokio::spawn(async move {
            let mut stream = TcpStream::connect(half_open_listen)
                .await
                .expect("half-open tunnel should accept client connections");
            stream
                .write_all(b"trigger-half-open")
                .await
                .expect("half-open tunnel client should write");
            let mut buffer = [0_u8; 16];
            let result = timeout(Duration::from_millis(300), stream.read(&mut buffer)).await;
            assert!(
                result.is_err(),
                "half-open tunnel should stay open long enough to time out at iteration {iteration}"
            );
            drop(stream);
        });

        normal_auto
            .await
            .expect("normal auto soak task should succeed");
        normal_throughput
            .await
            .expect("normal throughput soak task should succeed");
        normal_second_throughput
            .await
            .expect("normal second throughput soak task should succeed");
        abrupt_client_drop
            .await
            .expect("abrupt client-drop soak task should succeed");
        rst_case.await.expect("RST soak task should succeed");
        refuse_case.await.expect("refuse soak task should succeed");
        half_open_case
            .await
            .expect("half-open soak task should succeed");

        assert_tcp_round_trip(auto_listen, b"post-chaos-auto").await;
        assert_tcp_round_trip(throughput_listen, b"post-chaos-throughput").await;
        assert_tcp_round_trip(second_throughput_listen, b"post-chaos-throughput-second").await;

        sleep(Duration::from_millis(50)).await;
    }

    manager.stop_all().await;
    auto_backend.stop().await;
    throughput_backend.stop().await;
    second_throughput_backend.stop().await;
    rst_backend.stop().await;
    half_open_backend.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual benchmark for socket-mode user-space forwarding latency"]
async fn throughput_mode_forwarding_latency_benchmark() {
    let warmup = std::env::var("VORTO_LATENCY_WARMUP")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(50);
    let samples = std::env::var("VORTO_LATENCY_SAMPLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(500);
    let payload_size = std::env::var("VORTO_LATENCY_PAYLOAD")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(64);

    let backend = TcpEchoServer::spawn().await;
    let listen = reserve_tcp_addr();
    let spec = tcp_tunnel_spec("tcp-latency", listen, backend.addr);

    let mut manager = TunnelManager::default();
    assert!(manager.reconcile(vec![spec]).await);

    let direct = measure_tcp_echo_rtts(backend.addr, warmup, samples, payload_size).await;
    let tunneled = measure_tcp_echo_rtts(listen, warmup, samples, payload_size).await;

    print_latency_stats("direct", &direct);
    print_latency_stats("socket tunnel", &tunneled);

    let direct_p95 = percentile_us(&direct, 95.0);
    let tunneled_p95 = percentile_us(&tunneled, 95.0);
    let overhead_p95 = tunneled_p95.saturating_sub(direct_p95);
    println!(
        "socket tunnel latency overhead p95: {} us (direct p95={} us, tunnel p95={} us)",
        overhead_p95, direct_p95, tunneled_p95
    );

    if let Some(max_p95_us) = std::env::var("VORTO_MAX_THROUGHPUT_P95_US")
        .ok()
        .and_then(|value| value.parse::<u128>().ok())
    {
        assert!(
            tunneled_p95 <= max_p95_us,
            "throughput tunnel p95 latency {} us exceeded configured limit {} us",
            tunneled_p95,
            max_p95_us
        );
    }

    manager.stop_all().await;
    backend.stop().await;
}

async fn measure_tcp_echo_rtts(
    addr: std::net::SocketAddr,
    warmup: usize,
    samples: usize,
    payload_size: usize,
) -> Vec<u128> {
    let payload = vec![b'x'; payload_size];
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("latency probe should connect");

    for _ in 0..warmup {
        stream
            .write_all(&payload)
            .await
            .expect("latency warmup write should succeed");
        let mut echoed = vec![0_u8; payload_size];
        stream
            .read_exact(&mut echoed)
            .await
            .expect("latency warmup read should succeed");
    }

    let mut rtts = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        stream
            .write_all(&payload)
            .await
            .expect("latency sample write should succeed");
        let mut echoed = vec![0_u8; payload_size];
        stream
            .read_exact(&mut echoed)
            .await
            .expect("latency sample read should succeed");
        rtts.push(start.elapsed().as_micros());
    }

    rtts
}

fn print_latency_stats(label: &str, samples: &[u128]) {
    let avg = samples.iter().sum::<u128>() / samples.len() as u128;
    println!(
        "{} latency us: avg={} p50={} p95={} p99={} max={}",
        label,
        avg,
        percentile_us(samples, 50.0),
        percentile_us(samples, 95.0),
        percentile_us(samples, 99.0),
        samples.iter().copied().max().unwrap_or(0)
    );
}

fn percentile_us(samples: &[u128], percentile: f64) -> u128 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = ((percentile / 100.0) * (sorted.len().saturating_sub(1) as f64)).round() as usize;
    sorted[rank]
}
