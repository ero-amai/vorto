use std::collections::{HashMap, hash_map::Entry};
use std::io;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::Once;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use tokio::io::Interest;
use tokio::io::copy_bidirectional_with_sizes;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{MissedTickBehavior, interval};

#[cfg(target_os = "linux")]
use std::net::Shutdown;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use crate::AppResult;
use crate::config::{AppConfig, TcpMode, TunnelConfig};

const UDP_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);
const UDP_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const TCP_COPY_BUFFER_SIZE: usize = 256 * 1024;
const TCP_SOCKET_BUFFER_SIZE: libc::c_int = 1_048_576;
const UDP_SOCKET_BUFFER_SIZE: libc::c_int = 4 * 1024 * 1024;
const UDP_PACKET_BUFFER_SIZE: usize = 65_535;
const UDP_DRAIN_BURST: usize = 64;
#[cfg(target_os = "linux")]
const UDP_RECV_BATCH_SIZE: usize = 32;
#[cfg(target_os = "linux")]
const SPLICE_CHUNK_SIZE: usize = 256 * 1024;
#[cfg(target_os = "linux")]
const SPLICE_PIPE_SIZE: libc::c_int = 1_048_576;
#[cfg(target_os = "linux")]
static PIPE_SIZE_WARNING: Once = Once::new();

#[derive(Default)]
pub struct TunnelManager {
    active: HashMap<String, ActiveTunnel>,
}

struct ActiveTunnel {
    spec: TunnelConfig,
    stop_tx: watch::Sender<bool>,
    handles: Vec<JoinHandle<()>>,
}

struct UdpSession {
    upstream: Arc<UdpSocket>,
    last_seen: Instant,
    stop_tx: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl TunnelManager {
    pub async fn start_from_config(&mut self, config: &AppConfig) -> bool {
        self.reconcile(config.enabled_tunnels()).await
    }

    pub async fn reconcile(&mut self, desired_tunnels: Vec<TunnelConfig>) -> bool {
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

    pub async fn stop_all(&mut self) {
        let names = self.active.keys().cloned().collect::<Vec<_>>();
        for name in names {
            if let Some(active) = self.active.remove(&name) {
                active.stop().await;
            }
        }
    }
}

impl ActiveTunnel {
    async fn spawn(spec: TunnelConfig) -> AppResult<Self> {
        let (stop_tx, stop_rx) = watch::channel(false);
        let mut handles = Vec::new();

        if spec.protocol.supports_tcp() {
            let tcp_listener = TcpListener::bind(&spec.listen).await?;
            let tcp_spec = spec.clone();
            let tcp_stop = stop_rx.clone();
            handles.push(tokio::spawn(async move {
                if let Err(error) = run_tcp_tunnel(tcp_spec, tcp_listener, tcp_stop).await {
                    eprintln!("TCP tunnel stopped: {}", error);
                }
            }));
        }

        if spec.protocol.supports_udp() {
            let udp_socket = Arc::new(UdpSocket::bind(&spec.listen).await?);
            configure_udp_socket(udp_socket.as_ref())?;
            let udp_spec = spec.clone();
            let udp_stop = stop_rx.clone();
            handles.push(tokio::spawn(async move {
                if let Err(error) = run_udp_tunnel(udp_spec, udp_socket, udp_stop).await {
                    eprintln!("UDP tunnel stopped: {}", error);
                }
            }));
        }

        Ok(Self {
            spec,
            stop_tx,
            handles,
        })
    }

    async fn stop(self) {
        let _ = self.stop_tx.send(true);
        for handle in self.handles {
            let _ = handle.await;
        }
    }
}

pub async fn run_foreground(config_path: &Path) -> AppResult<()> {
    let config = AppConfig::load_for_runtime(config_path)?;
    let mut manager = TunnelManager::default();
    if !manager.start_from_config(&config).await {
        manager.stop_all().await;
        return Err(io::Error::other("Failed to start one or more tunnels.").into());
    }
    println!("Foreground mode started. Press Ctrl+C to stop.");
    wait_for_termination_signal().await?;
    manager.stop_all().await;
    Ok(())
}

pub async fn run_config_watcher(config_path: &Path, poll_interval: Duration) -> AppResult<()> {
    let mut manager = TunnelManager::default();
    let mut last_applied_config = None::<AppConfig>;

    loop {
        match AppConfig::load_for_runtime(config_path) {
            Ok(config) => {
                if last_applied_config.as_ref() != Some(&config) {
                    let applied = manager.reconcile(config.enabled_tunnels()).await;
                    if applied {
                        last_applied_config = Some(config);
                    } else {
                        eprintln!(
                            "Will retry the current config on the next poll because some tunnels failed to start."
                        );
                    }
                }
            }
            Err(error) => {
                eprintln!(
                    "Failed to read config. Keeping current tunnels unchanged: {}",
                    error
                );
            }
        }

        tokio::select! {
            result = wait_for_termination_signal() => {
                result?;
                break;
            }
            _ = tokio::time::sleep(poll_interval) => {}
        }
    }

    manager.stop_all().await;
    Ok(())
}

async fn run_tcp_tunnel(
    spec: TunnelConfig,
    listener: TcpListener,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (inbound, _) = result?;
                let target = spec.target.clone();
                let tcp_mode = spec.tcp_mode;
                let connection_stop = stop_rx.clone();
                connections.spawn(async move {
                    if let Err(error) = handle_tcp_connection(inbound, &target, tcp_mode, connection_stop).await {
                        eprintln!("TCP connection handling failed: {}", error);
                    }
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    eprintln!("TCP connection task failed: {}", error);
                }
            }
            _ = wait_for_shutdown(&mut stop_rx) => {
                break;
            }
        }
    }

