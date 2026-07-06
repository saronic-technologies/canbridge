use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use netlink_packet_core::{NetlinkMessage, NetlinkPayload};
use netlink_packet_route::link::{LinkAttribute, LinkFlags, LinkMessage, State};
use netlink_packet_route::RouteNetlinkMessage;
use netlink_sys::{protocols::NETLINK_ROUTE, Socket as NetlinkSocket, SocketAddr as NetlinkAddr};
use socketcan::{CanFdSocket, Socket};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// CAN-TCP Bridge - Forward CAN frames over TCP bidirectionally
#[derive(Parser, Debug)]
#[command(name = "canbridge", version, about)]
struct Args {
    /// Mode of operation
    #[arg(short, long)]
    mode: Mode,

    /// Address to listen on or connect to (host:port)
    #[arg(short, long)]
    addr: String,

    /// CAN interface name (e.g., can0, vcan0)
    #[arg(short, long)]
    iface: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Mode {
    Listen,
    Connect,
}

// Import wire protocol functions from lib
use canbridge::{
    can_to_wire, frame_hash, recv_wire, send_wire, wire_to_can,
};

/// How long a CAN read blocks before returning so the loop can check whether teardown was requested.
const CAN_READ_TIMEOUT: Duration = Duration::from_millis(500);
/// Netlink multicast group for link (interface) state-change events (`RTNLGRP_LINK`).
const RTNLGRP_LINK: u32 = 1;
/// Receive buffer for the netlink link-event socket.
const NETLINK_RECV_BUF: usize = 8192;

/// Up/down decision from a link's operstate (with an `IFF_UP` fallback when operstate is absent),
/// mirroring the old sysfs rule: CAN reports `Unknown` when up, so only Down/LowerLayerDown/
/// NotPresent count as down. Pure so it stays unit-testable without a socket.
fn link_is_up(oper: Option<State>, flags: LinkFlags) -> bool {
    match oper {
        Some(State::Down) | Some(State::LowerLayerDown) | Some(State::NotPresent) => false,
        Some(_) => true,
        None => flags.contains(LinkFlags::Up),
    }
}

fn link_oper_state(link: &LinkMessage) -> Option<State> {
    link.attributes.iter().find_map(|a| match a {
        LinkAttribute::OperState(s) => Some(*s),
        _ => None,
    })
}

fn link_name(link: &LinkMessage) -> Option<&str> {
    link.attributes.iter().find_map(|a| match a {
        LinkAttribute::IfName(n) => Some(n.as_str()),
        _ => None,
    })
}

/// Process every netlink message in one datagram, updating `prev_up`. Returns true if a genuine
/// down→up transition for `iface` was observed (the caller should exit so systemd reopens a fresh
/// socket). Pure (no socket, no `process::exit`) so the multi-message + `NLMSG_ALIGN` walk is
/// unit-testable without a live netlink socket.
fn process_link_datagram(buf: &[u8], iface: &str, prev_up: &mut Option<bool>) -> bool {
    let mut off = 0;
    while off < buf.len() {
        let msg = match NetlinkMessage::<RouteNetlinkMessage>::deserialize(&buf[off..]) {
            Ok(m) => m,
            Err(_) => break,
        };
        let len = msg.header.length as usize;
        if len == 0 {
            break;
        }
        match &msg.payload {
            NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewLink(link))
                if link_name(link) == Some(iface) =>
            {
                let up = link_is_up(link_oper_state(link), link.header.flags);
                if up && *prev_up == Some(false) {
                    return true;
                }
                *prev_up = Some(up);
            }
            NetlinkPayload::InnerMessage(RouteNetlinkMessage::DelLink(link))
                if link_name(link) == Some(iface) =>
            {
                *prev_up = Some(false);
            }
            _ => {}
        }
        off += (len + 3) & !3; // NLMSG_ALIGN to the next concatenated message
    }
    false
}

