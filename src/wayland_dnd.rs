//! Native Wayland drag-and-drop (winit has none): files dropped INTO the window, and
//! files dragged OUT of it.
//!
//! We attach our own `wl_data_device` to the same Wayland connection winit uses (via the
//! raw display pointer) and run its event queue on a worker thread, the same technique
//! smithay-clipboard uses for the clipboard.

use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    os::fd::AsFd,
    os::unix::ffi::OsStrExt,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::Sender,
        Arc, Condvar, Mutex,
    },
    thread,
    time::Duration,
};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::{
    backend::{Backend, ObjectId},
    delegate_noop, event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::{DndAction, WlDataDeviceManager},
        wl_data_offer::{self, WlDataOffer},
        wl_data_source::{self, WlDataSource},
        wl_pointer::{self, ButtonState, WlPointer},
        wl_registry,
        wl_seat::WlSeat,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};

const URI_LIST: &str = "text/uri-list";

// ───────────────────────── drag OUT (app -> file manager) ─────────────────────────

/// Shared between the UI (which extracts files in the background) and the Wayland thread
/// (which answers the drop target). Drop targets such as Dolphin/Plasma read the file list
/// once, while the pointer hovers, and reuse it for the drop, so the list is only handed
/// over once the files really exist on disk.
pub struct DragState {
    paths: Vec<PathBuf>,
    done: Mutex<Option<Result<(), String>>>,
    cv: Condvar,
}

impl DragState {
    pub fn new(paths: Vec<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            paths,
            done: Mutex::new(None),
            cv: Condvar::new(),
        })
    }

    /// Called by the extraction thread when it is finished.
    pub fn finish(&self, r: Result<(), String>) {
        *self.done.lock().unwrap() = Some(r);
        self.cv.notify_all();
    }

    fn uri_list(&self) -> String {
        self.paths.iter().map(|p| format!("file://{}\r\n", percent_encode(p))).collect()
    }

    /// None while extraction is still running, otherwise whether it succeeded.
    fn status(&self) -> Option<bool> {
        self.done.lock().unwrap().as_ref().map(|r| r.is_ok())
    }

    /// Blocks until extraction is finished; true if it succeeded.
    fn wait_done(&self) -> bool {
        let mut guard = self.done.lock().unwrap();
        while guard.is_none() {
            guard = self.cv.wait(guard).unwrap();
        }
        guard.as_ref().map(|r| r.is_ok()).unwrap_or(false)
    }
}

fn percent_encode(p: &Path) -> String {
    let mut out = String::new();
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Lets the UI start a compositor-driven drag.
pub struct DragHandle {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: WlDataDeviceManager,
    device: WlDataDevice,
    surface: WlSurface,
    serial: Arc<AtomicU32>,
}

impl DragHandle {
    /// Start dragging. `state` is completed later with the extracted file paths.
    /// Returns false if no valid button-press serial is known yet.
    pub fn start(&self, state: Arc<DragState>) -> bool {
        let serial = self.serial.load(Ordering::Relaxed);
        if serial == 0 {
            return false;
        }
        let source = self.manager.create_data_source(&self.qh, state);
        source.offer(URI_LIST.to_string());
        if self.manager.version() >= 3 {
            source.set_actions(DndAction::Copy);
        }
        self.device.start_drag(Some(&source), &self.surface, None, serial);
        let _ = self.conn.flush();
        true
    }
}

pub type SharedDrag = Arc<Mutex<Option<DragHandle>>>;

// ───────────────────────── setup ─────────────────────────

/// Start the Wayland worker if (and only if) the window runs on native Wayland.
pub fn start(
    cc: &eframe::CreationContext<'_>,
    tx: Sender<PathBuf>,
    hovering: Arc<AtomicBool>,
    drag: SharedDrag,
) {
    let Ok(handle) = cc.display_handle() else { return };
    let RawDisplayHandle::Wayland(h) = handle.as_raw() else { return };
    let display = h.display.as_ptr() as usize;
    let surface = cc.window_handle().ok().and_then(|w| match w.as_raw() {
        RawWindowHandle::Wayland(w) => Some(w.surface.as_ptr() as usize),
        _ => None,
    });
    let ctx = cc.egui_ctx.clone();

    thread::spawn(move || {
        if let Err(e) = run(display, surface, tx, hovering, drag, ctx) {
            eprintln!("rustarc: Wayland drag-and-drop unavailable: {e}");
        }
    });
}

struct State {
    conn: Connection,
    ctx: eframe::egui::Context,
    tx: Sender<PathBuf>,
    hovering: Arc<AtomicBool>,
    serial: Arc<AtomicU32>,
    offers: HashMap<ObjectId, Vec<String>>,
    dnd_offer: Option<WlDataOffer>,
}

fn run(
    display: usize,
    surface: Option<usize>,
    tx: Sender<PathBuf>,
    hovering: Arc<AtomicBool>,
    drag: SharedDrag,
    ctx: eframe::egui::Context,
) -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: the pointers come from winit's live wl_display / wl_surface and stay valid
    // for the lifetime of the application window.
    let backend = unsafe { Backend::from_foreign_display(display as *mut _) };
    let conn = Connection::from_backend(backend);

    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let manager: WlDataDeviceManager = globals.bind(&qh, 1..=3, ())?;
    let seat: WlSeat = globals.bind(&qh, 1..=1, ())?;
    let device = manager.get_data_device(&seat, &qh, ());
    // We only listen to pointer buttons to learn the serial a drag must be started with.
    let _pointer = seat.get_pointer(&qh, ());

    let serial = Arc::new(AtomicU32::new(0));

    if let Some(ptr) = surface {
        let wl_surface = unsafe { ObjectId::from_ptr(WlSurface::interface(), ptr as *mut _) }
            .ok()
            .and_then(|id| WlSurface::from_id(&conn, id).ok());
        if let Some(wl_surface) = wl_surface {
            *drag.lock().unwrap() = Some(DragHandle {
                conn: conn.clone(),
                qh: qh.clone(),
                manager: manager.clone(),
                device: device.clone(),
                surface: wl_surface,
                serial: serial.clone(),
            });
        }
    }

    let mut state = State {
        conn: conn.clone(),
        ctx,
        tx,
        hovering,
        serial,
        offers: HashMap::new(),
        dnd_offer: None,
    };

    loop {
        queue.blocking_dispatch(&mut state)?;
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

delegate_noop!(State: WlDataDeviceManager);
delegate_noop!(State: ignore WlSeat);

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Button { serial, state: WEnum::Value(ButtonState::Pressed), .. } = event {
            state.serial.store(serial, Ordering::Relaxed);
        }
    }
}