    while let Some(result) = connections.join_next().await {
        if let Err(error) = result {
            eprintln!("TCP connection task failed during shutdown: {}", error);
        }
    }

    Ok(())
}

async fn handle_tcp_connection(
    inbound: TcpStream,
    target: &str,
    tcp_mode: TcpMode,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    let outbound = tokio::select! {
        result = TcpStream::connect(target) => result?,
        _ = wait_for_shutdown(&mut stop_rx) => return Ok(()),
    };

    configure_tcp_streams(&inbound, &outbound, tcp_mode)?;

    #[cfg(target_os = "linux")]
    {
        match tcp_mode.effective() {
            TcpMode::Throughput => handle_tcp_connection_splice(inbound, outbound, stop_rx).await,
            TcpMode::Latency => handle_tcp_connection_copy(inbound, outbound, stop_rx).await,
            TcpMode::Auto => {
                unreachable!("auto mode should resolve before selecting a runtime path")
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = tcp_mode;
        handle_tcp_connection_copy(inbound, outbound, stop_rx).await
    }
}

fn configure_tcp_streams(
    inbound: &TcpStream,
    outbound: &TcpStream,
    tcp_mode: TcpMode,
) -> io::Result<()> {
    inbound.set_nodelay(true)?;
    outbound.set_nodelay(true)?;
    #[cfg(unix)]
    {
        tune_socket_buffers(
            inbound.as_raw_fd(),
            TCP_SOCKET_BUFFER_SIZE,
            TCP_SOCKET_BUFFER_SIZE,
        )?;
        tune_socket_buffers(
            outbound.as_raw_fd(),
            TCP_SOCKET_BUFFER_SIZE,
            TCP_SOCKET_BUFFER_SIZE,
        )?;
    }
    #[cfg(target_os = "linux")]
    if matches!(tcp_mode.effective(), TcpMode::Latency) {
        set_socket_option(
            inbound.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_QUICKACK,
            1,
        )?;
        set_socket_option(
            outbound.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_QUICKACK,
            1,
        )?;
    }
    Ok(())
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

async fn handle_tcp_connection_copy(
    mut inbound: TcpStream,
    mut outbound: TcpStream,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    tokio::select! {
        result = copy_bidirectional_with_sizes(
            &mut inbound,
            &mut outbound,
            TCP_COPY_BUFFER_SIZE,
            TCP_COPY_BUFFER_SIZE,
        ) => {
            result?;
        }
        _ = wait_for_shutdown(&mut stop_rx) => {}
    }

    Ok(())
}

#[cfg(target_os = "linux")]
async fn handle_tcp_connection_splice(
    inbound: TcpStream,
    outbound: TcpStream,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    let inbound = Arc::new(inbound);
    let outbound = Arc::new(outbound);

    tokio::select! {
        result = async {
            let result = tokio::try_join!(
                splice_one_way(inbound.clone(), outbound.clone()),
                splice_one_way(outbound.clone(), inbound.clone()),
            );

            if result.is_err() {
                shutdown_socket(inbound.as_ref(), Shutdown::Both);
                shutdown_socket(outbound.as_ref(), Shutdown::Both);
            }

            result
        } => {
            result?;
        }
        _ = wait_for_shutdown(&mut stop_rx) => {
            shutdown_socket(inbound.as_ref(), Shutdown::Both);
            shutdown_socket(outbound.as_ref(), Shutdown::Both);
        }
    }

    Ok(())
}

#[cfg(target_os = "linux")]
async fn splice_one_way(source: Arc<TcpStream>, destination: Arc<TcpStream>) -> io::Result<()> {
    let pipe = SplicePipe::new()?;

    loop {
        source.readable().await?;
        let copied = match source.try_io(Interest::READABLE, || {
            splice_raw(
                source.as_ref().as_raw_fd(),
                pipe.write.as_raw_fd(),
                SPLICE_CHUNK_SIZE,
            )
        }) {
            Ok(copied) => copied,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        };

        if copied == 0 {
            shutdown_socket(destination.as_ref(), Shutdown::Write);
            return Ok(());
        }

        let mut remaining = copied;
        while remaining > 0 {
            destination.writable().await?;
            let moved = match destination.try_io(Interest::WRITABLE, || {
                splice_raw(
                    pipe.read.as_raw_fd(),
                    destination.as_ref().as_raw_fd(),
                    remaining,
                )
            }) {
                Ok(moved) => moved,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error),
            };

            if moved == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "splice returned zero bytes while forwarding to the destination socket",
                ));
            }

            remaining -= moved;
        }
    }
}

