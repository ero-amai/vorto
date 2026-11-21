use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream, UdpSocket, lookup_host};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{MissedTickBehavior, interval, timeout};
use udpproxi::{UdpProxi, UdpProxiReceiver, UdpProxiSender};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use crate::AppResult;
use crate::config::{AppConfig, AppMode, TunnelConfig};
use crate::nft::NftManager;

const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(3);
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UDP_SOCKET_BUFFER_SIZE: libc::c_int = 4 * 1024 * 1024;
const UDP_PACKET_BUFFER_SIZE: usize = 65_535;

#[derive(Default)]
pub struct TunnelManager {
    active: HashMap<String, ActiveTunnel>,
}

pub(crate) struct ActiveTunnel {
    spec: TunnelConfig,
    stop_tx: watch::Sender<bool>,
    handles: Vec<JoinHandle<()>>,
}

#[derive(Clone)]
struct UdpResponseSink {
    socket: Weak<UdpSocket>,
}

struct ConnectedUdpEndpoint {
    socket: UdpSocket,
}

impl TunnelManager {
    pub async fn start_from_config(&mut self, config: &AppConfig) -> bool {
        self.reconcile(config.enabled_tunnels()).await
    }

    pub async fn reconcile(&mut self, desired_tunnels: Vec<TunnelConfig>) -> bool {
        self.retire_finished_tunnels().await;

        let desired_map = desired_tunnels
            .into_iter()
            .map(|tunnel| (tunnel.name.clone(), tunnel))
            .collect::<HashMap<_, _>>();
        let mut all_started = true;

        let mut to_remove = Vec::new();
        for (name, active) in &self.active {
            match desired_map.get(name) {
                Some(next) if *next == active.spec => {}
                _ => to_remove.push(name.clone()),
            }
        }

        for name in to_remove {
            if let Some(active) = self.active.remove(&name) {
                active.stop().await;
            }
        }

        for (name, tunnel) in desired_map {
            let should_start = match self.active.get(&name) {
                Some(active) => active.spec != tunnel,
                None => true,
            };

            if should_start {
                match ActiveTunnel::spawn(tunnel.clone()).await {
                    Ok(active) => {
                        self.active.insert(name, active);
                    }
                    Err(error) => {
                        all_started = false;
                        eprintln!("Failed to start tunnel '{}': {}", tunnel.name, error);
                    }
                }
            }
        }

        all_started
    }

    async fn retire_finished_tunnels(&mut self) {
        let finished = self
            .active
            .iter()
            .filter_map(|(name, active)| active.has_finished().then_some(name.clone()))
            .collect::<Vec<_>>();

        for name in finished {
            eprintln!(
                "Tunnel '{}' worker exited unexpectedly; recycling the tunnel state.",
                name
            );
            if let Some(active) = self.active.remove(&name) {
                active.stop().await;
            }
        }
    }

    pub async fn stop_all(&mut self) {
        let active_tunnels = self
            .active
            .drain()
            .map(|(_, active)| active)
            .collect::<Vec<_>>();
        let mut shutdown_tasks = JoinSet::new();

        for active in active_tunnels {
            shutdown_tasks.spawn(async move {
                active.stop().await;
            });
        }

        while let Some(result) = shutdown_tasks.join_next().await {
            if let Err(error) = result {
                eprintln!("Tunnel shutdown task failed: {}", error);
            }
        }
    }
}

impl ActiveTunnel {
    async fn spawn(spec: TunnelConfig) -> AppResult<Self> {
        let (tunnel, _) = Self::spawn_bound(spec).await?;
        Ok(tunnel)
    }

    pub(crate) async fn spawn_bound(spec: TunnelConfig) -> AppResult<(Self, u16)> {
        let tcp_listener = if spec.protocol.supports_tcp() {
            Some(build_tcp_listener(&spec.listen).await?)
        } else {
            None
        };
        let udp_endpoint = if spec.protocol.supports_udp() {
            let target = resolve_socket_addr(&spec.target, "target").await?;
            let listener = Arc::new(UdpSocket::bind(&spec.listen).await?);
            configure_udp_socket(&listener)?;
            Some((listener, target))
        } else {
            None
        };
        let listen_port = if let Some(listener) = &tcp_listener {
            listener.local_addr()?.port()
        } else if let Some((listener, _)) = &udp_endpoint {
            listener.local_addr()?.port()
        } else {
            unreachable!("every tunnel supports TCP or UDP");
        };

        let (stop_tx, stop_rx) = watch::channel(false);
        let mut handles = Vec::new();

        if let Some(listener) = tcp_listener {
            let target = spec.target.clone();
            let name = spec.name.clone();
            let stop = stop_rx.clone();
            handles.push(tokio::spawn(async move {
                if let Err(error) = run_tcp_tunnel(listener, &target, stop).await {
                    eprintln!("TCP tunnel '{}' stopped: {}", name, error);
                }
            }));
        }

        if let Some((listener, target)) = udp_endpoint {
            let name = spec.name.clone();
            handles.push(tokio::spawn(async move {
                if let Err(error) = run_udp_tunnel(name.clone(), listener, target, stop_rx).await {
                    eprintln!("UDP tunnel '{}' stopped: {}", name, error);
                }
            }));
        }

        Ok((
            Self {
                spec,
                stop_tx,
                handles,
            },
            listen_port,
        ))
    }

