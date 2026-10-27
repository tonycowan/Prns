use core::cell::{Cell, RefCell};
use core::ffi::c_void;
use core::ptr::NonNull;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::Message;
use objc2_core_bluetooth::CBL2CAPChannel;
use objc2_core_foundation::{
    CFOptionFlags, CFReadStream, CFStreamClientContext, CFStreamEventType, CFWriteStream,
};
use objc2_foundation::{NSInputStream, NSOutputStream};
use tokio::sync::{mpsc as tokio_mpsc, oneshot, Notify};

use prns_core::interfaces::bluetooth_auto::{BLE_HW_MTU, STREAM_FRAME_PREFIX_LEN};

use super::{core_bluetooth_peer_id, CoreBluetoothPeerId};

pub(super) const L2CAP_SDU_LEN: usize = STREAM_FRAME_PREFIX_LEN + BLE_HW_MTU;
const READ_CHUNK: usize = L2CAP_SDU_LEN;
const READ_EVENTS: CFOptionFlags = CFStreamEventType::HasBytesAvailable.0
    | CFStreamEventType::ErrorOccurred.0
    | CFStreamEventType::EndEncountered.0;
const WRITE_EVENTS: CFOptionFlags = CFStreamEventType::CanAcceptBytes.0
    | CFStreamEventType::ErrorOccurred.0
    | CFStreamEventType::EndEncountered.0;
/// Eight full frames. A larger burst sits here until the stream takes it; the sender waits
/// rather than abandoning the channel.
const OUTBOUND_CAP: usize = 8 * L2CAP_SDU_LEN;
/// How often a sender retries a write the stream did not ask for again. One connection
/// interval is enough to notice space that `CanAcceptBytes` did not report.
#[cfg(test)]
const WRITE_RETRY: Duration = Duration::from_millis(25);

struct Outbound {
    pending: VecDeque<u8>,
    closed: bool,
    /// Bytes ever queued. A sender waits until `released` catches this frame's mark.
    accepted: u64,
    /// Bytes the stream has taken.
    released: u64,
    /// Set when a flush finds bytes waiting and the stream will not take them. Cleared on the
    /// next write that moves bytes, so the trace can report how long the stream stayed dry.
    dry_since: Option<Instant>,
}

pub(super) struct OutboundQueue {
    state: Mutex<Outbound>,
    progressed: Notify,
}

impl OutboundQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(Outbound {
                pending: VecDeque::new(),
                closed: false,
                accepted: 0,
                released: 0,
                dry_since: None,
            }),
            progressed: Notify::new(),
        }
    }

    /// Queue `bytes` once there is room. The returned mark is the point `released` must reach
    /// before this frame is on the stream.
    pub(super) async fn enqueue(&self, bytes: &[u8]) -> Result<u64, super::MacosBleError> {
        loop {
            let progressed = self.progressed.notified();
            {
                let mut out = self
                    .state
                    .lock()
                    .map_err(|_| super::MacosBleError::Closed)?;
                if out.closed {
                    return Err(super::MacosBleError::Closed);
                }
                if out.pending.len().saturating_add(bytes.len()) <= OUTBOUND_CAP {
                    out.pending.extend(bytes.iter().copied());
                    out.accepted = out.accepted.saturating_add(bytes.len() as u64);
                    return Ok(out.accepted);
                }
            }
            progressed.await;
        }
    }

    /// `Ok(true)` once the stream has taken every byte up through `mark`. `Ok(false)` means try
    /// the write again. A closed stream fails the frame.
    #[cfg(test)]
    pub(super) async fn accepted(&self, mark: u64) -> Result<bool, super::MacosBleError> {
        let progressed = self.progressed.notified();
        {
            let out = self
                .state
                .lock()
                .map_err(|_| super::MacosBleError::Closed)?;
            if out.released >= mark {
                return Ok(true);
            }
            if out.closed {
                return Err(super::MacosBleError::Closed);
            }
        }
        tokio::select! {
            _ = progressed => Ok(false),
            _ = tokio::time::sleep(WRITE_RETRY) => Ok(false),
        }
    }

    fn notify(&self) {
        self.progressed.notify_one();
    }
}