/// Watch the CAN interface over netlink and force a process exit when it flaps down→up, so systemd
/// reopens a fresh socket. A raw CAN socket stays "open" across an interface down/up but goes stale
/// (frames stop) and the daemon would otherwise never notice. Runs for the life of the process.
fn spawn_link_monitor(iface: &str) {
    let iface = iface.to_string();
    thread::spawn(move || loop {
        // Self-healing: a returned error (socket setup or a fatal recv error) must never permanently
        // disable flap detection, so log and retry with a fresh socket rather than exiting the thread.
        if let Err(e) = run_link_monitor(&iface) {
            error!(error = %e, interface = %iface, "link monitor error; retrying in 1s");
        }
        thread::sleep(Duration::from_secs(1)); // avoid a tight spin if setup keeps failing
    });
}

fn run_link_monitor(iface: &str) -> Result<()> {
    let mut socket = NetlinkSocket::new(NETLINK_ROUTE).context("open netlink socket")?;
    socket.bind(&NetlinkAddr::new(0, 0)).context("bind netlink socket")?;
    socket
        .add_membership(RTNLGRP_LINK)
        .context("subscribe to link events")?;

    // `None` until we first observe the interface. We only restart on a genuine down→up transition
    // (a raw CAN socket held across a down/up goes stale). The first observation just seeds the
    // baseline, so an already-up interface and benign NEWLINK refreshes (e.g. from dhcpcd bringing
    // the link up or address changes) never trigger a restart.
    let mut prev_up: Option<bool> = None;
    let mut buf: Vec<u8> = Vec::with_capacity(NETLINK_RECV_BUF);
    loop {
        buf.clear(); // reset len to 0 so recv_from reuses the full capacity
        let n = match socket.recv_from(&mut buf, 0) {
            Ok((n, _)) => n,
            // Interrupted is benign; ENOBUFS means the receive buffer overflowed and messages were
            // dropped, but the socket stays valid — keep going rather than tearing it down.
            Err(e)
                if e.kind() == std::io::ErrorKind::Interrupted
                    || e.raw_os_error() == Some(libc::ENOBUFS) =>
            {
                continue;
            }
            Err(e) => return Err(e).context("recv netlink message"),
        };

        if process_link_datagram(&buf[..n], iface, &mut prev_up) {
            warn!(
                interface = %iface,
                "CAN interface came back up; exiting so systemd reopens a fresh socket"
            );
            std::process::exit(1);
        }
    }
}

/// Run the CAN → TCP forwarding loop with deduplication
///
/// This function reads frames from the CAN bus and forwards them over TCP.
/// It maintains a local HashSet of recently sent frame hashes (received via channel)
/// to filter out frames that we transmitted ourselves (which appear due to kernel loopback).
fn can_to_tcp_loop(
    socket: CanFdSocket,
    mut stream: TcpStream,
    iface: &str,
    sent_frames_rx: Receiver<u64>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    info!(interface = %iface, "[CAN→TCP] Starting forwarding loop");

    // Local map of frames we've recently sent with their timestamps
    let mut sent_frames: HashMap<u64, Instant> = HashMap::new();
    // Threshold for removing old entries (100ms should be more than enough for loopback)
    const CLEANUP_THRESHOLD: Duration = Duration::from_millis(100);

    loop {
        // The socket has a read timeout so this loop can observe `shutdown`: without it a blocking
        // read on an idle interface would park forever, hanging the connection teardown's join().
        let frame = match socket.read_frame() {
            Ok(frame) => frame,
            Err(e) if is_read_timeout(&e) => {
                if shutdown.load(Ordering::Relaxed) {
                    return Ok(());
                }
                continue;
            }
            Err(e) => return Err(e).context("Failed to read frame from CAN"),
        };

        let wire = can_to_wire(&frame);
        let hash = frame_hash(wire.can_id, &wire.data);

        // Drain any pending hashes from the channel AFTER receiving a frame
        // This ensures we catch any hashes that were sent while we were blocked on read_frame
        let now = Instant::now();
        while let Ok(hash) = sent_frames_rx.try_recv() {
            sent_frames.insert(hash, now);
        }

        // Clean up old entries that are past the threshold
        // This prevents unbounded growth while avoiding race conditions
        let old_count = sent_frames.len();
        sent_frames.retain(|_, timestamp| now.duration_since(*timestamp) < CLEANUP_THRESHOLD);
        if old_count > sent_frames.len() {
            debug!(
                removed = old_count - sent_frames.len(),
                remaining = sent_frames.len(),
                "[CAN→TCP] Cleaned up old frame hashes"
            );
        }

        // Check if this is a frame we recently sent (looped back)
        if sent_frames.remove(&hash).is_some() {
            // This is a looped-back frame we sent, skip it
            debug!(interface = %iface, "[CAN→TCP] Skipping looped-back frame");
            continue;
        }

        debug!(interface = %iface, ?frame, "[CAN→TCP] Received frame from CAN");

        if let Err(e) = send_wire(&mut stream, &wire) {
            error!(error = %e, "[CAN→TCP] Failed to send frame over TCP");
            return Err(e);
        }
        debug!(interface = %iface, "[CAN→TCP] Sent frame over TCP");
    }
}