    pub(crate) fn has_finished(&self) -> bool {
        self.handles.iter().any(JoinHandle::is_finished)
    }

    pub(crate) async fn stop(self) {
        let _ = self.stop_tx.send(true);
        let mut shutdown_tasks = JoinSet::new();

        for handle in self.handles {
            shutdown_tasks.spawn(async move {
                wait_for_task_shutdown(handle, "tunnel worker").await;
            });
        }

        while let Some(result) = shutdown_tasks.join_next().await {
            if let Err(error) = result {
                eprintln!("Tunnel worker shutdown task failed: {}", error);
            }
        }
    }
}

async fn reconcile_config(
    config: &AppConfig,
    socket_manager: &mut TunnelManager,
    nft_manager: &mut NftManager,
) -> AppResult<()> {
    match config.mode {
        AppMode::Socket => {
            if !socket_manager.start_from_config(config).await {
                return Err(io::Error::other("Failed to start one or more socket tunnels.").into());
            }
            nft_manager.stop_all().await?;
        }
        AppMode::Nft => {
            nft_manager.reconcile(&config.enabled_tunnels()).await?;
            socket_manager.stop_all().await;
        }
    }
    Ok(())
}

pub async fn run_foreground(config_path: &Path) -> AppResult<()> {
    let config = AppConfig::load_for_runtime(config_path)?;
    let mut socket_manager = TunnelManager::default();
    let mut nft_manager = NftManager::new();
    let termination = termination_signal()?;
    tokio::pin!(termination);

    if let Err(error) = reconcile_config(&config, &mut socket_manager, &mut nft_manager).await {
        socket_manager.stop_all().await;
        return Err(error);
    }
    println!("Foreground mode started. Press Ctrl+C to stop.");

    let mut health_tick = interval(HEALTH_CHECK_INTERVAL);
    health_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    health_tick.tick().await;

    let result = loop {
        tokio::select! {
            result = &mut termination => break result,
            _ = health_tick.tick() => {
                if let Err(error) = reconcile_config(&config, &mut socket_manager, &mut nft_manager).await {
                    eprintln!("Forwarding health check failed. Will retry: {error}");
                }
            }
        }
    };

    socket_manager.stop_all().await;
    let cleanup = nft_manager.stop_all().await;
    result?;
    cleanup
}

pub async fn run_config_watcher(
    config_path: &Path,
    poll_interval: Duration,
    mut report_status: impl FnMut(Option<&str>) -> AppResult<()>,
) -> AppResult<()> {
    let mut socket_manager = TunnelManager::default();
    let mut nft_manager = NftManager::new();
    let termination = termination_signal()?;
    tokio::pin!(termination);

    let mut previous_status = None;
    let result = loop {
        let applied = match AppConfig::load_for_runtime(config_path) {
            Ok(config) => reconcile_config(&config, &mut socket_manager, &mut nft_manager).await,
            Err(error) => Err(error),
        };
        let error = match applied {
            Ok(()) => None,
            Err(error) => {
                let mut message = error.to_string();
                // A broken new config must not prevent repair of the last installed rules.
                if let Err(recovery_error) = nft_manager.recover_active().await
                    && recovery_error.to_string() != message
                {
                    use std::fmt::Write as _;
                    write!(
                        message,
                        "\nFailed to recover installed nft rules: {recovery_error}"
                    )
                    .unwrap();
                }
                Some(message)
            }
        };
        if previous_status.as_ref() != Some(&error) {
            if let Some(error) = &error {
                eprintln!("Forwarding failed. Will retry: {error}");
            }
            if let Err(error) = report_status(error.as_deref()) {
                break Err(io::Error::other(error));
            }
            previous_status = Some(error);
        }

        tokio::select! {
            result = &mut termination => break result,
            _ = tokio::time::sleep(poll_interval) => {}
        }
    };

    socket_manager.stop_all().await;
    let cleanup = nft_manager.stop_all().await;
    result?;
    cleanup
}

