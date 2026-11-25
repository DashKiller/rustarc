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
use zbus::{blocking::connection::Builder as BusBuilder, interface};
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

// KDE's drag-extract protocol (used by Ark): the drag carries no files, only the name of a
// D-Bus service. When dropped, the file manager (KIO) calls
// `org.kde.ark.DndExtract.extractSelectedFilesTo(dir)` on it and we extract straight there.
const ARK_SERVICE: &str = "application/x-kde-ark-dndextract-service";
const ARK_PATH: &str = "application/x-kde-ark-dndextract-path";
const DND_PATH: &str = "/DndExtract";

/// Also offer a plain file list (needs the files to be extracted before the drag can be
/// answered, so it is slow). Used when D-Bus is unavailable, or with RUSTARC_DRAG_URIS=1.
fn uri_fallback() -> bool {
    std::env::var_os("RUSTARC_DRAG_URIS").is_some()
}

struct DndExtract {
    tx: Sender<PathBuf>,
    ctx: eframe::egui::Context,
}

#[interface(name = "org.kde.ark.DndExtract")]
impl DndExtract {
    #[zbus(name = "extractSelectedFilesTo")]
    fn extract_selected_files_to(&self, path: String) {
        let dest = match path.strip_prefix("file://") {
            Some(rest) => PathBuf::from(percent_decode(rest.find('/').map(|i| &rest[i..]).unwrap_or(rest))),
            None => PathBuf::from(path),
        };
        let _ = self.tx.send(dest);
        self.ctx.request_repaint();
    }
}

// ───────────────────────── drag OUT (app -> file manager) ─────────────────────────

/// Shared between the UI (which extracts files in the background) and the Wayland thread
/// (which answers the drop target). Drop targets such as Dolphin/Plasma read the file list
/// once, while the pointer hovers, and reuse it for the drop, so the list is only handed
/// over once the files really exist on disk.
pub struct DragState {
    paths: Vec<PathBuf>,
    done: Mutex<Option<Result<(), String>>>,
    cv: Condvar,
    /// the compositor reported the drag as cancelled (Escape, dropped on nothing, ...)
    cancelled: AtomicBool,
    /// the drop target accepted the drop and is finished with it
    finished: AtomicBool,
}

impl DragState {
    pub fn new(paths: Vec<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            paths,
            done: Mutex::new(None),
            cv: Condvar::new(),
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        })
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
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
    /// the D-Bus drag-extract service is up
    has_dbus: bool,
}

impl DragHandle {
    /// Start dragging. Returns None if no drag could be started (no button press seen yet),
    /// otherwise whether the caller must extract the files up front (file-list mode).
    /// In the normal D-Bus mode nothing is extracted until the drop target asks for it.
    pub fn start(&self, state: Arc<DragState>) -> Option<bool> {
        let serial = self.serial.load(Ordering::Relaxed);
        if serial == 0 {
            return None;
        }
        let uri_mode = !self.has_dbus || uri_fallback();
        let source = self.manager.create_data_source(&self.qh, state);
        if self.has_dbus {
            source.offer(ARK_SERVICE.to_string());
            source.offer(ARK_PATH.to_string());
        }
        if uri_mode {
            source.offer(URI_LIST.to_string());
        }
        if self.manager.version() >= 3 {
            source.set_actions(DndAction::Copy);
        }
        self.device.start_drag(Some(&source), &self.surface, None, serial);
        let _ = self.conn.flush();
        Some(uri_mode)
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
    extract_tx: Sender<PathBuf>,
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
        if let Err(e) = run(display, surface, tx, hovering, drag, ctx, extract_tx) {
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
    /// our D-Bus unique name, handed to drop targets that speak the Ark protocol
    service: String,
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
    extract_tx: Sender<PathBuf>,
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

    // D-Bus service the file manager calls when something dragged from us is dropped.
    let bus = BusBuilder::session()
        .and_then(|b| b.serve_at(DND_PATH, DndExtract { tx: extract_tx, ctx: ctx.clone() }))
        .and_then(|b| b.build());
    let (service, _bus) = match bus {
        Ok(c) => (c.unique_name().map(|n| n.to_string()).unwrap_or_default(), Some(c)),
        Err(e) => {
            eprintln!("rustarc: D-Bus drag extraction unavailable ({e}); using file lists instead");
            (String::new(), None)
        }
    };

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
                has_dbus: !service.is_empty(),
            });
        }
    }

    let mut state = State {
        conn: conn.clone(),
        ctx,
        tx,
        hovering,
        serial,
        service,
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
        state: &mut Self,
        source: &WlDataSource,
        event: wl_data_source::Event,
        data: &Arc<DragState>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                if mime_type == ARK_SERVICE {
                    let _ = File::from(fd).write_all(state.service.as_bytes());
                } else if mime_type == ARK_PATH {
                    let _ = File::from(fd).write_all(DND_PATH.as_bytes());
                } else if mime_type == URI_LIST {
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
            wl_data_source::Event::Cancelled => {
                data.cancelled.store(true, Ordering::Relaxed);
                source.destroy();
                state.ctx.request_repaint(); // let the UI drop its "Dragging…" status
            }
            wl_data_source::Event::DndFinished => {
                data.finished.store(true, Ordering::Relaxed);
                source.destroy();
                state.ctx.request_repaint();
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