/// Append one line beside the journal delivery log. Identical lines inside `coalesce` are dropped
/// so a dry stream does not rewrite the same refusal on every retry. File IO stays off the
/// outbound mutex.
pub(super) fn note_l2cap(coalesce: Duration, line: &str) {
    use std::io::Write;

    struct Gate {
        last_line: String,
        last_at: Option<Instant>,
    }
    static GATE: Mutex<Gate> = Mutex::new(Gate {
        last_line: String::new(),
        last_at: None,
    });
    let Ok(mut gate) = GATE.lock() else {
        return;
    };
    let now = Instant::now();
    if !coalesce.is_zero()
        && gate.last_line == line
        && gate
            .last_at
            .is_some_and(|at| now.saturating_duration_since(at) < coalesce)
    {
        return;
    }
    gate.last_line.clear();
    gate.last_line.push_str(line);
    gate.last_at = Some(now);

    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let path = std::path::PathBuf::from(home).join("Library/Application Support/Journal/l2cap.log");
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(file, "{} {line}", local_stamp());
}

fn local_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as libc::time_t;
    let millis = now.subsec_millis();
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    // SAFETY: `localtime_r` writes only into `tm` and does not retain `secs`.
    let ok = unsafe { libc::localtime_r(&secs, &mut tm) };
    if ok.is_null() {
        return format!("?.{millis:03}");
    }
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        tm.tm_hour, tm.tm_min, tm.tm_sec, millis
    )
}

pub(super) struct StreamPump {
    input: Retained<NSInputStream>,
    output: Retained<NSOutputStream>,
    inbound_tx: RefCell<Option<tokio_mpsc::Sender<Box<[u8]>>>>,
    outbound: Arc<OutboundQueue>,
    last_read: RefCell<Option<Instant>>,
    read_burst: Cell<usize>,
    read_count: Cell<u32>,
    _channel: Retained<CBL2CAPChannel>,
}

/// Queue-confined: close both halves before releasing the stream receiver. The receiver closure is
/// the data-plane terminal signal consumed by the owning link lifecycle.
fn mark_data_plane_dead(pump: &StreamPump) {
    if let Ok(mut out) = pump.outbound.state.lock() {
        out.closed = true;
    }
    pump.outbound.notify();
    pump.inbound_tx.borrow_mut().take();
}

/// Queue-confined: the caller already owns `pump.outbound`, so acquiring it again would deadlock.
fn release_data_plane_receiver(pump: &StreamPump) {
    pump.inbound_tx.borrow_mut().take();
}

#[derive(Clone, Copy)]
pub(super) struct PumpPtr(pub(super) *const StreamPump);
// SAFETY: PumpPtr is only dereferenced by jobs on the owning serial dispatch queue, and PumpHandle
// schedules destruction on that same queue after all cloned handles have been dropped.
unsafe impl Send for PumpPtr {}

struct SendStreamPtr(*mut StreamPump);
// SAFETY: ownership of this pointer is transferred exactly once to the owning serial dispatch
// queue, where it is reconstructed into the original Box and freed.
unsafe impl Send for SendStreamPtr {}

pub(super) struct PumpHandle {
    ptr: *mut StreamPump,
    queue: DispatchRetained<DispatchQueue>,
}
// SAFETY: the handle never directly accesses the pointee; Drop transfers the final raw pointer to
// the owning serial dispatch queue, so moving the handle between threads cannot race the pump.
unsafe impl Send for PumpHandle {}
// SAFETY: shared references only clone/hold the dispatch queue and pointer. All pointee access and
// final destruction are serialized on that queue.
unsafe impl Sync for PumpHandle {}