/// Whether a CAN read error is the benign timeout we set via [`CAN_READ_TIMEOUT`] (so the reader can
/// observe teardown) rather than a real read failure. Pure so it is unit-testable without a socket.
fn is_read_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

/// Whether a CAN write error is transient — the frame can be dropped and the loop kept alive,
/// rather than tearing the whole connection down. `ENOBUFS` means the TX qdisc was momentarily full
/// (common on the slow SPI MCP2515 under a burst of frames); `EAGAIN`/`EWOULDBLOCK` mean the write
/// would block. Any other errno is treated as fatal. Pure so it is unit-testable without a socket.
fn is_transient_write_err(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(code) if code == libc::ENOBUFS || code == libc::EAGAIN || code == libc::EWOULDBLOCK
    )
}

/// Run the TCP → CAN forwarding loop with deduplication tracking
///
/// This function receives frames from TCP and sends them to the CAN bus.
/// After successfully sending each frame, it notifies the reader thread via channel
/// so the reader can filter out the loopback when it appears.
fn tcp_to_can_loop(
    socket: CanFdSocket,
    mut stream: TcpStream,
    iface: &str,
    sent_frames_tx: Sender<u64>,
) -> Result<()> {
    info!(interface = %iface, "[TCP→CAN] Starting forwarding loop");

    loop {
        let wire = match recv_wire(&mut stream) {
            Ok(w) => w,
            Err(e) => {
                error!(error = %e, "[TCP→CAN] Failed to receive frame from TCP");
                return Err(e);
            }
        };

        debug!(
            interface = %iface,
            can_id = %format_args!("{:#x}", wire.can_id),
            data = ?wire.data,
            "[TCP→CAN] Received frame from TCP"
        );

        let frame = match wire_to_can(&wire) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "[TCP→CAN] Failed to convert wire frame to CAN");
                continue;
            }
        };

        // Notify the reader thread BEFORE sending to avoid race condition
        // where the loopback arrives before the hash is in the reader's set
        let hash = frame_hash(wire.can_id, &wire.data);
        // Ignore send errors - if the receiver is gone, we're shutting down anyway
        let _ = sent_frames_tx.send(hash);

        if let Err(e) = socket.write_frame(&frame) {
            // A transient TX-queue-full (ENOBUFS) or would-block must NOT kill the reverse loop —
            // that used to strand the whole connection (the ack path stayed dead until can0 was
            // toggled). Drop this one frame and keep serving; the queue drains as the bus catches up.
            if is_transient_write_err(&e) {
                warn!(error = %e, "[TCP→CAN] Transient CAN write error; dropping frame");
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            error!(error = %e, "[TCP→CAN] Failed to write frame to CAN");
            // Note: We already sent the hash, but the reader will eventually
            // clean it up since no matching frame will arrive
            return Err(e.into());
        }
        debug!(interface = %iface, "[TCP→CAN] Wrote frame to CAN");
    }
}

