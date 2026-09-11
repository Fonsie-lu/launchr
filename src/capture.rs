//! Screen capture through `wlr-screencopy-v1`.
//!
//! The launcher blurs its own backdrop instead of asking the compositor for a
//! blur rule, so it looks the same on river, sway, Hyprland or anything else
//! wlroots-based. Capturing happens before the window is mapped, so the
//! snapshot never contains the launcher itself.

use std::fs::File;
use std::ops::Deref;
use std::os::fd::{AsFd, OwnedFd};
use std::ptr::NonNull;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};
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
    /// Byte offsets of red, green and blue within each 4 byte pixel.
    pub channels: (usize, usize, usize),
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

/// Capture every output. Returns an empty vec when the compositor does not
/// implement screencopy — the caller then falls back to dimming only.
pub fn capture_outputs() -> Vec<Frame> {
    match try_capture() {
        Ok(frames) => frames,
        Err(error) => {
            eprintln!("launchr: screen capture unavailable ({error}), falling back to dim only");
            Vec::new()
        }
    }
}

fn try_capture() -> Result<Vec<Frame>, String> {
    let connection = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&connection).map_err(|e| e.to_string())?;
    let qh = queue.handle();

    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(|_| "no wl_shm")?;
    let manager: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .map_err(|_| "no wlr-screencopy-v1")?;
    let manager_version = manager.version();

    let outputs: Vec<_> = globals.contents().clone_list();
    let mut state = State { shm, manager_version, outputs: Vec::new(), slots: Vec::new(), pending: 0 };
    for global in outputs.iter().filter(|g| g.interface == "wl_output") {
        let index = state.outputs.len();
        let output: wl_output::WlOutput =
            globals
                .registry()
                .bind(global.name, global.version.min(4), &qh, index);
        state.outputs.push(Output { connector: None, proxy: output });
    }
    if state.outputs.is_empty() {
        return Err("no outputs".to_owned());
    }
    // Connector names only matter when there is more than one shot to pair
    // with a monitor, and the round trip that delivers them is a full
    // compositor turnaround, so a single output skips it.
    if state.outputs.len() > 1 {
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    }

    for index in 0..state.outputs.len() {
        manager.capture_output(0, &state.outputs[index].proxy, &qh, index);
        state.slots.push(Slot::default());
        state.pending += 1;
    }

    // Bounded so a compositor that never answers cannot hang the launcher.
    for _ in 0..200 {
        if state.pending == 0 {
            break;
        }
        queue.blocking_dispatch(&mut state).map_err(|e| e.to_string())?;
    }

    let mut frames = Vec::new();
    for (index, slot) in state.slots.iter().enumerate() {
        if !slot.ready {
            continue;
        }
        let (Some(file), Some(format)) = (slot.file.as_ref(), slot.format) else { continue };
        let size = (slot.stride * slot.height) as usize;
        let Some(mapping) = Mapping::new(file, size) else { continue };
        frames.push(Frame {
            connector: state.outputs[index].connector.clone(),
            width: slot.width,
            height: slot.height,
            stride: slot.stride,
            channels: channel_offsets(format),
            data: Pixels::Mapped(mapping),
        });
    }
    Ok(frames)
}

/// Byte offsets of R, G and B inside a 32 bit pixel of the given format.
fn channel_offsets(format: wl_shm::Format) -> (usize, usize, usize) {
    match format {
        // Little endian 0xAARRGGBB lands in memory as B, G, R, A.
        wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => (2, 1, 0),
        _ => (0, 1, 2),
    }
}

fn is_supported(format: wl_shm::Format) -> bool {
    matches!(
        format,
        wl_shm::Format::Xrgb8888
            | wl_shm::Format::Argb8888
            | wl_shm::Format::Xbgr8888
            | wl_shm::Format::Abgr8888
    )
}

struct Output {
    connector: Option<String>,
    proxy: wl_output::WlOutput,
}

#[derive(Default)]
struct Slot {
    format: Option<wl_shm::Format>,
    width: u32,
    height: u32,
    stride: u32,
    file: Option<File>,
    requested: bool,
    ready: bool,
}

struct State {
    shm: wl_shm::WlShm,
    manager_version: u32,
    outputs: Vec<Output>,
    slots: Vec<Slot>,
    pending: usize,
}

impl State {
    /// Allocate the shm buffer and ask the compositor to fill it. No-op until
    /// a usable format has been advertised, and only ever runs once per frame.
    fn request_copy(&mut self, index: usize, frame: &ZwlrScreencopyFrameV1, qh: &QueueHandle<Self>) {
        let slot = &mut self.slots[index];
        if slot.requested || slot.format.is_none() {
            return;
        }
        let size = slot.stride as u64 * slot.height as u64;
        let Some(fd) = memfd(size) else { return };
        let pool = self.shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            slot.width as i32,
            slot.height as i32,
            slot.stride as i32,
            slot.format.unwrap(),
            qh,
            (),
        );
        pool.destroy();
        slot.file = Some(File::from(fd));
        slot.requested = true;
        frame.copy(&buffer);
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
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer { format, width, height, stride } => {
                let slot = &mut state.slots[index];
                if slot.format.is_none() {
                    if let WEnum::Value(format) = format {
                        if is_supported(format) {
                            slot.format = Some(format);
                            slot.width = width;
                            slot.height = height;
                            slot.stride = stride;
                        }
                    }
                }
                // Version 3 batches the formats and ends with buffer_done.
                if state.manager_version < 3 {
                    state.request_copy(index, frame, qh);
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => state.request_copy(index, frame, qh),
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.slots[index].ready = true;
                state.pending = state.pending.saturating_sub(1);
                frame.destroy();
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.pending = state.pending.saturating_sub(1);
                frame.destroy();
            }
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
        if let wl_output::Event::Name { name } = event {
            state.outputs[index].connector = Some(name);
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