impl Dispatch<WlDataSource, Arc<DragState>> for State {
    fn event(
        _: &mut Self,
        source: &WlDataSource,
        event: wl_data_source::Event,
        data: &Arc<DragState>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                if mime_type == URI_LIST {
                    let uris = data.uri_list();
                    match data.status() {
                        // extraction failed: hand over nothing
                        Some(false) => {}
                        // files are ready: answer right away
                        Some(true) => {
                            let _ = File::from(fd).write_all(uris.as_bytes());
                        }
                        // still extracting: answer once it is done, from a helper thread so
                        // the Wayland thread keeps running
                        None => {
                            let data = data.clone();
                            thread::spawn(move || {
                                if data.wait_done() {
                                    let _ = File::from(fd).write_all(uris.as_bytes());
                                }
                            });
                        }
                    }
                }
            }
            wl_data_source::Event::Cancelled | wl_data_source::Event::DndFinished => {
                source.destroy();
            }
            _ => {}
        }
    }
}

// ───────────────────────── drop INTO the window ─────────────────────────

impl Dispatch<WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        offer: &WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                state.offers.insert(id.id(), Vec::new());
            }
            wl_data_device::Event::Enter { serial, id: Some(offer), .. } => {
                let accepts = state
                    .offers
                    .get(&offer.id())
                    .map(|m| m.iter().any(|m| m == URI_LIST))
                    .unwrap_or(false);
                if accepts {
                    offer.accept(serial, Some(URI_LIST.to_string()));
                    if offer.version() >= 3 {
                        offer.set_actions(DndAction::Copy, DndAction::Copy);
                    }
                    state.hovering.store(true, Ordering::Relaxed);
                    state.ctx.request_repaint();
                }
                state.dnd_offer = Some(offer);
            }
            wl_data_device::Event::Leave => {
                state.hovering.store(false, Ordering::Relaxed);
                state.dnd_offer = None;
                state.ctx.request_repaint();
            }
            wl_data_device::Event::Drop => {
                state.hovering.store(false, Ordering::Relaxed);
                if let Some(offer) = state.dnd_offer.take() {
                    receive_drop(state, offer);
                }
                state.ctx.request_repaint();
            }
            _ => {}
        }
    }

    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

fn receive_drop(state: &State, offer: WlDataOffer) {
    let Ok((reader, writer)) = UnixStream::pair() else { return };
    offer.receive(URI_LIST.to_string(), writer.as_fd());
    let _ = state.conn.flush();
    drop(writer); // so the reader sees EOF once the source is done

    let (tx, ctx, conn) = (state.tx.clone(), state.ctx.clone(), state.conn.clone());
    thread::spawn(move || {
        let mut reader = reader;
        let _ = reader.set_read_timeout(Some(Duration::from_secs(5)));
        let mut text = String::new();
        if reader.read_to_string(&mut text).is_ok() {
            if let Some(path) = text.lines().find_map(uri_to_path) {
                let _ = tx.send(path);
                ctx.request_repaint();
            }
        }
        if offer.version() >= 3 {
            offer.finish();
        }
        let _ = conn.flush();
    });
}

/// "file:///home/me/a%20b.7z" -> /home/me/a b.7z
fn uri_to_path(line: &str) -> Option<PathBuf> {
    let rest = line.trim().strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..]; // skip optional host
    Some(PathBuf::from(percent_decode(path)))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