/// Handle a single TCP connection for bidirectional bridging
fn handle_connection(stream: TcpStream, iface: &str) -> Result<()> {
    info!(interface = %iface, "Connection established, starting bridge");

    // Open two CAN sockets (one for each direction)
    let can_read = CanFdSocket::open(iface).context("Failed to open CAN socket for reading")?;
    let can_write = CanFdSocket::open(iface).context("Failed to open CAN socket for writing")?;

    // Bound the CAN read so the reader thread can observe teardown even on an idle interface.
    if let Err(e) = can_read.set_read_timeout(CAN_READ_TIMEOUT) {
        warn!(error = %e, "Could not set CAN read timeout");
    }

    // Create a channel for the writer to notify the reader about sent frames
    // This allows the reader to filter out looped-back frames
    let (sent_frames_tx, sent_frames_rx) = mpsc::channel();

    // Clone TCP stream for bidirectional communication; keep `stream` itself as a shutdown handle so
    // that when one direction's loop exits we can tear the connection down and unblock the other.
    let stream_read = stream.try_clone().context("Failed to clone TCP stream")?;
    let stream_write = stream.try_clone().context("Failed to clone TCP stream for writer")?;

    let iface_clone = iface.to_string();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_reader = Arc::clone(&shutdown);

    // Spawn CAN → TCP thread (reads from CAN, writes to TCP)
    let can_to_tcp_handle = thread::spawn(move || {
        if let Err(e) =
            can_to_tcp_loop(can_read, stream_write, &iface_clone, sent_frames_rx, shutdown_reader)
        {
            error!(error = %e, "[CAN→TCP] Thread exited with error");
        }
    });

    // Run TCP → CAN in main thread (reads from TCP, writes to CAN)
    let iface_clone = iface.to_string();
    if let Err(e) = tcp_to_can_loop(can_write, stream_read, &iface_clone, sent_frames_tx) {
        error!(error = %e, "[TCP→CAN] Loop exited with error");
    }

    // The reverse loop has exited, so this connection is finished. Signal the reader to stop and shut
    // the socket down so the CAN→TCP thread's next TCP write fails and it returns; the shutdown flag
    // covers the case where that thread is instead parked on an idle CAN read (its read timeout lets
    // it wake and observe the flag), so join() can never hang while the reverse path is dead.
    shutdown.store(true, Ordering::Relaxed);
    let _ = stream.shutdown(Shutdown::Both);

    // Wait for the other thread
    let _ = can_to_tcp_handle.join();

    info!(interface = %iface, "Connection closed");
    Ok(())
}