#[cfg(target_os = "linux")]
fn shutdown_socket(stream: &TcpStream, how: Shutdown) {
    let _ = unsafe { libc::shutdown(stream.as_raw_fd(), shutdown_mode(how)) };
}

#[cfg(target_os = "linux")]
fn shutdown_mode(how: Shutdown) -> libc::c_int {
    match how {
        Shutdown::Read => libc::SHUT_RD,
        Shutdown::Write => libc::SHUT_WR,
        Shutdown::Both => libc::SHUT_RDWR,
    }
}

#[cfg(target_os = "linux")]
fn splice_raw(fd_in: RawFd, fd_out: RawFd, length: usize) -> io::Result<usize> {
    loop {
        let result = unsafe {
            libc::splice(
                fd_in,
                std::ptr::null_mut(),
                fd_out,
                std::ptr::null_mut(),
                length,
                libc::SPLICE_F_MOVE | libc::SPLICE_F_MORE | libc::SPLICE_F_NONBLOCK,
            )
        };

        if result >= 0 {
            return Ok(result as usize);
        }

        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }

        return Err(error);
    }
}

#[cfg(target_os = "linux")]
struct SplicePipe {
    read: OwnedFd,
    write: OwnedFd,
}

#[cfg(target_os = "linux")]
impl SplicePipe {
    fn new() -> io::Result<Self> {
        let mut fds = [0; 2];
        let flags = libc::O_CLOEXEC | libc::O_NONBLOCK;
        if unsafe { libc::pipe2(fds.as_mut_ptr(), flags) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        tune_splice_pipe_size(write.as_raw_fd());

        Ok(Self { read, write })
    }
}

#[cfg(target_os = "linux")]
fn tune_splice_pipe_size(fd: RawFd) {
    let requested = SPLICE_PIPE_SIZE;
    let result = unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, requested) };
    if result >= 0 {
        let actual = result as libc::c_int;
        if actual < requested {
            warn_pipe_size_degraded(Some(actual), None);
        }
        return;
    }

    let error = io::Error::last_os_error();
    let actual = pipe_size(fd);
    warn_pipe_size_degraded(actual, Some(error));
}

#[cfg(target_os = "linux")]
fn pipe_size(fd: RawFd) -> Option<libc::c_int> {
    let result = unsafe { libc::fcntl(fd, libc::F_GETPIPE_SZ) };
    (result >= 0).then_some(result as libc::c_int)
}