impl Drop for PumpHandle {
    fn drop(&mut self) {
        let raw = SendStreamPtr(self.ptr);
        self.queue.exec_async(move || {
            let raw = raw;
            // SAFETY: this is the unique pointer produced by Box::into_raw in `wire_l2cap`. The
            // queue owns all stream callbacks; removing both clients before closing the streams
            // guarantees no callback can access the pump after Box::from_raw frees it.
            unsafe {
                let pump = &*raw.0;
                let cf_in = &*(Retained::as_ptr(&pump.input) as *const CFReadStream);
                let cf_out = &*(Retained::as_ptr(&pump.output) as *const CFWriteStream);
                cf_in.set_client(0, None, core::ptr::null_mut());
                cf_out.set_client(0, None, core::ptr::null_mut());
                pump.input.close();
                pump.output.close();
                drop(Box::from_raw(raw.0));
            }
        });
    }
}

pub(super) fn flush(pump: &StreamPump, cause: &'static str) {
    let mut wake = false;
    let mut note: Option<(Duration, String)> = None;
    {
        let Ok(mut out) = pump.outbound.state.lock() else {
            return;
        };
        if out.closed {
            return;
        }
        let pending = out.pending.len();
        // A space event means the stream asked for bytes. `hasSpaceAvailable` goes false after
        // one write even when the buffer will take another frame, so that event keeps writing
        // one frame at a time until a write comes back empty. A retry only writes while the
        // stream already says it has room, and never guesses.
        let drain = cause == "space";
        let mut saw_space = false;
        let mut frames = 0u32;
        let mut writing = !out.pending.is_empty() && (drain || pump.output.hasSpaceAvailable());
        while writing {
            saw_space = true;
            // One length-prefixed frame per write. A single write of a whole window is what the
            // stream takes one part of and then holds.
            let len = {
                let (front, _) = out.pending.as_slices();
                let frame = if front.len() >= STREAM_FRAME_PREFIX_LEN {
                    let body = u16::from_be_bytes([front[0], front[1]]) as usize;
                    STREAM_FRAME_PREFIX_LEN.saturating_add(body)
                } else {
                    front.len()
                };
                frame.min(front.len())
            };
            let ptr = out.pending.as_slices().0.as_ptr();
            // SAFETY: `ptr` addresses `len` bytes at the front of `out.pending`. The mutex guard
            // is held across the write, so the deque cannot reallocate underneath it.
            let written = unsafe {
                pump.output
                    .write_maxLength(NonNull::new_unchecked(ptr as *mut u8), len)
            };
            if written > 0 {
                let asked = len;
                out.pending.drain(..written as usize);
                out.released = out.released.saturating_add(written as u64);
                let dry = out.dry_since.take().map(|since| since.elapsed());
                if written as usize != asked {
                    note = Some((
                        Duration::ZERO,
                        format!(
                            "l2cap short asked={asked} wrote={written} pending={} cause={cause}",
                            out.pending.len()
                        ),
                    ));
                } else {
                    frames = frames.saturating_add(1);
                    if note.is_none() {
                        if let Some(dry) = dry.filter(|dry| *dry >= Duration::from_millis(50)) {
                            note = Some((
                                Duration::ZERO,
                                format!(
                                    "l2cap wrote {written} after {}ms dry pending={} cause={cause}",
                                    dry.as_millis(),
                                    out.pending.len()
                                ),
                            ));
                        }
                    }
                }
                wake = true;
                writing = !out.pending.is_empty() && (drain || pump.output.hasSpaceAvailable());
            } else if written < 0 {
                crate::diagnostic_log::warn!(
                    "bluetooth: L2CAP write returned {written} — data plane down"
                );
                note = Some((
                    Duration::ZERO,
                    format!("l2cap write-error {written} pending={pending} cause={cause}"),
                ));
                out.closed = true;
                drop(out);
                release_data_plane_receiver(pump);
                pump.outbound.notify();
                if let Some((coalesce, line)) = note {
                    note_l2cap(coalesce, &line);
                }
                return;
            } else {
                if out.dry_since.is_none() {
                    out.dry_since = Some(Instant::now());
                }
                note = Some((
                    Duration::from_millis(100),
                    format!(
                        "l2cap write-zero pending={} cause={cause}",
                        out.pending.len()
                    ),
                ));
                break;
            }
        }
        if frames > 1 && note.is_none() {
            note = Some((
                Duration::ZERO,
                format!(
                    "l2cap burst frames={frames} pending={} cause={cause}",
                    out.pending.len()
                ),
            ));
        }
        if pending > 0 && !saw_space {
            if out.dry_since.is_none() {
                out.dry_since = Some(Instant::now());
            }
            note = Some((
                Duration::from_millis(100),
                format!("l2cap no-space pending={pending} cause={cause}"),
            ));
        }
    }
    if wake {
        pump.outbound.notify();
    }
    if let Some((coalesce, line)) = note {
        note_l2cap(coalesce, &line);
    }
}