/// Handle a connection with pre-opened CAN read socket (for server mode)
fn handle_connection_with_can_socket(
    stream: TcpStream,
    can_read: CanFdSocket,
    iface: &str,
) -> Result<()> {
    info!(interface = %iface, "Connection established, starting bridge");

    // Open write socket for TCP→CAN direction
    let can_write = CanFdSocket::open(iface).context("Failed to open CAN socket for writing")?;

    // Bound the CAN read so the reader thread can observe teardown even on an idle interface.
    if let Err(e) = can_read.set_read_timeout(CAN_READ_TIMEOUT) {
        warn!(error = %e, "Could not set CAN read timeout");
    }

    // Create a channel for the writer to notify the reader about sent frames
    let (sent_frames_tx, sent_frames_rx) = mpsc::channel();

    // Clone TCP stream for bidirectional communication; keep `stream` itself as a shutdown handle so
    // that when one direction's loop exits we can tear the connection down and unblock the other.
    let stream_read = stream.try_clone().context("Failed to clone TCP stream")?;
    let stream_write = stream.try_clone().context("Failed to clone TCP stream for writer")?;

    let iface_clone = iface.to_string();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_reader = Arc::clone(&shutdown);

    // Spawn CAN → TCP thread
    let can_to_tcp_handle = thread::spawn(move || {
        if let Err(e) =
            can_to_tcp_loop(can_read, stream_write, &iface_clone, sent_frames_rx, shutdown_reader)
        {
            error!(error = %e, "[CAN→TCP] Thread exited with error");
        }
    });

    // Run TCP → CAN in main thread
    let iface_clone = iface.to_string();
    if let Err(e) = tcp_to_can_loop(can_write, stream_read, &iface_clone, sent_frames_tx) {
        error!(error = %e, "[TCP→CAN] Loop exited with error");
    }

    // The reverse loop has exited, so this connection is finished. Signal the reader to stop and shut
    // the socket down so the CAN→TCP thread's next TCP write fails and it returns; the shutdown flag
    // covers the case where that thread is instead parked on an idle CAN read (its read timeout lets
    // it wake and observe the flag), so the server loops back to accept() instead of hanging join().
    shutdown.store(true, Ordering::Relaxed);
    let _ = stream.shutdown(Shutdown::Both);

    // Wait for the other thread
    let _ = can_to_tcp_handle.join();

    info!(interface = %iface, "Connection closed");
    Ok(())
}

/// Run in server (listen) mode
fn run_server(addr: &str, iface: &str) -> Result<()> {
    let listener = TcpListener::bind(addr).context("Failed to bind to address")?;
    info!(address = %addr, interface = %iface, "Server listening");

    loop {
        // Open CAN read socket BEFORE accepting connection so frames are buffered
        let can_read =
            CanFdSocket::open(iface).context("Failed to open CAN socket for reading")?;

        debug!("CAN socket ready, waiting for TCP connection");

        match listener.accept() {
            Ok((stream, peer_addr)) => {
                info!(peer = %peer_addr, "Accepted connection");
                if let Err(e) = handle_connection_with_can_socket(stream, can_read, iface) {
                    error!(error = %e, "Connection error");
                }
                debug!("Waiting for next connection");
            }
            Err(e) => {
                error!(error = %e, "Failed to accept connection");
            }
        }
    }
}