#[cfg(target_os = "linux")]
fn warn_pipe_size_degraded(actual: Option<libc::c_int>, error: Option<io::Error>) {
    PIPE_SIZE_WARNING.call_once(|| match (actual, error) {
        (Some(actual), Some(error)) => eprintln!(
            "Warning: failed to raise splice pipe size to {} bytes (current: {} bytes): {}",
            SPLICE_PIPE_SIZE, actual, error
        ),
        (None, Some(error)) => eprintln!(
            "Warning: failed to raise splice pipe size to {} bytes: {}",
            SPLICE_PIPE_SIZE, error
        ),
        (Some(actual), None) => eprintln!(
            "Warning: splice pipe size is {} bytes, below requested {} bytes.",
            actual, SPLICE_PIPE_SIZE
        ),
        (None, None) => {}
    });
}

async fn run_udp_tunnel(
    spec: TunnelConfig,
    listener: Arc<UdpSocket>,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    let mut sessions = HashMap::<std::net::SocketAddr, UdpSession>::new();
    #[cfg(target_os = "linux")]
    let mut batch = UdpRecvBatch::new(UDP_RECV_BATCH_SIZE);
    #[cfg(not(target_os = "linux"))]
    let mut buffer = vec![0_u8; UDP_PACKET_BUFFER_SIZE];
    let mut cleanup_tick = interval(UDP_CLEANUP_INTERVAL);
    cleanup_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    cleanup_tick.tick().await;

    loop {
        tokio::select! {
            readiness = listener.readable() => {
                readiness?;

                #[cfg(target_os = "linux")]
                forward_udp_listener_batch(
                    &listener,
                    spec.target.as_str(),
                    &mut sessions,
                    &stop_rx,
                    &mut batch,
                )
                .await?;

                #[cfg(not(target_os = "linux"))]
                {
                    for _ in 0..UDP_DRAIN_BURST {
                        let (size, client_addr) = match listener.try_recv_from(&mut buffer) {
                            Ok(result) => result,
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                            Err(error) => return Err(error.into()),
                        };

                        if let Entry::Vacant(entry) = sessions.entry(client_addr) {
                            let session = create_udp_session(client_addr, spec.target.clone(), listener.clone(), stop_rx.clone()).await?;
                            entry.insert(session);
                        }

                        if let Some(session) = sessions.get_mut(&client_addr) {
                            session.last_seen = Instant::now();
                            send_udp_packet(session.upstream.as_ref(), &buffer[..size]).await?;
                        }
                    }
                }
            }
            _ = cleanup_tick.tick() => {
                retire_stale_udp_sessions(&mut sessions);
            }
            _ = wait_for_shutdown(&mut stop_rx) => {
                break;
            }
        }
    }

    shutdown_udp_sessions(sessions).await;

    Ok(())
}

#[cfg(target_os = "linux")]
async fn forward_udp_listener_batch(
    listener: &Arc<UdpSocket>,
    target: &str,
    sessions: &mut HashMap<std::net::SocketAddr, UdpSession>,
    global_stop_rx: &watch::Receiver<bool>,
    batch: &mut UdpRecvBatch,
) -> AppResult<()> {
    let received = batch.recv(listener.as_ref().as_raw_fd())?;
    if received == 0 {
        return Ok(());
    }

    let mut grouped = Vec::<(std::net::SocketAddr, Vec<usize>)>::new();

    for index in 0..received {
        let client_addr = batch.client_addr(index)?;

        if let Entry::Vacant(entry) = sessions.entry(client_addr) {
            let session = create_udp_session(
                client_addr,
                target.to_string(),
                listener.clone(),
                global_stop_rx.clone(),
            )
            .await?;
            entry.insert(session);
        }

        if let Some(session) = sessions.get_mut(&client_addr) {
            session.last_seen = Instant::now();
        }

        if let Some((_, packet_indexes)) = grouped
            .iter_mut()
            .find(|(group_client_addr, _)| *group_client_addr == client_addr)
        {
            packet_indexes.push(index);
        } else {
            grouped.push((client_addr, vec![index]));
        }
    }

    for (client_addr, packet_indexes) in grouped {
        if let Some(session) = sessions.get(&client_addr) {
            send_udp_batch_connected(session.upstream.as_ref(), batch, &packet_indexes).await?;
        }
    }

    Ok(())
}