unsafe extern "C-unwind" fn read_cb(
    _stream: *mut CFReadStream,
    event: CFStreamEventType,
    info: *mut c_void,
) {
    // SAFETY: `wire_l2cap` registers `info` as its Box-backed StreamPump and unregisters the client
    // callbacks on the same serial queue before freeing the allocation.
    let pump = unsafe { &*(info as *const StreamPump) };
    if (event.0 & CFStreamEventType::HasBytesAvailable.0) != 0 {
        let mut buf = [0u8; READ_CHUNK];
        while pump.input.hasBytesAvailable() {
            // SAFETY: `buf.as_mut_ptr()` is non-null and valid for READ_CHUNK writable bytes for the
            // duration of the synchronous NSInputStream read.
            let read = unsafe {
                pump.input
                    .read_maxLength(NonNull::new_unchecked(buf.as_mut_ptr()), READ_CHUNK)
            };
            if read > 0 {
                let now = Instant::now();
                if let Some(prev) = *pump.last_read.borrow() {
                    let gap = now.saturating_duration_since(prev);
                    if gap >= Duration::from_millis(200) {
                        note_l2cap(
                            Duration::ZERO,
                            &format!(
                                "l2cap read-gap {}ms burst={}B reads={} then {read}B",
                                gap.as_millis(),
                                pump.read_burst.get(),
                                pump.read_count.get()
                            ),
                        );
                        pump.read_burst.set(0);
                        pump.read_count.set(0);
                    }
                }
                *pump.last_read.borrow_mut() = Some(now);
                pump.read_burst.set(pump.read_burst.get() + read as usize);
                pump.read_count.set(pump.read_count.get() + 1);
                let accepted = pump
                    .inbound_tx
                    .borrow()
                    .as_ref()
                    .is_some_and(|tx| tx.try_send(Box::from(&buf[..read as usize])).is_ok());
                if !accepted {
                    note_l2cap(
                        Duration::ZERO,
                        &format!("l2cap read-dropped {read}B inbound queue closed"),
                    );
                    mark_data_plane_dead(pump);
                    break;
                }
            } else {
                break;
            }
        }
    }
    if (event.0 & (CFStreamEventType::ErrorOccurred.0 | CFStreamEventType::EndEncountered.0)) != 0 {
        crate::diagnostic_log::warn!(
            "bluetooth: L2CAP read stream closed/errored — inbound data plane down"
        );
        mark_data_plane_dead(pump);
    }
}

unsafe extern "C-unwind" fn write_cb(
    _stream: *mut CFWriteStream,
    event: CFStreamEventType,
    info: *mut c_void,
) {
    // SAFETY: `wire_l2cap` registers `info` as its Box-backed StreamPump and unregisters the client
    // callbacks on the same serial queue before freeing the allocation.
    let pump = unsafe { &*(info as *const StreamPump) };
    if (event.0 & CFStreamEventType::CanAcceptBytes.0) != 0 {
        flush(pump, "space");
    }
    if (event.0 & (CFStreamEventType::ErrorOccurred.0 | CFStreamEventType::EndEncountered.0)) != 0 {
        crate::diagnostic_log::warn!(
            "bluetooth: L2CAP write stream closed/errored — outbound data plane down"
        );
        mark_data_plane_dead(pump);
    }
}

