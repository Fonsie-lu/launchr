//! Screen capture through `wlr-screencopy-v1`.
//!
//! The launcher blurs its own backdrop instead of asking the compositor for a
//! blur rule, so it looks the same on river, sway, Hyprland or anything else
//! wlroots-based. Capturing happens before the window is mapped, so the
//! snapshot never contains the launcher itself.

use crate::blur::Orientation;
use rustix::event::{PollFd, PollFlags, Timespec};
use std::fs::File;
use std::ops::Deref;
use std::os::fd::{AsFd, OwnedFd};
use std::ptr::NonNull;
use std::time::{Duration, Instant};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// One captured output, still in the compositor's pixel format.
pub struct Frame {
    /// Connector name (`DP-1`, `HDMI-A-1`), used to pair the shot with the
    /// GDK monitor the launcher ends up on.
    pub connector: Option<String>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// Bit offsets of the top eight bits of red, green and blue in a pixel
    /// read as a little endian `u32` — which is how every `wl_shm` format is
    /// defined, so one rule covers 8 and 10 bits per channel alike.
    pub channels: [u32; 3],
    /// What turns the buffer into the picture on the output.
    pub orientation: Orientation,
    pub data: Pixels,
}

/// The captured pixels. The compositor writes them into a memfd we hand it, so
/// the shared mapping is read straight out of that — a full screen frame is
/// several megabytes and copying it out first is pure latency.
pub enum Pixels {
    Mapped(Mapping),
    Owned(Vec<u8>),
}

impl Deref for Pixels {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Pixels::Mapped(mapping) => mapping,
            Pixels::Owned(data) => data,
        }
    }
}

impl From<Vec<u8>> for Pixels {
    fn from(data: Vec<u8>) -> Self {
        Pixels::Owned(data)
    }
}

/// A read-only shared mapping of the capture buffer.
pub struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

impl Mapping {
    fn new(file: &File, len: usize) -> Option<Self> {
        if len == 0 {
            return None;
        }
        // SAFETY: a fresh mapping of a memfd this process owns; the pointer is
        // only ever read, through the slice built below, and unmapped on drop.
        let ptr = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                rustix::mm::ProtFlags::READ,
                rustix::mm::MapFlags::SHARED,
                file,
                0,
            )
        }
        .ok()?;
        Some(Mapping { ptr: NonNull::new(ptr.cast())?, len })
    }
}

