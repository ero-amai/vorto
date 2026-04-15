use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};

use super::super::*;
use crate::config::Protocol;

pub(super) struct TcpEchoServer {
    pub(super) addr: SocketAddr,
    stop_tx: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl TcpEchoServer {
    pub(super) async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("TCP echo listener should bind");
        let addr = listener
            .local_addr()
            .expect("TCP echo listener should expose an address");
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (mut stream, _) = match result {
                            Ok(result) => result,
                            Err(_) => break,
                        };
                        connections.spawn(async move {
                            let mut buffer = vec![0_u8; 16 * 1024];
                            loop {
                                let read = match stream.read(&mut buffer).await {
                                    Ok(0) => break,
                                    Ok(read) => read,
                                    Err(_) => break,
                                };

                                if stream.write_all(&buffer[..read]).await.is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                    _ = wait_for_shutdown(&mut stop_rx) => {
                        connections.abort_all();
                        while connections.join_next().await.is_some() {}
                        break;
                    }
                }
            }
        });

        Self {
            addr,
            stop_tx,
            handle,
        }
    }

    pub(super) async fn stop(self) {
        let _ = self.stop_tx.send(true);
        wait_for_task_shutdown(self.handle, "test TCP echo server").await;
    }
}

pub(super) struct TcpDropFirstServer {
    pub(super) addr: SocketAddr,
    stop_tx: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl TcpDropFirstServer {
    pub(super) async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("TCP flaky listener should bind");
        let addr = listener
            .local_addr()
            .expect("TCP flaky listener should expose an address");
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let dropped_once = Arc::new(AtomicBool::new(false));

        let handle = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (mut stream, _) = match result {
                            Ok(result) => result,
                            Err(_) => break,
                        };
                        let dropped_once = dropped_once.clone();
                        connections.spawn(async move {
                            if !dropped_once.swap(true, Ordering::SeqCst) {
                                let mut buffer = [0_u8; 1024];
                                let _ = stream.read(&mut buffer).await;
                                return;
                            }

                            let mut buffer = vec![0_u8; 16 * 1024];
                            loop {
                                let read = match stream.read(&mut buffer).await {
                                    Ok(0) => break,
                                    Ok(read) => read,
                                    Err(_) => break,
                                };

                                if stream.write_all(&buffer[..read]).await.is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                    _ = wait_for_shutdown(&mut stop_rx) => {
                        connections.abort_all();
                        while connections.join_next().await.is_some() {}
                        break;
                    }
                }
            }
        });

        Self {
            addr,
            stop_tx,
            handle,
        }
    }

    pub(super) async fn stop(self) {
        let _ = self.stop_tx.send(true);
        wait_for_task_shutdown(self.handle, "test TCP flaky server").await;
    }
}

pub(super) struct TcpResetServer {
    pub(super) addr: SocketAddr,
    stop_tx: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl TcpResetServer {
    pub(super) async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("TCP reset listener should bind");
        let addr = listener
            .local_addr()
            .expect("TCP reset listener should expose an address");
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (stream, _) = match result {
                            Ok(result) => result,
                            Err(_) => break,
                        };
                        let _ = force_tcp_reset(&stream);
                        drop(stream);
                    }
                    _ = wait_for_shutdown(&mut stop_rx) => break,
                }
            }
        });

        Self {
            addr,
            stop_tx,
            handle,
        }
    }

    pub(super) async fn stop(self) {
        let _ = self.stop_tx.send(true);
        wait_for_task_shutdown(self.handle, "test TCP reset server").await;
    }
}

pub(super) struct TcpHalfOpenServer {
    pub(super) addr: SocketAddr,
    stop_tx: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl TcpHalfOpenServer {
    pub(super) async fn spawn(hold_time: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("TCP half-open listener should bind");
        let addr = listener
            .local_addr()
            .expect("TCP half-open listener should expose an address");
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (stream, _) = match result {
                            Ok(result) => result,
                            Err(_) => break,
                        };
                        connections.spawn(async move {
                            tokio::time::sleep(hold_time).await;
                            drop(stream);
                        });
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                    _ = wait_for_shutdown(&mut stop_rx) => {
                        connections.abort_all();
                        while connections.join_next().await.is_some() {}
                        break;
                    }
                }
            }
        });

        Self {
            addr,
            stop_tx,
            handle,
        }
    }

    pub(super) async fn stop(self) {
        let _ = self.stop_tx.send(true);
        wait_for_task_shutdown(self.handle, "test TCP half-open server").await;
    }
}