async fn build_tcp_listener(listen: &str) -> AppResult<TcpListener> {
    Ok(TcpListener::bind(listen).await?)
}

async fn resolve_socket_addr(value: &str, label: &str) -> AppResult<SocketAddr> {
    if let Ok(addr) = value.parse::<SocketAddr>() {
        return Ok(addr);
    }

    let mut addrs = lookup_host(value).await.map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Failed to resolve {label} address {value}: {error}"),
        )
    })?;
    addrs.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Failed to resolve {label} address {value}: no addresses found"),
        )
        .into()
    })
}

async fn run_tcp_tunnel(
    listener: TcpListener,
    target: &str,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    let mut connections = JoinSet::new();
    let target = target.to_string();

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (inbound, _) = match result {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        if *stop_rx.borrow() {
                            break;
                        }
                        return Err(error.into());
                    }
                };

                let connection_target = target.clone();
                let connection_stop = stop_rx.clone();
                connections.spawn(async move {
                    if let Err(error) =
                        handle_tcp_connection(inbound, &connection_target, connection_stop).await
                    {
                        eprintln!("TCP tunnel connection failed: {}", error);
                    }
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result
                    && !error.is_cancelled()
                {
                    eprintln!("TCP connection task failed: {}", error);
                }
            }
            _ = wait_for_shutdown(&mut stop_rx) => {
                break;
            }
        }
    }

    drain_tcp_connections(&mut connections).await;

    Ok(())
}

async fn handle_tcp_connection(
    inbound: TcpStream,
    target: &str,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    let outbound = tokio::select! {
        result = timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(target)) => {
            match result {
                Ok(connect_result) => connect_result?,
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("timed out connecting to target {target} after {:?}", TCP_CONNECT_TIMEOUT),
                    ).into());
                }
            }
        }
        _ = wait_for_shutdown(&mut stop_rx) => return Ok(()),
    };

    configure_tcp_streams(&inbound, &outbound)?;
    handle_tcp_connection_copy(inbound, outbound, stop_rx).await
}

fn configure_tcp_streams(inbound: &TcpStream, outbound: &TcpStream) -> io::Result<()> {
    inbound.set_nodelay(true)?;
    outbound.set_nodelay(true)?;
    Ok(())
}

async fn handle_tcp_connection_copy(
    mut inbound: TcpStream,
    mut outbound: TcpStream,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    tokio::select! {
        result = copy_bidirectional(&mut inbound, &mut outbound) => {
            result?;
        }
        _ = wait_for_shutdown(&mut stop_rx) => {}
    }

    Ok(())
}

async fn drain_tcp_connections(connections: &mut JoinSet<()>) {
    while !connections.is_empty() {
        match timeout(SHUTDOWN_GRACE_PERIOD, connections.join_next()).await {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(error))) => {
                if !error.is_cancelled() {
                    eprintln!("TCP connection task failed during shutdown: {}", error);
                }
            }
            Ok(None) => break,
            Err(_) => {
                eprintln!(
                    "Warning: TCP connection tasks did not stop within {:?}; aborting them.",
                    SHUTDOWN_GRACE_PERIOD
                );
                connections.abort_all();
                while let Some(result) = connections.join_next().await {
                    if let Err(error) = result
                        && !error.is_cancelled()
                    {
                        eprintln!("TCP connection task failed after abort: {}", error);
                    }
                }
                break;
            }
        }
    }
}

async fn run_udp_tunnel(
    tunnel_name: String,
    listener: Arc<UdpSocket>,
    target: SocketAddr,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    let response_sink = UdpResponseSink {
        socket: Arc::downgrade(&listener),
    };
    let mut proxy = UdpProxi::new(response_sink, |_from, to| async move {
        Ok(ConnectedUdpEndpoint {
            socket: build_udp_upstream_socket(to).await?,
        })
    });
    let mut buffer = vec![0_u8; UDP_PACKET_BUFFER_SIZE];

    loop {
        tokio::select! {
            result = listener.recv_from(&mut buffer) => {
                let (size, client_addr) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        if *stop_rx.borrow() {
                            break;
                        }
                        return Err(error.into());
                    }
                };

                if let Err(error) = proxy.send_packet(&buffer[..size], client_addr, target).await {
                    if *stop_rx.borrow() {
                        break;
                    }
                    eprintln!("UDP tunnel '{}' forward failed: {}", tunnel_name, error);
                }
            }
            _ = wait_for_shutdown(&mut stop_rx) => {
                break;
            }
        }
    }

    Ok(())
}