pub(super) fn wire_l2cap(
    channel: &CBL2CAPChannel,
    queue: &DispatchRetained<DispatchQueue>,
) -> Option<DataPlane> {
    // SAFETY: CoreBluetooth supplied a live retained channel, and its accessors return retained
    // objects whose runtime types match the generated bindings.
    let (input, output) = unsafe { (channel.inputStream()?, channel.outputStream()?) };
    let (inbound_tx, inbound_rx) = tokio_mpsc::channel::<Box<[u8]>>(16);
    let outbound = Arc::new(OutboundQueue::new());
    let pump = Box::into_raw(Box::new(StreamPump {
        input,
        output,
        inbound_tx: RefCell::new(Some(inbound_tx)),
        outbound: outbound.clone(),
        last_read: RefCell::new(None),
        read_burst: Cell::new(0),
        read_count: Cell::new(0),
        _channel: channel.retain(),
    }));
    // SAFETY: `pump` is a live Box allocation kept by PumpHandle. NSInputStream/NSOutputStream are
    // toll-free bridged to the corresponding CFStream types. The client context pointer stays live
    // until PumpHandle unregisters both callbacks on this same serial queue before freeing it.
    unsafe {
        let pump_ref = &*pump;
        let cf_in = &*(Retained::as_ptr(&pump_ref.input) as *const CFReadStream);
        let cf_out = &*(Retained::as_ptr(&pump_ref.output) as *const CFWriteStream);
        let mut ctx = CFStreamClientContext {
            version: 0,
            info: pump as *mut c_void,
            retain: None,
            release: None,
            copyDescription: None,
        };
        cf_in.set_client(READ_EVENTS, Some(read_cb), &mut ctx);
        cf_in.set_dispatch_queue(Some(&**queue));
        cf_out.set_client(WRITE_EVENTS, Some(write_cb), &mut ctx);
        cf_out.set_dispatch_queue(Some(&**queue));
        pump_ref.input.open();
        pump_ref.output.open();
    }
    note_l2cap(Duration::ZERO, "l2cap channel open");
    Some(DataPlane {
        inbound_rx,
        outbound,
        queue: queue.clone(),
        pump_ptr: PumpPtr(pump),
        pump: Arc::new(PumpHandle {
            ptr: pump,
            queue: queue.clone(),
        }),
    })
}

/// Read the exact CoreBluetooth peer identity without touching or opening the channel streams.
pub(super) fn l2cap_peer_id(channel: &CBL2CAPChannel) -> Option<CoreBluetoothPeerId> {
    // SAFETY: CoreBluetooth supplied a live channel and retains its peer for the callback.
    unsafe { channel.peer().map(|peer| core_bluetooth_peer_id(&peer)) }
}

/// Explicitly close a refused channel. CoreBluetooth has no delegate acknowledgement for an
/// inbound CoC, so closing both unopened stream halves is the only immediate negative signal.
pub(super) fn close_l2cap(channel: &CBL2CAPChannel) {
    // SAFETY: CoreBluetooth supplied a live channel. Closing an unopened or already closed
    // NSStream is idempotent and does not schedule either stream on a run loop or dispatch queue.
    unsafe {
        if let Some(input) = channel.inputStream() {
            input.close();
        }
        if let Some(output) = channel.outputStream() {
            output.close();
        }
    }
}

pub(super) struct DataPlane {
    pub(super) inbound_rx: tokio_mpsc::Receiver<Box<[u8]>>,
    pub(super) outbound: Arc<OutboundQueue>,
    pub(super) queue: DispatchRetained<DispatchQueue>,
    pub(super) pump_ptr: PumpPtr,
    pub(super) pump: Arc<PumpHandle>,
}

impl DataPlane {
    fn is_closed(&self) -> bool {
        self.inbound_rx.is_closed()
            || self
                .outbound
                .state
                .lock()
                .map_or(true, |outbound| outbound.closed)
    }
}

const MAX_BUFFERED_L2CAP: usize = 4;

#[derive(Default)]
pub(super) struct PendingL2cap {
    waiters: VecDeque<oneshot::Sender<DataPlane>>,
    ready: VecDeque<DataPlane>,
}