/// Run in client (connect) mode
fn run_client(addr: &str, iface: &str) -> Result<()> {
    loop {
        info!(address = %addr, interface = %iface, "Connecting to server");

        match TcpStream::connect(addr) {
            Ok(stream) => {
                if let Err(e) = handle_connection(stream, iface) {
                    error!(error = %e, "Connection error");
                }
            }
            Err(e) => {
                error!(error = %e, "Failed to connect to server");
            }
        }

        // Wait before reconnecting
        info!("Reconnecting in 1 second");
        thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn main() -> Result<()> {
    // Initialize tracing with environment filter
    // Set RUST_LOG=debug for debug output, RUST_LOG=info for normal output
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
            .add_directive(tracing::Level::INFO.into()))
        .init();

    let args = Args::parse();

    // Recover from an interface flap: a raw CAN socket held across a down/up goes stale, so watch the
    // link and exit (systemd restarts us) when it returns, reopening a fresh socket.
    spawn_link_monitor(&args.iface);

    match args.mode {
        Mode::Listen => run_server(&args.addr, &args.iface),
        Mode::Connect => run_client(&args.addr, &args.iface),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_read_timeout, is_transient_write_err, link_is_up, process_link_datagram};
    use netlink_packet_core::NetlinkMessage;
    use netlink_packet_route::link::{LinkAttribute, LinkFlags, LinkMessage, State};
    use netlink_packet_route::RouteNetlinkMessage;
    use std::io::{Error, ErrorKind};

    /// Serialize a single RTM_NEWLINK datagram for `iface` reporting operstate `oper`.
    fn newlink(iface: &str, oper: State) -> Vec<u8> {
        let mut link = LinkMessage::default();
        link.attributes.push(LinkAttribute::IfName(iface.into()));
        link.attributes.push(LinkAttribute::OperState(oper));
        let mut msg = NetlinkMessage::from(RouteNetlinkMessage::NewLink(link));
        msg.finalize();
        let mut buf = vec![0u8; msg.buffer_len()];
        msg.serialize(&mut buf);
        buf
    }

    #[test]
    fn transient_write_errors_keep_the_loop_alive() {
        assert!(is_transient_write_err(&Error::from_raw_os_error(libc::ENOBUFS)));
        // EAGAIN == EWOULDBLOCK on Linux.
        assert!(is_transient_write_err(&Error::from_raw_os_error(libc::EAGAIN)));
        assert!(is_transient_write_err(&Error::from_raw_os_error(libc::EWOULDBLOCK)));
    }

    #[test]
    fn fatal_write_errors_are_not_transient() {
        assert!(!is_transient_write_err(&Error::from_raw_os_error(libc::ENETDOWN)));
        assert!(!is_transient_write_err(&Error::from_raw_os_error(libc::EPERM)));
        // No errno at all (not an OS error) is treated as fatal.
        assert!(!is_transient_write_err(&Error::other("no errno")));
    }

    #[test]
    fn read_timeout_lets_the_reader_check_for_teardown() {
        // SO_RCVTIMEO surfaces as WouldBlock on Linux; poll-based timeouts as TimedOut.
        assert!(is_read_timeout(&Error::from_raw_os_error(libc::EAGAIN)));
        assert!(is_read_timeout(&Error::from(ErrorKind::WouldBlock)));
        assert!(is_read_timeout(&Error::from(ErrorKind::TimedOut)));
        // A real read failure is not a timeout and must stay fatal.
        assert!(!is_read_timeout(&Error::from_raw_os_error(libc::ENETDOWN)));
    }

    #[test]
    fn link_is_up_unless_operstate_down() {
        // CAN reports Unknown when up.
        assert!(link_is_up(Some(State::Up), LinkFlags::empty()));
        assert!(link_is_up(Some(State::Unknown), LinkFlags::empty()));
        assert!(!link_is_up(Some(State::Down), LinkFlags::Up));
        assert!(!link_is_up(Some(State::LowerLayerDown), LinkFlags::empty()));
        assert!(!link_is_up(Some(State::NotPresent), LinkFlags::empty()));
        // No operstate attribute: fall back to the admin IFF_UP flag.
        assert!(link_is_up(None, LinkFlags::Up));
        assert!(!link_is_up(None, LinkFlags::empty()));
    }

    #[test]
    fn detects_transition_across_two_messages_in_one_datagram() {
        // Two concatenated NEWLINK messages (down then up) exercise the multi-message + NLMSG_ALIGN
        // walk: the down seeds Some(false), and the up in the SAME datagram is the down→up trigger.
        let mut buf = newlink("can0", State::Down);
        while buf.len() % 4 != 0 {
            buf.push(0); // pad to NLMSG_ALIGN before the next message, as the kernel does
        }
        buf.extend(newlink("can0", State::Up));

        let mut prev_up = None;
        assert!(process_link_datagram(&buf, "can0", &mut prev_up));
    }

    #[test]
    fn first_observation_seeds_without_triggering() {
        let buf = newlink("can0", State::Up);
        let mut prev_up = None;
        assert!(!process_link_datagram(&buf, "can0", &mut prev_up));
        assert_eq!(prev_up, Some(true));
    }

    #[test]
    fn foreign_interface_is_ignored() {
        let buf = newlink("eth0", State::Up);
        let mut prev_up = Some(false);
        assert!(!process_link_datagram(&buf, "can0", &mut prev_up));
        assert_eq!(prev_up, Some(false)); // untouched
    }
}