async fn build_udp_upstream_socket(target: SocketAddr) -> io::Result<UdpSocket> {
    let bind_addr = match target {
        SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let socket = UdpSocket::bind(bind_addr).await?;
    configure_udp_socket(&socket)?;
    socket.connect(target).await?;
    Ok(socket)
}

impl UdpProxiSender for ConnectedUdpEndpoint {
    fn send<'a>(
        &'a self,
        packet: &'a [u8],
        _from: SocketAddr,
        _to: SocketAddr,
    ) -> impl Future<Output = io::Result<()>> + 'a + Send {
        async move {
            let sent = self.socket.send(packet).await?;
            if sent == packet.len() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "UDP socket sent a partial datagram",
                ))
            }
        }
    }
}

impl UdpProxiReceiver for ConnectedUdpEndpoint {
    fn recv<'a>(
        &'a self,
        buff: &'a mut [u8],
    ) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + 'a + Send {
        async move {
            let len = self.socket.recv(buff).await?;
            let peer = self.socket.peer_addr()?;
            Ok((len, peer))
        }
    }
}

impl UdpProxiSender for UdpResponseSink {
    fn send<'a>(
        &'a self,
        packet: &'a [u8],
        _from: SocketAddr,
        to: SocketAddr,
    ) -> impl Future<Output = io::Result<()>> + 'a + Send {
        async move {
            let Some(socket) = self.socket.upgrade() else {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "UDP listener closed",
                ));
            };

            let sent = socket.send_to(packet, to).await?;
            if sent == packet.len() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "UDP socket sent a partial datagram",
                ))
            }
        }
    }
}

fn configure_udp_socket(socket: &UdpSocket) -> io::Result<()> {
    #[cfg(unix)]
    {
        tune_socket_buffers(
            socket.as_raw_fd(),
            UDP_SOCKET_BUFFER_SIZE,
            UDP_SOCKET_BUFFER_SIZE,
        )?;
    }

    Ok(())
}

#[cfg(unix)]
fn tune_socket_buffers(
    fd: libc::c_int,
    recv_bytes: libc::c_int,
    send_bytes: libc::c_int,
) -> io::Result<()> {
    set_socket_option(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, recv_bytes)?;
    set_socket_option(fd, libc::SOL_SOCKET, libc::SO_SNDBUF, send_bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn tune_socket_buffers(_fd: i32, _recv_bytes: i32, _send_bytes: i32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_socket_option(
    fd: libc::c_int,
    level: libc::c_int,
    name: libc::c_int,
    value: libc::c_int,
) -> io::Result<()> {
    let result = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            &value as *const _ as *const libc::c_void,
            std::mem::size_of_val(&value) as libc::socklen_t,
        )
    };

    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

async fn wait_for_task_shutdown(handle: JoinHandle<()>, label: &str) {
    let mut handle = Box::pin(handle);
    let abort_handle = handle.abort_handle();

    match timeout(SHUTDOWN_GRACE_PERIOD, &mut handle).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            if !error.is_cancelled() {
                eprintln!("{label} failed while stopping: {}", error);
            }
        }
        Err(_) => {
            eprintln!(
                "Warning: {label} did not stop within {:?}; aborting it.",
                SHUTDOWN_GRACE_PERIOD
            );
            abort_handle.abort();
            if let Err(error) = handle.await
                && !error.is_cancelled()
            {
                eprintln!("{label} failed after abort: {}", error);
            }
        }
    }
}

async fn wait_for_shutdown(stop_rx: &mut watch::Receiver<bool>) {
    if *stop_rx.borrow() {
        return;
    }

    while stop_rx.changed().await.is_ok() {
        if *stop_rx.borrow() {
            return;
        }
    }
}

fn termination_signal() -> io::Result<impl Future<Output = io::Result<()>>> {
    #[cfg(unix)]
    let (mut terminate, mut interrupt) = {
        use tokio::signal::unix::{SignalKind, signal};
        (
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        )
    };

    Ok(async move {
        #[cfg(unix)]
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await?;
        Ok(())
    })
}

#[cfg(test)]
#[path = "runtime_tests/mod.rs"]
mod tests;