impl Deref for Mapping {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr` maps `len` readable bytes for as long as `self` lives.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly the mapping made in `new`.
        unsafe {
            let _ = rustix::mm::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

/// Capture every output, giving up at `deadline`. Returns an empty vec when
/// the compositor does not implement screencopy, or does not answer in time —
/// the caller then falls back to dimming only.
///
/// `deadline` covers every compositor exchange after the registry is up. The
/// round trip inside `registry_queue_init` cannot be given a timeout, so it
/// sits outside; the caller's own wait is what bounds that one.
pub fn capture_outputs(deadline: Instant) -> Vec<Frame> {
    match try_capture(deadline) {
        Ok(frames) => frames,
        Err(error) => {
            eprintln!("launchr: screen capture unavailable ({error}), falling back to dim only");
            Vec::new()
        }
    }
}

fn try_capture(deadline: Instant) -> Result<Vec<Frame>, String> {
    let connection = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&connection).map_err(|e| e.to_string())?;
    let qh = queue.handle();

    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(|_| "no wl_shm")?;
    let manager: ZwlrScreencopyManagerV1 =
        globals.bind(&qh, 1..=3, ()).map_err(|_| "no wlr-screencopy-v1")?;

    let mut state = State { shm, manager_version: manager.version(), outputs: Vec::new() };
    let outputs: Vec<_> = globals.contents().clone_list();
    for global in outputs.iter().filter(|g| g.interface == "wl_output") {
        let index = state.outputs.len();
        let proxy: wl_output::WlOutput =
            globals.registry().bind(global.name, global.version.min(4), &qh, index);
        state.outputs.push(Output::new(proxy));
    }
    if state.outputs.is_empty() {
        return Err("no outputs".to_owned());
    }
    // Requested before anything is waited on: the copies need only the output
    // proxies, so putting the framebuffer readback in flight first lets the
    // output descriptions arrive alongside it rather than one round trip
    // ahead. They arrive before the frames do in any case: the compositor
    // sends them in answer to the bind, which it reads before these requests.
    for (index, output) in state.outputs.iter().enumerate() {
        manager.capture_output(0, &output.proxy, &qh, index);
    }

    // Bounded in wall clock, not in iterations: neither `blocking_dispatch` nor
    // `roundtrip` takes a timeout, so a compositor that goes quiet would
    // otherwise hang the launcher here forever.
    //
    // Connector names only matter when there is more than one shot to pair with
    // a monitor; with one output there is nothing to pair and the wait ends as
    // soon as the frame lands. `wl_output` only has a name from version 4 on,
    // so an older compositor's outputs are not waited on for one.
    let want_names = state.outputs.len() > 1;
    wait_until(&mut queue, &mut state, deadline, |state| {
        state.outputs.iter().all(|output| {
            output.slot.finished
                && (!want_names || output.connector.is_some() || output.proxy.version() < 4)
        })
    })?;

    let mut frames = Vec::new();
    for output in &state.outputs {
        let slot = &output.slot;
        // A shot that cannot be paired with its monitor is worse than none:
        // shown on the wrong one, it puts another output's desktop behind the
        // panel. Only a lone output needs no name.
        if !slot.ready || (want_names && output.connector.is_none()) {
            continue;
        }
        let (Some(file), Some(channels)) = (slot.file.as_ref(), slot.channels) else { continue };
        let size = slot.stride as usize * slot.height as usize;
        let Some(mapping) = Mapping::new(file, size) else { continue };
        frames.push(Frame {
            connector: output.connector.clone(),
            width: slot.width,
            height: slot.height,
            stride: slot.stride,
            channels,
            orientation: orientation(output.transform, slot.y_invert),
            data: Pixels::Mapped(mapping),
        });
    }
    Ok(frames)
}

/// Dispatch events until `done`, or until `deadline` passes. Giving up is not
/// an error: the caller keeps whatever did arrive.
fn wait_until(
    queue: &mut EventQueue<State>,
    state: &mut State,
    deadline: Instant,
    done: impl Fn(&State) -> bool,
) -> Result<(), String> {
    while !done(state) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !dispatch_before(queue, state, left)? {
            eprintln!("launchr: screen capture timed out waiting for the compositor");
            return Ok(());
        }
    }
    Ok(())
}

/// One dispatch pass that gives up after `timeout` instead of blocking for as
/// long as the compositor feels like. Returns `false` when the pass made no
/// progress, which the caller treats as "stop waiting".
fn dispatch_before(
    queue: &mut EventQueue<State>,
    state: &mut State,
    timeout: Duration,
) -> Result<bool, String> {
    if queue.dispatch_pending(state).map_err(|e| e.to_string())? > 0 {
        return Ok(true);
    }
    queue.flush().map_err(|e| e.to_string())?;
    // `None` means "dispatch before asking again" — but the dispatch above
    // already came up empty this pass, so one more attempt decides it. Saying
    // "keep going" unconditionally here would spin on the CPU for the whole
    // deadline without ever reaching the poll below.
    let Some(guard) = queue.prepare_read() else {
        return Ok(queue.dispatch_pending(state).map_err(|e| e.to_string())? > 0);
    };

    let timeout = Timespec::try_from(timeout).map_err(|e| e.to_string())?;
    // Scoped so the borrow of `guard` ends before `guard.read()` consumes it.
    let ready = {
        let fd = guard.connection_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::IN | PollFlags::ERR)];
        rustix::io::retry_on_intr(|| rustix::event::poll(&mut fds, Some(&timeout)))
            .map_err(|e| e.to_string())?
    };
    if ready == 0 {
        return Ok(false);
    }
    guard.read().map_err(|e| e.to_string())?;
    queue.dispatch_pending(state).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Where the top eight bits of red, green and blue sit in a pixel of
/// `format`, read as a little endian `u32` (see `Frame::channels`), or `None`
/// for a format the downscale cannot read.
///
/// The 10 bit formats are what an output driven at 10 bits per channel hands
/// over. Without them such an output was never captured at all.
fn channel_shifts(format: wl_shm::Format) -> Option<[u32; 3]> {
    use wl_shm::Format;
    Some(match format {
        // 0xAARRGGBB: blue in the low byte.
        Format::Xrgb8888 | Format::Argb8888 => [16, 8, 0],
        Format::Xbgr8888 | Format::Abgr8888 => [0, 8, 16],
        // 2 bits of alpha or padding above three 10 bit channels.
        Format::Xrgb2101010 | Format::Argb2101010 => [22, 12, 2],
        Format::Xbgr2101010 | Format::Abgr2101010 => [2, 12, 22],
        _ => return None,
    })
}

/// What it takes to turn a captured buffer into the picture on the output.
///
/// Screencopy hands over the buffer as it is scanned out, which for a rotated
/// or flipped output is not the picture its user sees; and a frame flagged
/// `y_invert` stores its rows bottom up on top of that. The table was
/// established against grim on an output in each of the eight transforms.
pub(crate) fn orientation(transform: wl_output::Transform, y_invert: bool) -> Orientation {
    use wl_output::Transform;
    let (flip_x, flip_y, transpose) = match transform {
        Transform::_90 => (false, true, true),
        Transform::_180 => (true, true, false),
        Transform::_270 => (true, false, true),
        Transform::Flipped => (true, false, false),
        Transform::Flipped90 => (false, false, true),
        Transform::Flipped180 => (false, true, false),
        Transform::Flipped270 => (true, true, true),
        _ => (false, false, false),
    };
    // Mirrors commute, so the row order folds into the vertical one.
    Orientation { flip_x, flip_y: flip_y ^ y_invert, transpose }
}

struct Output {
    proxy: wl_output::WlOutput,
    connector: Option<String>,
    transform: wl_output::Transform,
    slot: Slot,
}

impl Output {
    fn new(proxy: wl_output::WlOutput) -> Self {
        Output {
            proxy,
            connector: None,
            transform: wl_output::Transform::Normal,
            slot: Slot::default(),
        }
    }
}

/// One output's frame as it comes together.
#[derive(Default)]
struct Slot {
    /// Set together with the geometry below, from the first buffer type
    /// offered whose format the downscale can read.
    channels: Option<[u32; 3]>,
    format: Option<wl_shm::Format>,
    width: u32,
    height: u32,
    stride: u32,
    y_invert: bool,
    file: Option<File>,
    requested: bool,
    /// Nothing more is coming for this frame: it is ready, failed, or was
    /// given up on.
    finished: bool,
    ready: bool,
}

struct State {
    shm: wl_shm::WlShm,
    manager_version: u32,
    outputs: Vec<Output>,
}

impl State {
    /// Allocate the shm buffer and ask the compositor to fill it, once every
    /// buffer type for the frame has been offered. Gives up on the frame
    /// straight away if none of them is usable, rather than leaving the wait
    /// to run into its deadline for a copy that will never be made.
    fn request_copy(
        &mut self,
        index: usize,
        frame: &ZwlrScreencopyFrameV1,
        qh: &QueueHandle<Self>,
    ) {
        let slot = &mut self.outputs[index].slot;
        if slot.requested || slot.finished {
            return;
        }
        let size = slot.stride as u64 * slot.height as u64;
        let (Some(format), Ok(pool_size)) = (slot.format, i32::try_from(size)) else {
            return self.finish(index, frame, false);
        };
        let Some(fd) = memfd(size) else { return self.finish(index, frame, false) };
        let pool = self.shm.create_pool(fd.as_fd(), pool_size, qh, ());
        let buffer = pool.create_buffer(
            0,
            slot.width as i32,
            slot.height as i32,
            slot.stride as i32,
            format,
            qh,
            (),
        );
        pool.destroy();
        slot.file = Some(File::from(fd));
        slot.requested = true;
        frame.copy(&buffer);
    }

    fn finish(&mut self, index: usize, frame: &ZwlrScreencopyFrameV1, ready: bool) {
        let slot = &mut self.outputs[index].slot;
        slot.finished = true;
        slot.ready = ready;
        frame.destroy();
    }
}

fn memfd(size: u64) -> Option<OwnedFd> {
    let fd = rustix::fs::memfd_create("launchr-capture", rustix::fs::MemfdFlags::CLOEXEC).ok()?;
    rustix::fs::ftruncate(&fd, size).ok()?;
    Some(fd)
}

impl Dispatch<ZwlrScreencopyFrameV1, usize> for State {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        &index: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::{Event, Flags};
        match event {
            Event::Buffer { format, width, height, stride } => {
                let slot = &mut state.outputs[index].slot;
                if slot.format.is_none()
                    && let WEnum::Value(format) = format
                    && let Some(channels) = channel_shifts(format)
                {
                    slot.format = Some(format);
                    slot.channels = Some(channels);
                    slot.width = width;
                    slot.height = height;
                    slot.stride = stride;
                }
                // Before version 3 this is the only buffer type offered;
                // from 3 on the list ends with buffer_done.
                if state.manager_version < 3 {
                    state.request_copy(index, frame, qh);
                }
            }
            Event::BufferDone => state.request_copy(index, frame, qh),
            Event::Flags { flags: WEnum::Value(flags) } => {
                state.outputs[index].slot.y_invert = flags.contains(Flags::YInvert);
            }
            Event::Ready { .. } => state.finish(index, frame, true),
            Event::Failed => state.finish(index, frame, false),
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        &index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let output = &mut state.outputs[index];
        match event {
            wl_output::Event::Name { name } => output.connector = Some(name),
            wl_output::Event::Geometry { transform: WEnum::Value(transform), .. } => {
                output.transform = transform;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