async fn create_udp_session(
    client_addr: std::net::SocketAddr,
    target: String,
    listener: Arc<UdpSocket>,
    global_stop_rx: watch::Receiver<bool>,
) -> AppResult<UdpSession> {
    let upstream = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    configure_udp_socket(upstream.as_ref())?;
    upstream.connect(&target).await?;

    let (session_stop_tx, session_stop_rx) = watch::channel(false);
    let session_upstream = upstream.clone();
    let handle = tokio::spawn(async move {
        if let Err(error) = pump_udp_responses(
            listener,
            session_upstream,
            client_addr,
            global_stop_rx,
            session_stop_rx,
        )
        .await
        {
            eprintln!("UDP session handling failed: {}", error);
        }
    });

    Ok(UdpSession {
        upstream,
        last_seen: Instant::now(),
        stop_tx: session_stop_tx,
        handle,
    })
}

fn retire_stale_udp_sessions(sessions: &mut HashMap<std::net::SocketAddr, UdpSession>) {
    let stale = sessions
        .iter()
        .filter_map(|(addr, session)| {
            if session.last_seen.elapsed() > UDP_SESSION_IDLE_TIMEOUT {
                Some(*addr)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    for addr in stale {
        if let Some(session) = sessions.remove(&addr) {
            retire_udp_session(session);
        }
    }
}

fn retire_udp_session(session: UdpSession) {
    let _ = session.stop_tx.send(true);
    tokio::spawn(async move {
        let _ = session.handle.await;
    });
}

async fn shutdown_udp_sessions(sessions: HashMap<std::net::SocketAddr, UdpSession>) {
    let mut handles = Vec::with_capacity(sessions.len());
    for (_, session) in sessions {
        let _ = session.stop_tx.send(true);
        handles.push(session.handle);
    }

    for handle in handles {
        let _ = handle.await;
    }
}

async fn pump_udp_responses(
    listener: Arc<UdpSocket>,
    upstream: Arc<UdpSocket>,
    client_addr: std::net::SocketAddr,
    mut global_stop_rx: watch::Receiver<bool>,
    mut session_stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    let mut buffer = vec![0_u8; UDP_PACKET_BUFFER_SIZE];

    loop {
        tokio::select! {
            readiness = upstream.readable() => {
                readiness?;

                for _ in 0..UDP_DRAIN_BURST {
                    let size = match upstream.try_recv(&mut buffer) {
                        Ok(size) => size,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) => return Err(error.into()),
                    };

                    send_udp_packet_to(listener.as_ref(), &buffer[..size], client_addr).await?;
                }
            }
            _ = wait_for_shutdown(&mut global_stop_rx) => {
                break;
            }
            _ = wait_for_shutdown(&mut session_stop_rx) => {
                break;
            }
        }
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn send_udp_packet(socket: &UdpSocket, payload: &[u8]) -> io::Result<()> {
    loop {
        match socket.try_send(payload) {
            Ok(sent) if sent == payload.len() => return Ok(()),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "UDP socket sent a partial datagram",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => socket.writable().await?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

async fn send_udp_packet_to(
    socket: &UdpSocket,
    payload: &[u8],
    target: std::net::SocketAddr,
) -> io::Result<()> {
    loop {
        match socket.try_send_to(payload, target) {
            Ok(sent) if sent == payload.len() => return Ok(()),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "UDP socket sent a partial datagram",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => socket.writable().await?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(target_os = "linux")]
async fn send_udp_batch_connected(
    socket: &UdpSocket,
    batch: &UdpRecvBatch,
    packet_indexes: &[usize],
) -> io::Result<()> {
    let mut start = 0;
    while start < packet_indexes.len() {
        socket.writable().await?;
        let payloads = packet_indexes[start..]
            .iter()
            .map(|index| batch.payload(*index))
            .collect::<Vec<_>>();
        match sendmmsg_connected_raw(socket.as_raw_fd(), &payloads) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "sendmmsg returned zero datagrams for a connected UDP socket",
                ));
            }
            Ok(sent) => start += sent,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
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

#[cfg(target_os = "linux")]
struct UdpRecvBatch {
    payloads: Vec<Vec<u8>>,
    addresses: Vec<libc::sockaddr_storage>,
    address_lengths: Vec<libc::socklen_t>,
    payload_lengths: Vec<usize>,
}

#[cfg(target_os = "linux")]
impl UdpRecvBatch {
    fn new(size: usize) -> Self {
        let payloads = (0..size)
            .map(|_| vec![0_u8; UDP_PACKET_BUFFER_SIZE])
            .collect::<Vec<_>>();
        let addresses = (0..size)
            .map(|_| unsafe { std::mem::zeroed::<libc::sockaddr_storage>() })
            .collect::<Vec<_>>();
        let address_lengths = vec![0; size];
        let payload_lengths = vec![0; size];

        Self {
            payloads,
            addresses,
            address_lengths,
            payload_lengths,
        }
    }

    fn recv(&mut self, fd: RawFd) -> io::Result<usize> {
        let mut iovecs = (0..self.payloads.len())
            .map(|_| unsafe { std::mem::zeroed::<libc::iovec>() })
            .collect::<Vec<_>>();
        let mut messages = (0..self.payloads.len())
            .map(|_| unsafe { std::mem::zeroed::<libc::mmsghdr>() })
            .collect::<Vec<_>>();

        for index in 0..self.payloads.len() {
            self.addresses[index] = unsafe { std::mem::zeroed() };
            self.address_lengths[index] = 0;
            self.payload_lengths[index] = 0;
            iovecs[index] = libc::iovec {
                iov_base: self.payloads[index].as_mut_ptr().cast(),
                iov_len: self.payloads[index].len(),
            };
            messages[index] = libc::mmsghdr {
                msg_hdr: libc::msghdr {
                    msg_name: (&mut self.addresses[index] as *mut libc::sockaddr_storage).cast(),
                    msg_namelen: std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
                    msg_iov: (&mut iovecs[index] as *mut libc::iovec).cast(),
                    msg_iovlen: 1,
                    msg_control: std::ptr::null_mut(),
                    msg_controllen: 0,
                    msg_flags: 0,
                },
                msg_len: 0,
            };
        }

        loop {
            let result = unsafe {
                libc::recvmmsg(
                    fd,
                    messages.as_mut_ptr(),
                    messages.len() as u32,
                    libc::MSG_DONTWAIT,
                    std::ptr::null_mut(),
                )
            };

            if result >= 0 {
                let received = result as usize;
                for (index, message) in messages.iter().enumerate().take(received) {
                    self.address_lengths[index] = message.msg_hdr.msg_namelen;
                    self.payload_lengths[index] = message.msg_len as usize;
                }
                return Ok(received);
            }

            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(0),
                _ => return Err(error),
            }
        }
    }

    fn payload(&self, index: usize) -> &[u8] {
        &self.payloads[index][..self.payload_lengths[index]]
    }

    fn client_addr(&self, index: usize) -> io::Result<std::net::SocketAddr> {
        socket_addr_from_storage(&self.addresses[index], self.address_lengths[index])
    }
}

#[cfg(target_os = "linux")]
fn sendmmsg_connected_raw(fd: RawFd, payloads: &[&[u8]]) -> io::Result<usize> {
    let mut iovecs = payloads
        .iter()
        .map(|payload| libc::iovec {
            iov_base: payload.as_ptr().cast_mut().cast(),
            iov_len: payload.len(),
        })
        .collect::<Vec<_>>();
    let mut messages = iovecs
        .iter_mut()
        .map(|iov| libc::mmsghdr {
            msg_hdr: libc::msghdr {
                msg_name: std::ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: (iov as *mut libc::iovec).cast(),
                msg_iovlen: 1,
                msg_control: std::ptr::null_mut(),
                msg_controllen: 0,
                msg_flags: 0,
            },
            msg_len: 0,
        })
        .collect::<Vec<_>>();

    loop {
        let result = unsafe {
            libc::sendmmsg(
                fd,
                messages.as_mut_ptr(),
                messages.len() as u32,
                libc::MSG_DONTWAIT,
            )
        };

        if result >= 0 {
            return Ok(result as usize);
        }

        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }

        return Err(error);
    }
}

#[cfg(target_os = "linux")]
fn socket_addr_from_storage(
    storage: &libc::sockaddr_storage,
    name_len: libc::socklen_t,
) -> io::Result<std::net::SocketAddr> {
    if name_len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing UDP peer address from recvmmsg",
        ));
    }

    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            let addr =
                unsafe { *(storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
            Ok(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)),
                u16::from_be(addr.sin_port),
            )))
        }
        libc::AF_INET6 => {
            let addr =
                unsafe { *(storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>() };
            Ok(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                std::net::Ipv6Addr::from(addr.sin6_addr.s6_addr),
                u16::from_be(addr.sin6_port),
                addr.sin6_flowinfo,
                addr.sin6_scope_id,
            )))
        }
        family => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported UDP socket family: {}", family),
        )),
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

async fn wait_for_termination_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {}
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}