pub(super) struct UdpEchoServer {
    pub(super) addr: SocketAddr,
    stop_tx: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl UdpEchoServer {
    pub(super) async fn spawn() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("UDP echo socket should bind");
        let addr = socket
            .local_addr()
            .expect("UDP echo socket should expose an address");
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut buffer = vec![0_u8; UDP_PACKET_BUFFER_SIZE];
            loop {
                tokio::select! {
                    result = socket.recv_from(&mut buffer) => {
                        let (size, peer) = match result {
                            Ok(result) => result,
                            Err(_) => break,
                        };

                        if socket.send_to(&buffer[..size], peer).await.is_err() {
                            break;
                        }
                    }
                    _ = wait_for_shutdown(&mut stop_rx) => break,
                }
            }
        });

        Self {
            addr,
            stop_tx,
            handle,
        }
    }

    pub(super) async fn stop(self) {
        let _ = self.stop_tx.send(true);
        wait_for_task_shutdown(self.handle, "test UDP echo server").await;
    }
}

pub(super) struct IperfServer {
    child: Child,
}

impl IperfServer {
    pub(super) fn spawn(port: u16) -> io::Result<Self> {
        let child = std::process::Command::new("iperf3")
            .args(["-s", "-p", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self { child })
    }
}

impl Drop for IperfServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub(super) fn reserve_tcp_addr() -> SocketAddr {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral TCP listener should bind");
    let addr = listener
        .local_addr()
        .expect("ephemeral TCP listener should expose an address");
    drop(listener);
    addr
}

pub(super) fn reserve_udp_addr() -> SocketAddr {
    let socket =
        std::net::UdpSocket::bind("127.0.0.1:0").expect("ephemeral UDP socket should bind");
    let addr = socket
        .local_addr()
        .expect("ephemeral UDP socket should expose an address");
    drop(socket);
    addr
}

pub(super) fn tcp_tunnel_spec(
    name: &str,
    listen: SocketAddr,
    target: SocketAddr,
    tcp_mode: TcpMode,
) -> TunnelConfig {
    TunnelConfig {
        name: name.to_string(),
        listen: listen.to_string(),
        target: target.to_string(),
        protocol: Protocol::Tcp,
        tcp_mode,
        enabled: true,
    }
}

pub(super) fn udp_tunnel_spec(name: &str, listen: SocketAddr, target: SocketAddr) -> TunnelConfig {
    TunnelConfig {
        name: name.to_string(),
        listen: listen.to_string(),
        target: target.to_string(),
        protocol: Protocol::Udp,
        tcp_mode: TcpMode::Auto,
        enabled: true,
    }
}

pub(super) async fn assert_tcp_round_trip(addr: SocketAddr, payload: &[u8]) {
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("tunnel should accept a TCP connection");
    stream
        .write_all(payload)
        .await
        .expect("payload should be forwarded");
    let mut echoed = vec![0_u8; payload.len()];
    stream
        .read_exact(&mut echoed)
        .await
        .expect("payload should be echoed back");
    assert_eq!(echoed, payload);
}

pub(super) async fn assert_udp_round_trip(addr: SocketAddr, payload: &[u8]) {
    let socket = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("UDP client socket should bind");
    socket
        .send_to(payload, addr)
        .await
        .expect("payload should be sent");
    let mut echoed = vec![0_u8; payload.len()];
    let (size, _) = timeout(Duration::from_secs(3), socket.recv_from(&mut echoed))
        .await
        .expect("UDP reply should arrive")
        .expect("UDP reply should be readable");
    assert_eq!(&echoed[..size], payload);
}

pub(super) async fn assert_socket_closes_without_forwarding(mut stream: TcpStream) {
    let mut buffer = [0_u8; 16];
    match timeout(Duration::from_secs(1), stream.read(&mut buffer))
        .await
        .expect("broken tunnel read should complete")
    {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
            ) => {}
        Ok(read) => {
            panic!("broken tunnel should not forward data, got {read} bytes instead")
        }
        Err(error) => panic!("unexpected broken tunnel error: {error}"),
    }
}

#[cfg(unix)]
fn force_tcp_reset(stream: &TcpStream) -> io::Result<()> {
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    let result = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &linger as *const _ as *const libc::c_void,
            std::mem::size_of_val(&linger) as libc::socklen_t,
        )
    };

    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn force_tcp_reset(_stream: &TcpStream) -> io::Result<()> {
    Ok(())
}
