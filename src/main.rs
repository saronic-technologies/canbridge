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

/// Dedup notifications from the TCP→CAN writer to the CAN→TCP reader. The writer registers each frame
/// it puts on the bus so the reader can drop the kernel loopback; if a write is dropped (transient
/// error) it retracts the hash, so a legitimate incoming frame with the same ID+data isn't filtered.
enum DedupMsg {
    /// A frame was written to CAN — expect and filter its loopback.
    Sent(u64),
    /// A frame was dropped before reaching the bus — no loopback will come, so forget the hash.
    Dropped(u64),
}

/// Pending loopbacks for a given hash: a count (identical frames can be in flight at once) plus the
/// most recent timestamp, used only by the eviction safety net.
struct Pending {
    count: u32,
    last: Instant,
}

/// Tracks hashes of frames the writer put on the bus so the reader can drop their kernel loopback.
/// A `Sent` adds one pending loopback for a hash; a `Dropped` retracts one (its write never reached
/// the bus); [`is_loopback`] consumes one pending loopback; [`evict_older_than`] is a safety net
/// against unbounded growth. The per-hash count is what lets a dropped frame retract only its own
/// instance instead of cancelling a concurrent successful send's still-pending loopback.
#[derive(Default)]
struct LoopbackFilter {
    sent: HashMap<u64, Pending>,
}

impl LoopbackFilter {
    fn apply(&mut self, msg: DedupMsg, now: Instant) {
        match msg {
            DedupMsg::Sent(h) => {
                let e = self.sent.entry(h).or_insert(Pending { count: 0, last: now });
                e.count += 1;
                e.last = now;
            }
            // The writer dropped this frame, so one fewer loopback will arrive — retract a single
            // pending instance (not the whole hash), so a concurrent successful send of the same
            // ID+data keeps its pending loopback and a real incoming frame isn't wrongly filtered.
            DedupMsg::Dropped(h) => self.consume(h),
        }
    }

    /// Drop one pending loopback for `hash`, removing the entry when it reaches zero.
    fn consume(&mut self, hash: u64) {
        if let Some(e) = self.sent.get_mut(&hash) {
            e.count -= 1;
            if e.count == 0 {
                self.sent.remove(&hash);
            }
        }
    }

    /// Drop entries older than `threshold`; returns how many were evicted.
    fn evict_older_than(&mut self, now: Instant, threshold: Duration) -> usize {
        let before = self.sent.len();
        self.sent
            .retain(|_, p| now.duration_since(p.last) < threshold);
        before - self.sent.len()
    }

    /// True if `hash` matches a pending send (consuming one) — i.e. this is our own loopback.
    fn is_loopback(&mut self, hash: u64) -> bool {
        if self.sent.contains_key(&hash) {
            self.consume(hash);
            true
        } else {
            false
        }
    }

    fn len(&self) -> usize {
        self.sent.len()
    }
}