impl PendingL2cap {
    pub(super) fn deliver(&mut self, mut data: DataPlane) -> bool {
        self.reap_closed();
        while let Some(tx) = self.waiters.pop_front() {
            match tx.send(data) {
                Ok(()) => return true,
                Err(returned) => data = returned,
            }
        }
        if self.ready.len() >= MAX_BUFFERED_L2CAP {
            return false;
        }
        self.ready.push_back(data);
        true
    }

    pub(super) fn arm(&mut self, tx: oneshot::Sender<DataPlane>) -> bool {
        self.reap_closed();
        if tx.is_closed() {
            return false;
        }
        match self.ready.pop_front() {
            Some(data) => {
                let _ = tx.send(data);
                true
            }
            None => {
                self.waiters.retain(|waiter| !waiter.is_closed());
                if self.waiters.len() >= MAX_BUFFERED_L2CAP {
                    return false;
                }
                self.waiters.push_back(tx);
                true
            }
        }
    }

    pub(super) fn can_deliver(&self) -> bool {
        self.waiters.iter().any(|waiter| !waiter.is_closed())
            || self.ready.len() < MAX_BUFFERED_L2CAP
    }

    pub(super) fn reap_closed(&mut self) {
        self.waiters.retain(|waiter| !waiter.is_closed());
        self.ready.retain(|data| !data.is_closed());
    }

    pub(super) fn is_empty(&self) -> bool {
        self.waiters.is_empty() && self.ready.is_empty()
    }

    #[cfg(test)]
    pub(super) fn waiter_len(&self) -> usize {
        self.waiters.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::OutboundQueue;

    fn release(queue: &OutboundQueue, n: usize) {
        let mut out = queue.state.lock().expect("outbound");
        let n = n.min(out.pending.len());
        out.pending.drain(..n);
        out.released = out.released.saturating_add(n as u64);
        drop(out);
        queue.notify();
    }

    #[tokio::test]
    async fn a_frame_is_held_until_the_stream_accepts_it() {
        let queue = Arc::new(OutboundQueue::new());
        let mark = queue.enqueue(&[1, 2, 3, 4]).await.expect("room");
        let waiting = {
            let queue = Arc::clone(&queue);
            tokio::spawn(async move {
                loop {
                    if queue.accepted(mark).await.expect("open") {
                        return;
                    }
                }
            })
        };
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(!waiting.is_finished());
        release(&queue, 4);
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the stream accepted the frame")
            .expect("task");
    }

    #[tokio::test]
    async fn a_full_queue_waits_for_room_instead_of_refusing_the_frame() {
        let queue = Arc::new(OutboundQueue::new());
        let frame = vec![0u8; super::L2CAP_SDU_LEN];
        for _ in 0..8 {
            queue.enqueue(&frame).await.expect("the first eight fit");
        }
        let queued = {
            let queue = Arc::clone(&queue);
            let frame = frame.clone();
            tokio::spawn(async move { queue.enqueue(&frame).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!queued.is_finished());
        release(&queue, super::L2CAP_SDU_LEN);
        tokio::time::timeout(Duration::from_secs(1), queued)
            .await
            .expect("room came free")
            .expect("task")
            .expect("enqueued");
    }

    #[tokio::test]
    async fn closing_the_queue_fails_a_frame_still_waiting_on_the_stream() {
        let queue = Arc::new(OutboundQueue::new());
        let mark = queue.enqueue(&[9]).await.expect("room");
        let waiting = {
            let queue = Arc::clone(&queue);
            tokio::spawn(async move {
                loop {
                    match queue.accepted(mark).await {
                        Ok(true) => return Ok(()),
                        Ok(false) => {}
                        Err(err) => return Err(err),
                    }
                }
            })
        };
        {
            let mut out = queue.state.lock().expect("outbound");
            out.closed = true;
        }
        queue.notify();
        let result = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the wait ended")
            .expect("task");
        assert!(result.is_err());
    }
}