/// Run the CAN → TCP forwarding loop with deduplication
///
/// This function reads frames from the CAN bus and forwards them over TCP.
/// It maintains a local map of recently sent frame hashes (received via channel)
/// to filter out frames that we transmitted ourselves (which appear due to kernel loopback).
fn can_to_tcp_loop(
    socket: CanFdSocket,
    mut stream: TcpStream,
    iface: &str,
    sent_frames_rx: Receiver<DedupMsg>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    info!(interface = %iface, "[CAN→TCP] Starting forwarding loop");

    // Tracks frames we've recently written so their kernel loopback can be filtered.
    let mut filter = LoopbackFilter::default();
    // Threshold for removing old entries (100ms should be more than enough for loopback)
    const CLEANUP_THRESHOLD: Duration = Duration::from_millis(100);

    loop {
        // The socket has a read timeout so this loop can observe `shutdown`: without it a blocking
        // read on an idle interface would park forever, hanging the connection teardown's join().
        let maybe_frame = match socket.read_frame() {
            Ok(frame) => Some(frame),
            Err(e) if is_read_timeout(&e) => {
                if shutdown.load(Ordering::Relaxed) {
                    return Ok(());
                }
                None
            }
            Err(e) => return Err(e).context("Failed to read frame from CAN"),
        };

        // Drain pending dedup notifications and evict stale entries EVERY iteration — including idle
        // read timeouts — so neither the unbounded channel nor the filter map can grow without bound
        // while the writer drops frames (ENOBUFS) and no loopback traffic wakes the reader.
        let now = Instant::now();
        while let Ok(msg) = sent_frames_rx.try_recv() {
            filter.apply(msg, now);
        }
        let removed = filter.evict_older_than(now, CLEANUP_THRESHOLD);
        if removed > 0 {
            debug!(
                removed,
                remaining = filter.len(),
                "[CAN→TCP] Cleaned up old frame hashes"
            );
        }

        let frame = match maybe_frame {
            Some(frame) => frame,
            None => continue,
        };

        let wire = can_to_wire(&frame);
        let hash = frame_hash(wire.can_id, &wire.data);

        // Check if this is a frame we recently sent (looped back)
        if filter.is_loopback(hash) {
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
/// This function receives frames from TCP and sends them to the CAN bus. It notifies the reader
/// thread of each frame's hash *before* writing (so the reader has it registered before the kernel
/// loopback can arrive), then retracts the hash if the write is dropped so the reader doesn't filter
/// a legitimate incoming frame with the same ID+data.
fn tcp_to_can_loop(
    socket: CanFdSocket,
    mut stream: TcpStream,
    iface: &str,
    sent_frames_tx: Sender<DedupMsg>,
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

        // Register the hash with the reader BEFORE writing, to avoid the race where the kernel
        // loopback reaches the reader before the hash is in its map.
        let hash = frame_hash(wire.can_id, &wire.data);
        // Ignore send errors - if the receiver is gone, we're shutting down anyway
        let _ = sent_frames_tx.send(DedupMsg::Sent(hash));

        if let Err(e) = socket.write_frame(&frame) {
            // A transient TX-queue-full (ENOBUFS) or would-block must NOT kill the reverse loop —
            // that used to strand the whole connection (the ack path stayed dead until can0 was
            // toggled). Drop this one frame and keep serving; the queue drains as the bus catches up.
            if is_transient_write_err(&e) {
                warn!(error = %e, "[TCP→CAN] Transient CAN write error; dropping frame");
                // The frame never hit the bus, so no loopback will come: retract the hash so it can't
                // falsely filter a real incoming frame with the same ID+data (within CLEANUP_THRESHOLD).
                let _ = sent_frames_tx.send(DedupMsg::Dropped(hash));
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            error!(error = %e, "[TCP→CAN] Failed to write frame to CAN");
            // Fatal error: we return and the connection is torn down, so the stale hash is moot
            // (the reader thread exits with the connection).
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
    if let Err(e) = can_read.set_read_timeout(Some(CAN_READ_TIMEOUT)) {
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
    if let Err(e) = can_read.set_read_timeout(Some(CAN_READ_TIMEOUT)) {
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
    use super::{
        is_read_timeout, is_transient_write_err, link_is_up, process_link_datagram, DedupMsg,
        LoopbackFilter,
    };
    use netlink_packet_core::NetlinkMessage;
    use netlink_packet_route::link::{LinkAttribute, LinkFlags, LinkMessage, State};
    use netlink_packet_route::RouteNetlinkMessage;
    use std::io::{Error, ErrorKind};
    use std::time::{Duration, Instant};

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
    fn dropped_retracts_a_pending_loopback_hash() {
        let now = Instant::now();
        let mut f = LoopbackFilter::default();
        f.apply(DedupMsg::Sent(42), now);
        // The write was dropped, so no loopback will come: the hash must be retracted, and a real
        // incoming frame with the same hash must NOT be filtered.
        f.apply(DedupMsg::Dropped(42), now);
        assert!(!f.is_loopback(42));
        assert_eq!(f.len(), 0);
    }

    #[test]
    fn sent_hash_filters_exactly_one_loopback() {
        let now = Instant::now();
        let mut f = LoopbackFilter::default();
        f.apply(DedupMsg::Sent(7), now);
        assert!(f.is_loopback(7)); // our loopback is filtered once...
        assert!(!f.is_loopback(7)); // ...and a later real frame with the same hash passes through
    }

    #[test]
    fn a_drop_retracts_only_its_own_instance_not_a_concurrent_send() {
        let now = Instant::now();
        let mut f = LoopbackFilter::default();
        // Two same-hash writes in flight: the first succeeded, the second was dropped (ENOBUFS).
        f.apply(DedupMsg::Sent(9), now);
        f.apply(DedupMsg::Sent(9), now);
        f.apply(DedupMsg::Dropped(9), now);
        // The dropped write retracts only one instance, so the successful write's loopback is still
        // filtered (not echoed back over TCP) — and only that one.
        assert!(f.is_loopback(9));
        assert!(!f.is_loopback(9));
    }

    #[test]
    fn stale_hashes_are_evicted_after_the_threshold() {
        let start = Instant::now();
        let mut f = LoopbackFilter::default();
        f.apply(DedupMsg::Sent(1), start);
        let later = start + Duration::from_millis(101);
        assert_eq!(f.evict_older_than(later, Duration::from_millis(100)), 1);
        assert!(!f.is_loopback(1));
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
