//! egui front-end, styled after KDE's Ark.

use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Receiver, Sender},
        Arc,
    },
    thread,
};

use eframe::egui::{self, Align, Color32, Layout, Pos2, Rect, Sense, Vec2};
use egui_extras::{Column, TableBuilder};

use crate::archive::{self, Entry, Format};
use crate::icons::{self, Icon};
use crate::wayland_dnd::{DragState, SharedDrag};

// ───────────────────────────── tree model ─────────────────────────────

struct Node {
    name: String,
    /// Full path inside the archive, without a trailing slash.
    full: String,
    is_dir: bool,
    size: u64,
    packed: Option<u64>,
    crc: Option<u32>,
    method: Option<String>,
    mode: Option<String>,
    owner: Option<String>,
    group: Option<String>,
    date: Option<String>,
    version: Option<String>,
    children: Vec<usize>,
}

#[derive(Default)]
struct Tree {
    nodes: Vec<Node>,
    roots: Vec<usize>,
}

fn build_tree(entries: &[Entry]) -> Tree {
    let mut nodes: Vec<Node> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();

    for e in entries {
        let comps: Vec<&str> = e.name.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
        if comps.is_empty() {
            continue;
        }
        let mut cur = String::new();
        let mut parent: Option<usize> = None;
        for (i, c) in comps.iter().enumerate() {
            if !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(c);
            let last = i + 1 == comps.len();
            let idx = match index.get(&cur) {
                Some(&i) => i,
                None => {
                    let idx = nodes.len();
                    nodes.push(Node {
                        name: c.to_string(),
                        full: cur.clone(),
                        is_dir: !last || e.is_dir,
                        size: 0,
                        packed: None,
                        crc: None,
                        method: None,
                        mode: None,
                        owner: None,
                        group: None,
                        date: None,
                        version: None,
                        children: Vec::new(),
                    });
                    index.insert(cur.clone(), idx);
                    match parent {
                        Some(p) => nodes[p].children.push(idx),
                        None => roots.push(idx),
                    }
                    idx
                }
            };
            if last {
                let n = &mut nodes[idx];
                n.is_dir = n.is_dir || e.is_dir;
                n.size = e.size;
                n.packed = e.packed;
                n.crc = e.crc;
                n.method = e.method.clone();
                n.mode = e.mode.clone();
                n.owner = e.owner.clone();
                n.group = e.group.clone();
                n.date = e.date.clone();
                n.version = e.version.clone();
            }
            parent = Some(idx);
        }
    }

    fn sum(nodes: &mut Vec<Node>, i: usize) -> u64 {
        if !nodes[i].is_dir {
            return nodes[i].size;
        }
        let kids = nodes[i].children.clone();
        let mut total = 0;
        for k in kids {
            total += sum(nodes, k);
        }
        nodes[i].size = total;
        total
    }
    for r in roots.clone() {
        sum(&mut nodes, r);
    }

    // folders first, then case-insensitive by name
    let keys: Vec<(bool, String)> = nodes.iter().map(|n| (!n.is_dir, n.name.to_lowercase())).collect();
    for i in 0..nodes.len() {
        let mut ch = std::mem::take(&mut nodes[i].children);
        ch.sort_by(|a, b| keys[*a].cmp(&keys[*b]));
        nodes[i].children = ch;
    }
    roots.sort_by(|a, b| keys[*a].cmp(&keys[*b]));

    Tree { nodes, roots }
}

/// Flatten the tree into the rows that are currently visible: (node index, depth).
fn visible_rows(tree: &Tree, expanded: &HashSet<String>, needle: &str) -> Vec<(usize, usize)> {
    fn mark(tree: &Tree, i: usize, needle: &str, out: &mut Vec<bool>) -> bool {
        let mut any = tree.nodes[i].name.to_lowercase().contains(needle);
        for &c in &tree.nodes[i].children {
            if mark(tree, c, needle, out) {
                any = true;
            }
        }
        out[i] = any;
        any
    }
    fn walk(
        tree: &Tree,
        i: usize,
        depth: usize,
        expanded: &HashSet<String>,
        matches: Option<&Vec<bool>>,
        rows: &mut Vec<(usize, usize)>,
    ) {
        if let Some(m) = matches {
            if !m[i] {
                return;
            }
        }
        rows.push((i, depth));
        let n = &tree.nodes[i];
        if n.is_dir && (matches.is_some() || expanded.contains(&n.full)) {
            for &c in &n.children {
                walk(tree, c, depth + 1, expanded, matches, rows);
            }
        }
    }

    let matches = if needle.is_empty() {
        None
    } else {
        let mut m = vec![false; tree.nodes.len()];
        for &r in &tree.roots {
            mark(tree, r, needle, &mut m);
        }
        Some(m)
    };
    let mut rows = Vec::new();
    for &r in &tree.roots {
        walk(tree, r, 0, expanded, matches.as_ref(), &mut rows);
    }
    rows
}

// ───────────────────────────── columns ────────────────────────────────

const NCOLS: usize = 10;
const COL_NAMES: [&str; NCOLS] = [
    "Original Size", "Compressed Size", "Mode", "Owner", "Group",
    "Rate", "CRC checksum", "Method", "Version", "Date",
];

/// Column preferences are remembered per archive family.
fn cols_key(f: Format) -> &'static str {
    match f {
        Format::SevenZ => "7z",
        Format::Rar => "rar",
        Format::Zip => "zip",
        Format::Tar(_) => "tar",
        Format::Single(_) => "single",
    }
}

/// Default columns, following KDE Ark. (ZIP and single-file defaults are best guesses.)
fn default_cols(f: Format) -> [bool; NCOLS] {
    let on: &[usize] = match f {
        Format::Rar => &[0, 1, 2, 5, 6, 7, 8],
        Format::SevenZ => &[0, 2, 6, 7, 9],
        Format::Tar(_) => &[0, 2, 3, 4, 9],
        Format::Zip => &[0, 1, 5, 6, 7, 9],
        Format::Single(_) => &[0],
    };
    let mut v = [false; NCOLS];
    for &i in on {
        v[i] = true;
    }
    v
}

#[derive(Clone, Copy)]
enum MenuAction {
    Preview,
    OpenExternal,
    OpenWith,
    Rename,
    Remove,
    ExtractSelected,
    ExtractAll,
    Cut,
    Copy,
    Paste,
    AddFiles,
}

// ──────────────────────────── app state ───────────────────────────────

enum Msg {
    Status(String),
    Opened(PathBuf, Result<(Format, Vec<Entry>), String>),
    Done(Result<String, String>),
    Preview(String, Result<String, String>),
}

pub struct App {
    path: Option<PathBuf>,
    format: Option<Format>,
    tree: Tree,
    /// Visible optional columns: Original Size, Compressed Size, CRC checksum, Method.
    show_cols: [bool; NCOLS],
    col_prefs: HashMap<String, [bool; NCOLS]>,
    cols_key: String,
    table_gen: u64,
    /// Longest cell texts per optional column, used to size the columns.
    col_samples: Vec<Vec<String>>,
    expanded: HashSet<String>,
    selected: HashSet<String>,
    last_clicked: Option<String>,
    filter: String,
    show_find: bool,
    focus_find: bool,
    password: String,
    show_password: bool,
    pw_retry: Option<PathBuf>,
    preview: Option<(String, String)>,
    confirm_remove: Option<Vec<String>>,
    rename: Option<(String, String)>,
    clipboard: Option<(PathBuf, Vec<String>, bool)>,
    status: String,
    busy: bool,
    pending_open: Option<PathBuf>,
    last_title: String,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    startup: Option<String>,
    styled: bool,
    dnd_rx: Receiver<PathBuf>,
    dnd_hovering: Arc<AtomicBool>,
    drag: SharedDrag,
    /// destination folders sent by the drop target (KDE drag-extract protocol)
    extract_rx: Receiver<PathBuf>,
    /// what the current drag carries: (archive, top-level items)
    drag_job: Option<(PathBuf, Vec<String>)>,
}

pub fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("rustarc-{}", std::process::id()))
}

impl App {
    pub fn new(
        startup: Option<String>,
        dnd_rx: Receiver<PathBuf>,
        dnd_hovering: Arc<AtomicBool>,
        drag: SharedDrag,
        extract_rx: Receiver<PathBuf>,
    ) -> Self {
        let (tx, rx) = channel();
        Self {
            path: None,
            format: None,
            tree: Tree::default(),
            show_cols: default_cols(Format::SevenZ),
            col_prefs: HashMap::new(),
            cols_key: String::new(),
            table_gen: 0,
            col_samples: vec![Vec::new(); NCOLS],
            expanded: HashSet::new(),
            selected: HashSet::new(),
            last_clicked: None,
            filter: String::new(),
            show_find: false,
            focus_find: false,
            password: String::new(),
            show_password: false,
            pw_retry: None,
            preview: None,
            confirm_remove: None,
            rename: None,
            clipboard: None,
            status: "Open an archive (or drop one here).".into(),
            busy: false,
            pending_open: None,
            last_title: String::new(),
            tx,
            rx,
            startup,
            styled: false,
            dnd_rx,
            dnd_hovering,
            drag,
            extract_rx,
            drag_job: None,
        }
    }

    // ── background helpers ──

    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = format!("Opening {}…", path.display());
        let (tx, ctx, pw) = (self.tx.clone(), ctx.clone(), self.password.clone());
        thread::spawn(move || {
            let r = archive::list(&path, &pw);
            let _ = tx.send(Msg::Opened(path, r));
            ctx.request_repaint();
        });
    }

    fn run<F>(&mut self, ctx: &egui::Context, label: &str, f: F)
    where
        F: FnOnce(&dyn Fn(String)) -> Result<String, String> + Send + 'static,
    {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = label.into();
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        thread::spawn(move || {
            let tx2 = tx.clone();
            let ctx2 = ctx.clone();
            let progress = move |s: String| {
                let _ = tx2.send(Msg::Status(s));
                ctx2.request_repaint();
            };
            let r = f(&progress);
            let _ = tx.send(Msg::Done(r));
            ctx.request_repaint();
        });
    }

    fn pump(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Status(s) => self.status = s,
                Msg::Opened(path, res) => {
                    self.busy = false;
                    match res {
                        Ok((fmt, entries)) => {
                            self.tree = build_tree(&entries);
                            self.col_samples = (0..NCOLS)
                                .map(|k| {
                                    let mut v: Vec<String> =
                                        self.tree.nodes.iter().filter_map(|n| cell_text(n, k)).collect();
                                    v.sort_by_key(|s| std::cmp::Reverse(s.chars().count()));
                                    v.dedup();
                                    v.truncate(3);
                                    v
                                })
                                .collect();
                            self.expanded.clear(); // folders start collapsed
                            self.status = format!("{} archive", fmt.label());
                            let key = cols_key(fmt);
                            self.cols_key = key.to_string();
                            self.table_gen += 1;
                            self.show_cols = self.col_prefs.get(key).copied().unwrap_or_else(|| default_cols(fmt));
                            self.format = Some(fmt);
                            self.selected.clear();
                            self.last_clicked = None;
                            self.path = Some(path);
                        }
                        Err(e) => {
                            if e.to_lowercase().contains("password") {
                                self.pw_retry = Some(path);
                                self.show_password = true;
                                self.status = "This archive needs a password.".into();
                            } else {
                                self.status = format!("Error: {e}");
                            }
                        }
                    }
                }
                Msg::Done(res) => {
                    self.busy = false;
                    self.status = match res {
                        Ok(s) => s,
                        Err(e) => format!("Error: {e}"),
                    };
                    if let Some(p) = self.pending_open.take() {
                        self.open(ctx, p);
                    }
                }
                Msg::Preview(name, res) => {
                    self.busy = false;
                    match res {
                        Ok(text) => {
                            self.status = format!("Preview: {name}");
                            self.preview = Some((name, text));
                        }
                        Err(e) => self.status = e,
                    }
                }
            }
        }
    }

    // ── actions ──

    fn pick_and_open(&mut self, ctx: &egui::Context) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter(
                "Archives",
                &[
                    "7z", "rar", "zip", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst",
                    "tzst", "jar", "apk", "cbz", "001",
                ],
            )
            .pick_file()
        {
            self.open(ctx, p);
        }
    }

    fn selected_files(&self) -> Vec<String> {
        self.tree
            .nodes
            .iter()
            .filter(|n| !n.is_dir && self.selected.contains(&n.full))
            .map(|n| n.full.clone())
            .collect()
    }

    fn extract(&mut self, ctx: &egui::Context, only_selected: bool) {
        let Some(path) = self.path.clone() else { return };
        let Some(dest) = rfd::FileDialog::new().set_title("Extract to…").pick_folder() else {
            return;
        };
        let sel: Option<HashSet<String>> = only_selected.then(|| self.selected.clone());
        let pw = self.password.clone();
        self.run(ctx, "Extracting…", move |p| {
            archive::extract(&path, &dest, sel.as_ref(), &pw, p)
        });
    }

    fn test(&mut self, ctx: &egui::Context) {
        let Some(path) = self.path.clone() else { return };
        let pw = self.password.clone();
        self.run(ctx, "Testing…", move |p| archive::test(&path, &pw, p));
    }

    fn open_external(&mut self, ctx: &egui::Context, ask: bool) {
        let Some(path) = self.path.clone() else { return };
        let Some(full) = self.selected_files().into_iter().next() else { return };
        let pw = self.password.clone();
        self.run(ctx, "Opening…", move |p| {
            let dest = temp_dir();
            let mut sel = HashSet::new();
            sel.insert(full.clone());
            archive::extract(&path, &dest, Some(&sel), &pw, p)?;
            let target = dest.join(full.rsplit('/').next().unwrap_or(&full));
            if ask {
                return archive::open_with_chooser(&target);
            }
            std::process::Command::new("xdg-open")
                .arg(&target)
                .spawn()
                .map_err(|e| format!("Could not start xdg-open: {e}"))?;
            Ok(format!("Opened {full}"))
        });
    }

    fn preview_selected(&mut self, ctx: &egui::Context) {
        if self.busy {
            return;
        }
        let Some(path) = self.path.clone() else { return };
        let Some(full) = self.selected_files().into_iter().next() else { return };
        let pw = self.password.clone();
        self.busy = true;
        self.status = "Preparing preview…".into();
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        thread::spawn(move || {
            let result = (|| -> Result<String, String> {
                let dest = temp_dir();
                let mut sel = HashSet::new();
                sel.insert(full.clone());
                archive::extract(&path, &dest, Some(&sel), &pw, &|_| {})?;
                let mut buf = Vec::new();
                std::fs::File::open(dest.join(full.rsplit('/').next().unwrap_or(&full)))
                    .map_err(|e| e.to_string())?
                    .take(512 * 1024)
                    .read_to_end(&mut buf)
                    .map_err(|e| e.to_string())?;
                if buf.contains(&0) {
                    return Err("Binary file – no text preview. Use \"Open in External Application\".".into());
                }
                Ok(String::from_utf8_lossy(&buf).into_owned())
            })();
            let _ = tx.send(Msg::Preview(full, result));
            ctx.request_repaint();
        });
    }

    /// Drag the selected items out of the window. On KDE the drop target tells us where to
    /// extract them (like Ark); otherwise they are extracted to a temp folder first.
    fn begin_drag_out(&mut self) {
        let Some(path) = self.path.clone() else { return };
        // only top-level selections: a selected folder already covers its children
        let tops: Vec<String> = self
            .selected
            .iter()
            .filter(|p| {
                !self
                    .selected
                    .iter()
                    .any(|q| q.as_str() != p.as_str() && p.starts_with(&format!("{q}/")))
            })
            .cloned()
            .collect();
        if tops.is_empty() {
            return;
        }

        // Only used in file-list mode: where the extracted files will be placed.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dest = temp_dir().join(format!("drag-{nanos}"));
        let planned: Vec<PathBuf> = tops
            .iter()
            .map(|t| dest.join(t.rsplit('/').next().unwrap_or(t)))
            .collect();

        let state = DragState::new(planned);
        let started = self
            .drag
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|h| h.start(state.clone())));
        let Some(uri_mode) = started else {
            self.status = "Dragging files out is only supported on Wayland.".into();
            return;
        };

        if uri_mode {
            // Plain file-list drag (no D-Bus): the files must exist before the target can be
            // answered, so extract them now.
            self.status = format!("Dragging {} item(s) – large files can take a moment.", tops.len());
            let pw = self.password.clone();
            thread::spawn(move || {
                let sel: HashSet<String> = tops.iter().cloned().collect();
                // staging folder: a target never sees a half-written file
                let staging = dest.join(".partial");
                let result = archive::extract(&path, &staging, Some(&sel), &pw, &|_| {}).and_then(|_| {
                    for t in &tops {
                        let name = t.rsplit('/').next().unwrap_or(t);
                        std::fs::rename(staging.join(name), dest.join(name)).map_err(|e| e.to_string())?;
                    }
                    Ok(())
                });
                state.finish(result);
            });
        } else {
            // KDE protocol: nothing is extracted now. When the item is dropped, the file
            // manager tells us the destination folder and we extract straight into it.
            self.status = format!("Dragging {} item(s)…", tops.len());
            self.drag_job = Some((path, tops));
        }
    }

    /// The drop target (KDE file manager) told us where the dragged items should go.
    fn drag_extract_to(&mut self, ctx: &egui::Context, dest: PathBuf) {
        let Some((path, items)) = self.drag_job.take() else { return };
        if self.busy {
            self.status = "Busy – drop the items again in a moment.".into();
            self.drag_job = Some((path, items));
            return;
        }
        let sel: HashSet<String> = items.into_iter().collect();
        let pw = self.password.clone();
        self.run(ctx, "Extracting…", move |p| archive::extract(&path, &dest, Some(&sel), &pw, p));
    }

    /// Selected items without those already covered by a selected parent folder.
    fn top_level_selection(&self) -> Vec<String> {
        self.selected
            .iter()
            .filter(|p| {
                !self
                    .selected
                    .iter()
                    .any(|q| q.as_str() != p.as_str() && p.starts_with(&format!("{q}/")))
            })
            .cloned()
            .collect()
    }

    /// Add / remove / rename / paste are possible for this archive?
    fn can_modify(&self) -> bool {
        match (&self.path, self.format) {
            (Some(p), Some(f)) => archive::can_modify(p, f),
            _ => false,
        }
    }

    fn begin_rename(&mut self) {
        if self.selected.len() != 1 {
            return;
        }
        if let Some(full) = self.selected.iter().next().cloned() {
            let name = full.rsplit('/').next().unwrap_or(&full).to_string();
            self.rename = Some((full, name));
        }
    }

    fn do_rename(&mut self, ctx: &egui::Context, full: String, new_name: String) {
        let Some(path) = self.path.clone() else { return };
        let pw = self.password.clone();
        self.pending_open = Some(path.clone());
        self.run(ctx, "Renaming…", move |p| archive::rename_item(&path, &full, &new_name, &pw, p));
    }

    /// Cut / copy the selection inside the archive (pasted with `paste`).
    fn clip_set(&mut self, ctx: &egui::Context, cut: bool) {
        let Some(path) = self.path.clone() else { return };
        let items = self.top_level_selection();
        if items.is_empty() {
            return;
        }
        // also put the names on the system clipboard so Ctrl+V reaches us
        ctx.copy_text(items.join("\n"));
        self.status = format!("{} {} item(s)", if cut { "Cut" } else { "Copied" }, items.len());
        self.clipboard = Some((path, items, cut));
    }

    /// Folder that a paste goes into: the selected folder, or the folder of the selected file.
    fn paste_target(&self) -> String {
        if self.selected.len() == 1 {
            if let Some(full) = self.selected.iter().next() {
                let is_dir = self.tree.nodes.iter().any(|n| n.is_dir && &n.full == full);
                if is_dir {
                    return full.clone();
                }
                return full.rfind('/').map(|i| full[..i].to_string()).unwrap_or_default();
            }
        }
        String::new()
    }

    fn paste(&mut self, ctx: &egui::Context) {
        let Some(path) = self.path.clone() else { return };
        let Some((clip_path, items, cut)) = self.clipboard.clone() else { return };
        if clip_path != path {
            self.status = "Paste only works inside the archive the items were copied from.".into();
            return;
        }
        let dest = self.paste_target();
        let pw = self.password.clone();
        if cut {
            self.clipboard = None;
        }
        self.pending_open = Some(path.clone());
        self.run(ctx, "Pasting…", move |p| archive::paste_items(&path, &items, &dest, cut, &pw, p));
    }

    fn menu_action(&mut self, ctx: &egui::Context, act: MenuAction) {
        match act {
            MenuAction::Preview => self.preview_selected(ctx),
            MenuAction::OpenExternal => self.open_external(ctx, false),
            MenuAction::OpenWith => self.open_external(ctx, true),
            MenuAction::Rename => self.begin_rename(),
            MenuAction::Remove => self.confirm_remove = Some(self.top_level_selection()),
            MenuAction::ExtractSelected => self.extract(ctx, true),
            MenuAction::ExtractAll => self.extract(ctx, false),
            MenuAction::Cut => self.clip_set(ctx, true),
            MenuAction::Copy => self.clip_set(ctx, false),
            MenuAction::Paste => self.paste(ctx),
            MenuAction::AddFiles => self.add_files(ctx, false),
        }
    }

    fn remove_selected(&mut self, ctx: &egui::Context, items: Vec<String>) {
        let Some(path) = self.path.clone() else { return };
        if items.is_empty() {
            return;
        }
        let sel: HashSet<String> = items.into_iter().collect();
        let pw = self.password.clone();
        self.pending_open = Some(path.clone());
        self.run(ctx, "Removing…", move |p| archive::remove_items(&path, &sel, &pw, p));
    }

    /// Add files (or folders) to the open archive; goes into the selected folder if exactly
    /// one folder is selected, otherwise to the top level.
    fn add_files(&mut self, ctx: &egui::Context, folders: bool) {
        let Some(path) = self.path.clone() else { return };
        if !self.can_modify() {
            return;
        }
        let dialog = rfd::FileDialog::new().set_title("Add to archive");
        let sources: Vec<PathBuf> = if folders {
            dialog.pick_folders().unwrap_or_default()
        } else {
            dialog.pick_files().unwrap_or_default()
        };
        if sources.is_empty() {
            return;
        }
        let into = if self.selected.len() == 1 {
            self.tree
                .nodes
                .iter()
                .find(|n| n.is_dir && self.selected.contains(&n.full))
                .map(|n| n.full.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let pw = self.password.clone();
        self.pending_open = Some(path.clone());
        self.run(ctx, "Adding files…", move |p| archive::add_files(&path, &sources, &into, &pw, p));
    }

    fn create_archive(&mut self, ctx: &egui::Context, folders: bool) {
        let dialog = rfd::FileDialog::new().set_title("Choose what to add");
        let sources: Vec<PathBuf> = if folders {
            dialog.pick_folders().unwrap_or_default()
        } else {
            dialog.pick_files().unwrap_or_default()
        };
        if sources.is_empty() {
            return;
        }
        let Some(dest) = rfd::FileDialog::new()
            .set_title("Save new archive as…")
            .add_filter("7-Zip", &["7z"])
            .add_filter("ZIP", &["zip"])
            .add_filter("TAR.GZ", &["tar.gz", "tgz"])
            .add_filter("TAR.BZ2", &["tar.bz2"])
            .add_filter("TAR.XZ", &["tar.xz"])
            .add_filter("TAR.ZST", &["tar.zst"])
            .add_filter("TAR", &["tar"])
            .set_file_name("archive.7z")
            .save_file()
        else {
            return;
        };
        // The extension you type decides the format (.7z, .zip, .tar.gz, …); default is .7z.
        let dest = archive::normalize_dest(dest);
        self.pending_open = Some(dest.clone());
        self.run(ctx, "Compressing…", move |p| archive::create(&sources, &dest, p));
    }
}

// ───────────────────────────── widgets ────────────────────────────────

/// Text shown in optional column `k` for a node.
fn cell_text(n: &Node, k: usize) -> Option<String> {
    match k {
        0 => Some(human(n.size)),
        1 => n.packed.map(human),
        2 => n.mode.clone(),
        3 => n.owner.clone(),
        4 => n.group.clone(),
        5 => match n.packed {
            Some(p) if n.size > 0 && !n.is_dir => {
                let saved = (1.0 - p as f64 / n.size as f64) * 100.0;
                Some(format!("{:.0} %", saved.max(0.0)))
            }
            _ => None,
        },
        6 => n.crc.map(|c| format!("{c:08X}")),
        7 => n.method.clone(),
        8 => n.version.clone(),
        _ => n.date.clone(),
    }
}

/// Find the Noto Sans regular font: bundled in the AppImage, via fontconfig, or common paths.
fn find_noto_sans() -> Option<Vec<u8>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("APPDIR") {
        paths.push(PathBuf::from(dir).join("usr/share/fonts/NotoSans-Regular.ttf"));
    }
    if let Ok(out) = std::process::Command::new("fc-match")
        .args(["-f", "%{family}|%{file}", "Noto Sans"])
        .output()
    {
        let s = String::from_utf8_lossy(&out.stdout).to_string();
        if let Some((fam, file)) = s.split_once('|') {
            // fc-match falls back to other fonts, so only accept an exact "Noto Sans"
            if fam.split(',').next().map(str::trim) == Some("Noto Sans") {
                paths.push(PathBuf::from(file.trim()));
            }
        }
    }
    for p in [
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/opentype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/TTF/NotoSans-Regular.ttf",
    ] {
        paths.push(PathBuf::from(p));
    }
    paths.into_iter().find_map(|p| std::fs::read(p).ok())
}

fn install_font(ctx: &egui::Context, bytes: Vec<u8>) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("NotoSans".to_owned(), egui::FontData::from_owned(bytes));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "NotoSans".to_owned()); // egui's built-in fonts stay as fallback
    ctx.set_fonts(fonts);
}

fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let (mut v, mut i) = (n as f64, 0);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Ark-style toolbar button: icon + text, flat until hovered.
fn tool_button(ui: &mut egui::Ui, icon: Icon, label: &str, enabled: bool) -> egui::Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let color = if enabled { ui.visuals().text_color() } else { Color32::from_rgb(127, 140, 141) };
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, color);
    let (pad, isz, gap) = (8.0, 18.0, 6.0);
    let size = Vec2::new(pad + isz + gap + galley.size().x + pad, 30.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if enabled && resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + pad + isz / 2.0, rect.center().y),
        Vec2::splat(isz),
    );
    icons::draw(ui.painter(), icon_rect, icon, color);
    ui.painter().galley(
        Pos2::new(icon_rect.right() + gap, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    resp
}

fn type_color(_icon: Icon, visuals: &egui::Visuals) -> Color32 {
    visuals.text_color()
}

/// Breeze Dark look (as used by KDE Ark).
fn ark_visuals() -> egui::Visuals {
    use egui::{Rounding, Stroke};
    let mut v = egui::Visuals::dark();
    let text = Color32::from_rgb(239, 240, 241);
    let panel = Color32::from_rgb(42, 46, 50);
    let base = Color32::from_rgb(35, 38, 41);
    let button = Color32::from_rgb(49, 54, 59);
    let hover = Color32::from_rgb(61, 67, 73);
    let line = Color32::from_rgb(62, 68, 74);
    let accent = Color32::from_rgb(61, 174, 233);

    v.panel_fill = panel;
    v.window_fill = panel;
    v.extreme_bg_color = base;
    v.faint_bg_color = Color32::from_rgb(26, 29, 31);
    v.window_stroke = Stroke::new(1.0_f32, line);
    // Shadow for dialogs and right-click menus: centred behind the window (no offset).
    let shadow = egui::epaint::Shadow {
        offset: Vec2::ZERO,
        blur: 16.0,
        spread: 0.0,
        color: Color32::from_black_alpha(95),
    };
    v.window_shadow = shadow;
    v.popup_shadow = shadow;
    v.hyperlink_color = accent;
    v.selection.bg_fill = accent;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);

    let r = Rounding::same(4.0);
    v.widgets.noninteractive.bg_fill = panel;
    v.widgets.noninteractive.weak_bg_fill = panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, line);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, text);

    for w in [&mut v.widgets.inactive, &mut v.widgets.open] {
        w.bg_fill = button;
        w.weak_bg_fill = button;
        w.bg_stroke = Stroke::new(1.0_f32, line);
        w.fg_stroke = Stroke::new(1.0_f32, text);
        w.rounding = r;
    }
    v.widgets.hovered.bg_fill = hover;
    v.widgets.hovered.weak_bg_fill = hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, accent);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.widgets.hovered.rounding = r;
    v.widgets.active.bg_fill = button;
    v.widgets.active.weak_bg_fill = button;
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, accent);
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.widgets.active.rounding = r;
    v
}

// ───────────────────────────── main loop ──────────────────────────────

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.styled {
            ctx.set_visuals(ark_visuals());
            if let Some(bytes) = find_noto_sans() {
                install_font(ctx, bytes);
            }
            self.styled = true;
        }
        if let Some(p) = self.startup.take() {
            self.open(ctx, PathBuf::from(p));
        }
        self.pump(ctx);

        // Drag & drop: open the first dropped file (X11 via egui, Wayland via wayland_dnd.rs).
        let mut dropped = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()));
        while let Ok(p) = self.dnd_rx.try_recv() {
            dropped = Some(p);
        }
        while let Ok(dest) = self.extract_rx.try_recv() {
            self.drag_extract_to(ctx, dest);
        }
        if let Some(p) = dropped {
            self.open(ctx, p);
        }

        // Keyboard shortcuts
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
            self.show_find = !self.show_find;
            self.focus_find = self.show_find;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O)) {
            self.pick_and_open(ctx);
        }

        // Window title like Ark: "<file> — RustArc"
        let title = match &self.path {
            Some(p) => format!(
                "{} — RustArc",
                p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            ),
            None => "RustArc".to_string(),
        };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        let has = self.path.is_some() && !self.busy;
        let has_file_sel = has && !self.selected_files().is_empty();
        let can_add = has && self.can_modify();

        // Shortcuts shown in the context menu
        if !ctx.wants_keyboard_input() && has {
            let has_items = !self.selected.is_empty();
            if has_file_sel && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::P)) {
                self.preview_selected(ctx);
            }
            if has_items && can_add && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F2)) {
                self.begin_rename();
            }
            if has_items && can_add && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Delete)) {
                self.confirm_remove = Some(self.top_level_selection());
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::E)) {
                self.extract(ctx, has_items);
            }
            if can_add && ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::A)) {
                self.add_files(ctx, false);
            }
            let (ev_copy, ev_cut, ev_paste) = ctx.input(|i| {
                (
                    i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                    i.events.iter().any(|e| matches!(e, egui::Event::Cut)),
                    i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))),
                )
            });
            if can_add && has_items && ev_cut {
                self.clip_set(ctx, true);
            } else if can_add && has_items && ev_copy {
                self.clip_set(ctx, false);
            }
            if can_add && ev_paste {
                self.paste(ctx);
            }
        }

        // ── toolbar ──
        // Toolbar colour follows window focus: #292C30 when active, #202326 when inactive.
        let window_focused = ctx.input(|i| i.raw.focused);
        let toolbar_bg = if window_focused {
            Color32::from_rgb(0x29, 0x2C, 0x30)
        } else {
            Color32::from_rgb(0x20, 0x23, 0x26)
        };
        egui::TopBottomPanel::top("toolbar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .fill(toolbar_bg)
                    .inner_margin(egui::Margin::symmetric(6.0, 4.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Extract (+ dropdown)
                    if tool_button(ui, Icon::Extract, "Extract", has).clicked() {
                        let only_selected = !self.selected.is_empty();
                        self.extract(ctx, only_selected);
                    }
                    ui.add_enabled_ui(has, |ui| {
                        let r = ui.menu_button("  ", |ui| {
                            if ui.button("Extract all…").clicked() {
                                ui.close_menu();
                                self.extract(ctx, false);
                            }
                            if ui
                                .add_enabled(!self.selected.is_empty(), egui::Button::new("Extract selected…"))
                                .clicked()
                            {
                                ui.close_menu();
                                self.extract(ctx, true);
                            }
                        });
                        icons::draw(
                            ui.painter(),
                            Rect::from_center_size(r.response.rect.center(), Vec2::splat(12.0)),
                            Icon::ChevronDown,
                            ui.visuals().text_color(),
                        );
                    });

                    let r = tool_button(ui, Icon::Add, "Add Files", can_add);
                    if r.clicked() {
                        self.add_files(ctx, false);
                    }
                    if !can_add {
                        r.on_hover_text("Adding files is not supported for this archive type (e.g. RAR)");
                    }

                    if tool_button(ui, Icon::Find, "Find", true).clicked() {
                        self.show_find = !self.show_find;
                        self.focus_find = self.show_find;
                    }

                    ui.separator();

                    if tool_button(ui, Icon::Preview, "Preview", has_file_sel).clicked() {
                        self.preview_selected(ctx);
                    }
                    if tool_button(ui, Icon::Open, "Open in External Application", has_file_sel).clicked() {
                        self.open_external(ctx, false);
                    }
                    let can_remove = can_add && !self.selected.is_empty();
                    let r = tool_button(ui, Icon::Remove, "Remove from Archive", can_remove);
                    if r.clicked() {
                        self.confirm_remove = Some(self.top_level_selection());
                    }
                    if !can_remove {
                        r.on_hover_text(if can_add {
                            "Select the items to remove first"
                        } else {
                            "Removing is not supported for this archive type (e.g. RAR)"
                        });
                    }

                    // Hamburger menu on the right
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let r = ui.menu_button("      ", |ui| {
                            if ui.button("Open archive…").clicked() {
                                ui.close_menu();
                                self.pick_and_open(ctx);
                            }
                            if ui.add_enabled(!self.busy, egui::Button::new("New archive from files…")).clicked() {
                                ui.close_menu();
                                self.create_archive(ctx, false);
                            }
                            if ui.add_enabled(!self.busy, egui::Button::new("New archive from folders…")).clicked() {
                                ui.close_menu();
                                self.create_archive(ctx, true);
                            }
                            ui.separator();
                            if ui.add_enabled(can_add, egui::Button::new("Add files…")).clicked() {
                                ui.close_menu();
                                self.add_files(ctx, false);
                            }
                            if ui.add_enabled(can_add, egui::Button::new("Add folder…")).clicked() {
                                ui.close_menu();
                                self.add_files(ctx, true);
                            }
                            if ui
                                .add_enabled(can_add && !self.selected.is_empty(), egui::Button::new("Remove from archive…"))
                                .clicked()
                            {
                                ui.close_menu();
                                self.confirm_remove = Some(self.top_level_selection());
                            }
                            ui.separator();
                            if ui.add_enabled(has, egui::Button::new("Extract all…")).clicked() {
                                ui.close_menu();
                                self.extract(ctx, false);
                            }
                            if ui
                                .add_enabled(has && !self.selected.is_empty(), egui::Button::new("Extract selected…"))
                                .clicked()
                            {
                                ui.close_menu();
                                self.extract(ctx, true);
                            }
                            if ui.add_enabled(has, egui::Button::new("Test archive")).clicked() {
                                ui.close_menu();
                                self.test(ctx);
                            }
                            ui.separator();
                            if ui.button("Set password…").clicked() {
                                ui.close_menu();
                                self.show_password = true;
                            }
                            if ui.add_enabled(has, egui::Button::new("Reload")).clicked() {
                                ui.close_menu();
                                if let Some(p) = self.path.clone() {
                                    self.open(ctx, p);
                                }
                            }
                            ui.separator();
                            if ui.button("Quit").clicked() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        });
                        icons::draw(
                            ui.painter(),
                            Rect::from_center_size(r.response.rect.center(), Vec2::splat(16.0)),
                            Icon::Menu,
                            ui.visuals().text_color(),
                        );
                    });
                });
            });

        // ── find bar ──
        if self.show_find {
            egui::TopBottomPanel::top("findbar").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    icons::draw(
                        ui.painter(),
                        Rect::from_center_size(
                            ui.cursor().left_center() + Vec2::new(9.0, 0.0),
                            Vec2::splat(16.0),
                        ),
                        Icon::Find,
                        ui.visuals().weak_text_color(),
                    );
                    ui.add_space(22.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("Find…")
                            .desired_width(260.0),
                    );
                    if self.focus_find {
                        r.request_focus();
                        self.focus_find = false;
                    }
                    if ui.button("Close").clicked() {
                        self.show_find = false;
                        self.filter.clear();
                    }
                });
            });
        } else if !self.filter.is_empty() {
            self.filter.clear();
        }

        // ── status bar ──
        let (mut folders, mut files, mut total) = (0usize, 0usize, 0u64);
        for n in &self.tree.nodes {
            if n.is_dir {
                folders += 1;
            } else {
                files += 1;
                total += n.size;
            }
        }
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.busy {
                    ui.spinner();
                }
                ui.add(egui::Label::new(self.status.as_str()).selectable(false));
                if self.path.is_some() {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let sel = if self.selected.is_empty() {
                            String::new()
                        } else {
                            format!("{} selected   ", self.selected.len())
                        };
                        ui.add(
                            egui::Label::new(format!(
                                "{sel}{folders} folder(s), {files} file(s), {}",
                                human(total)
                            ))
                            .selectable(false),
                        );
                    });
                }
            });
        });

        // ── file tree ──
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::from_rgb(20, 22, 24)))
            .show(ctx, |ui| {
            if self.path.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.label("Drop an archive here (7z, RAR, ZIP, TAR, GZ, …), or open one from the menu.");
                });
                return;
            }

            let needle = self.filter.to_lowercase();
            let filtering = !needle.is_empty();
            let rows = visible_rows(&self.tree, &self.expanded, &needle);

            let multi = ui.input(|i| i.modifiers.command);
            let range = ui.input(|i| i.modifiers.shift);
            let weak = ui.visuals().weak_text_color();
            let visuals = ui.visuals().clone();

            let mut toggle: Option<String> = None;
            let mut clicked: Option<usize> = None;
            let mut dbl: Option<usize> = None;
            let mut drag_start: Option<usize> = None;
            let mut right_clicked: Option<usize> = None;
            let mut menu_action: Option<MenuAction> = None;
            // facts the context menu needs (the table closure only borrows `self` immutably)
            let m_file_sel = has_file_sel;
            let m_can_modify = can_add;
            let m_single = self.selected.len() == 1;
            let m_has_sel = !self.selected.is_empty();
            let m_has_clip = self
                .clipboard
                .as_ref()
                .map(|c| self.path.as_ref() == Some(&c.0))
                .unwrap_or(false);

            // Optional columns; right-click a header to show / hide them.
            let vis = self.show_cols;
            let mut new_cols = vis;

            // Optional columns fit their content; Name takes all remaining width so the table
            // always spans the window. If even that is too narrow, the table scrolls sideways.
            let avail_w = ui.available_width();
            let font = egui::TextStyle::Body.resolve(ui.style());
            let ink = ui.visuals().text_color();
            let measure = |ui: &egui::Ui, s: &str| -> f32 {
                ui.painter().layout_no_wrap(s.to_string(), font.clone(), ink).size().x
            };
            // egui's automatic spacing between columns is turned off inside the table, so the
            // widths below are the whole story: every column gets 8 px of padding on its left
            // (like Name) and 12 px on its right (8 px for the last column).
            let last_col = (0..NCOLS).rev().find(|&k| vis[k]);
            let mut widths = [0.0f32; NCOLS];
            let mut used = 0.0f32;
            for k in 0..NCOLS {
                if vis[k] {
                    let mut w = measure(ui, COL_NAMES[k]);
                    for s in &self.col_samples[k] {
                        w = w.max(measure(ui, s));
                    }
                    widths[k] = (w + 8.0 + if Some(k) == last_col { 8.0 } else { 12.0 }).ceil();
                    used += widths[k];
                }
            }
            const NAME_MIN: f32 = 120.0;
            // Whole pixels, rounded down: the table never exceeds the window, and there is no
            // visible gap on the right.
            let name_w = (avail_w - used).floor().max(NAME_MIN);
            // Only allow horizontal scrolling when the columns genuinely cannot fit.
            let needs_h_scroll = used + NAME_MIN > avail_w + 0.5;

            let row_w = name_w + used;

            ui.push_id(self.table_gen, |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.visuals_mut().widgets.hovered.bg_fill = Color32::TRANSPARENT;
            ui.visuals_mut().widgets.hovered.weak_bg_fill = Color32::TRANSPARENT;
            egui::ScrollArea::new([needs_h_scroll, false]).auto_shrink([false, false]).show(ui, |ui| {
            // Slot for the row backgrounds, reserved *before* any table content so that whatever
            // is drawn into it ends up underneath all cell text.
            let bg_idx = ui.painter().add(egui::Shape::Noop);
            let mut row_infos: Vec<(Rect, bool, bool, usize)> = Vec::new(); // rect, odd, selected, depth
            let mut header_bottom = f32::NEG_INFINITY;
            let mut table = TableBuilder::new(ui)
                .striped(false)
                .sense(Sense::click_and_drag())
                .column(Column::exact(name_w).clip(true));
            let mut headers: Vec<&str> = vec!["Name"];
            for k in 0..NCOLS {
                if vis[k] {
                    table = table.column(Column::exact(widths[k]));
                    headers.push(COL_NAMES[k]);
                }
            }

            table
                .header(26.0, |mut h| {
                    for (i, t) in headers.iter().enumerate() {
                        let (hrect, _) = h.col(|ui| {
                            let resp = ui.interact(ui.max_rect(), ui.id().with(("col_header", i)), Sense::click());
                            resp.context_menu(|ui| {
                                for k in 0..NCOLS {
                                    ui.checkbox(&mut new_cols[k], COL_NAMES[k]);
                                }
                            });
                            ui.horizontal_centered(|ui| {
                                ui.add_space(8.0);
                                ui.add(egui::Label::new(*t).selectable(false));
                            });
                            let rect = ui.max_rect();
                            let stroke = egui::Stroke::new(1.0_f32, Color32::from_rgb(62, 68, 74));
                            let p = ui.painter();
                            p.hline(rect.x_range(), rect.bottom() - 0.5, stroke);
                            if i + 1 < headers.len() {
                                p.vline(rect.right() - 0.5, rect.y_range(), stroke);
                            }
                        });
                        header_bottom = header_bottom.max(hrect.bottom());
                    }
                })
                .body(|body| {
                    body.rows(29.0, rows.len(), |mut row| {
                        let (ni, depth) = rows[row.index()];
                        let n = &self.tree.nodes[ni];
                        let is_selected = self.selected.contains(&n.full);
                        let odd_row = row.index() % 2 == 1;

                        row.col(|ui| {
                            let cell = ui.max_rect();
                            row_infos.push((
                                Rect::from_min_size(cell.min, Vec2::new(row_w, cell.height())),
                                odd_row,
                                is_selected,
                                depth,
                            ));
                            ui.horizontal_centered(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.add_space(8.0 + depth as f32 * 18.0);
                                if n.is_dir {
                                    let open = filtering || self.expanded.contains(&n.full);
                                    let (r, resp) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::click());
                                    icons::draw(
                                        ui.painter(),
                                        r.shrink(2.0),
                                        if open { Icon::ChevronDown } else { Icon::ChevronRight },
                                        weak,
                                    );
                                    if resp.clicked() {
                                        toggle = Some(n.full.clone());
                                    }
                                } else {
                                    ui.add_space(16.0);
                                }
                                let icon = if n.is_dir { Icon::Folder } else { icons::for_file(&n.name) };
                                let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                                icons::draw(ui.painter(), r, icon, type_color(icon, &visuals));
                                ui.add(egui::Label::new(n.name.as_str()).selectable(false));
                            });
                        });

                        for k in 0..NCOLS {
                            if !vis[k] {
                                continue;
                            }
                            row.col(|ui| {
                                ui.horizontal_centered(|ui| {
                                    ui.add_space(8.0);
                                    let text = cell_text(n, k);
                                    if let Some(t) = text {
                                        ui.add(egui::Label::new(t).selectable(false));
                                    }
                                });
                            });
                        }

                        let resp = row.response();
                        if resp.secondary_clicked() {
                            right_clicked = Some(ni);
                        }
                        if !n.is_dir {
                            resp.clone().context_menu(|ui| {
                                // plain helper: returns true when clicked (so it holds no borrow of `menu_action`)
                                let item = |ui: &mut egui::Ui, label: &str, shortcut: &str, enabled: bool| -> bool {
                                    ui.add_enabled(enabled, egui::Button::new(label).shortcut_text(shortcut))
                                        .clicked()
                                };
                                if item(ui, "Preview", "Ctrl+P", m_file_sel) {
                                    menu_action = Some(MenuAction::Preview);
                                    ui.close_menu();
                                }
                                if item(ui, "Open in External Application", "", m_file_sel) {
                                    menu_action = Some(MenuAction::OpenExternal);
                                    ui.close_menu();
                                }
                                if item(ui, "Open With…", "", m_file_sel) {
                                    menu_action = Some(MenuAction::OpenWith);
                                    ui.close_menu();
                                }
                                ui.separator();
                                if item(ui, "Rename…", "F2", m_can_modify && m_single) {
                                    menu_action = Some(MenuAction::Rename);
                                    ui.close_menu();
                                }
                                if item(ui, "Remove from Archive", "Del", m_can_modify && m_has_sel) {
                                    menu_action = Some(MenuAction::Remove);
                                    ui.close_menu();
                                }
                                ui.menu_button("Extract", |ui| {
                                    if item(ui, "Extract selected…", "Ctrl+E", m_has_sel) {
                                        menu_action = Some(MenuAction::ExtractSelected);
                                        ui.close_menu();
                                    }
                                    if item(ui, "Extract all…", "", true) {
                                        menu_action = Some(MenuAction::ExtractAll);
                                        ui.close_menu();
                                    }
                                });
                                ui.separator();
                                if item(ui, "Cut", "Ctrl+X", m_can_modify && m_has_sel) {
                                    menu_action = Some(MenuAction::Cut);
                                    ui.close_menu();
                                }
                                if item(ui, "Copy", "Ctrl+C", m_can_modify && m_has_sel) {
                                    menu_action = Some(MenuAction::Copy);
                                    ui.close_menu();
                                }
                                if item(ui, "Paste", "Ctrl+V", m_can_modify && m_has_clip) {
                                    menu_action = Some(MenuAction::Paste);
                                    ui.close_menu();
                                }
                                ui.separator();
                                if item(ui, "Add Files…", "Alt+A", m_can_modify) {
                                    menu_action = Some(MenuAction::AddFiles);
                                    ui.close_menu();
                                }
                            });
                        }
                        if resp.drag_started() {
                            drag_start = Some(ni);
                        } else if resp.double_clicked() {
                            dbl = Some(ni);
                        } else if resp.clicked() {
                            clicked = Some(ni);
                        }
                    });
                });
            // Row backgrounds (stripes, hover, selection) span every column and sit underneath
            // the cell content, in the slot reserved above.
            {
                let view = ui.clip_rect();
                let body_clip = Rect::from_min_max(
                    Pos2::new(view.left(), header_bottom.max(view.top())),
                    view.max,
                );
                let pointer = ui
                    .input(|i| i.pointer.hover_pos())
                    .filter(|pos| ui.ctx().layer_id_at(*pos) == Some(ui.layer_id()));
                let accent = visuals.selection.bg_fill;
                let mut shapes: Vec<egui::Shape> = Vec::new();
                for (r, odd, sel, depth) in &row_infos {
                    let vis_rect = r.intersect(body_clip);
                    if vis_rect.height() <= 0.0 {
                        continue;
                    }
                    if *odd {
                        shapes.push(egui::Shape::rect_filled(vis_rect, 0.0, visuals.faint_bg_color));
                    }
                    // like Ark: rounded, inset, thin outline, starting at the icon (the
                    // expander arrow stays outside the highlight)
                    let left = r.left() + 21.0 + *depth as f32 * 18.0;
                    let right = r.right() - 4.0;
                    if right <= left {
                        continue;
                    }
                    let hl = Rect::from_min_max(
                        Pos2::new(left, r.top() + 1.0),
                        Pos2::new(right, r.bottom() - 1.0),
                    )
                    .intersect(body_clip);
                    if hl.height() <= 0.0 {
                        continue;
                    }
                    if *sel {
                        shapes.push(egui::Shape::rect_filled(hl, 4.0, accent));
                        shapes.push(egui::Shape::rect_stroke(
                            hl,
                            4.0,
                            egui::Stroke::new(1.0_f32, Color32::from_rgb(110, 200, 245)),
                        ));
                    } else if pointer.map_or(false, |p| vis_rect.contains(p)) {
                        shapes.push(egui::Shape::rect_filled(hl, 4.0, accent.gamma_multiply(0.22)));
                        shapes.push(egui::Shape::rect_stroke(
                            hl,
                            4.0,
                            egui::Stroke::new(1.0_f32, accent.gamma_multiply(0.5)),
                        ));
                    }
                }
                ui.painter().set(bg_idx, egui::Shape::Vec(shapes));
            }
            });
            });
            if new_cols != self.show_cols {
                self.table_gen += 1;
                self.show_cols = new_cols;
                self.col_prefs.insert(self.cols_key.clone(), new_cols);
            }

            if let Some(full) = toggle {
                if !self.expanded.remove(&full) {
                    self.expanded.insert(full);
                }
            }

            if let Some(idx) = clicked {
                let full = self.tree.nodes[idx].full.clone();
                let mut ranged = false;
                if range {
                    if let Some(last) = self.last_clicked.clone() {
                        let a = rows.iter().position(|(n, _)| self.tree.nodes[*n].full == last);
                        let b = rows.iter().position(|(n, _)| *n == idx);
                        if let (Some(a), Some(b)) = (a, b) {
                            if !multi {
                                self.selected.clear();
                            }
                            for r in a.min(b)..=a.max(b) {
                                self.selected.insert(self.tree.nodes[rows[r].0].full.clone());
                            }
                            ranged = true;
                        }
                    }
                }
                if !ranged {
                    if multi {
                        if !self.selected.remove(&full) {
                            self.selected.insert(full.clone());
                        }
                    } else {
                        self.selected.clear();
                        self.selected.insert(full.clone());
                    }
                    self.last_clicked = Some(full);
                }
            }

            if let Some(idx) = right_clicked {
                let full = self.tree.nodes[idx].full.clone();
                if !self.selected.contains(&full) {
                    self.selected.clear();
                    self.selected.insert(full.clone());
                    self.last_clicked = Some(full);
                }
            }
            if let Some(act) = menu_action {
                self.menu_action(ctx, act);
            }

            if let Some(idx) = drag_start {
                let full = self.tree.nodes[idx].full.clone();
                if !self.selected.contains(&full) {
                    self.selected.clear();
                    self.selected.insert(full.clone());
                    self.last_clicked = Some(full);
                }
                self.begin_drag_out();
            }

            if let Some(idx) = dbl {
                let (full, is_dir) = {
                    let n = &self.tree.nodes[idx];
                    (n.full.clone(), n.is_dir)
                };
                if is_dir {
                    if !self.expanded.remove(&full) {
                        self.expanded.insert(full);
                    }
                } else {
                    self.selected.clear();
                    self.selected.insert(full);
                    self.open_external(ctx, false);
                }
            }
        });

        // ── password dialog ──
        if self.show_password {
            let mut open = true;
            let mut apply = false;
            egui::Window::new("Password")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("Enter the archive password:");
                    let r = ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        apply = true;
                    }
                    if ui.button("OK").clicked() {
                        apply = true;
                    }
                });
            if apply {
                self.show_password = false;
                if let Some(p) = self.pw_retry.take().or_else(|| self.path.clone()) {
                    self.open(ctx, p);
                }
            } else {
                self.show_password = open;
            }
        }

        // ── rename dialog ──
        let mut rename_apply: Option<(String, String)> = None;
        let mut rename_cancel = false;
        if let Some((full, text)) = self.rename.as_mut() {
            egui::Window::new("Rename")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("New name:");
                    let r = ui.add(egui::TextEdit::singleline(&mut *text).desired_width(280.0));
                    r.request_focus();
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Rename").clicked() || enter {
                            rename_apply = Some((full.clone(), text.clone()));
                        }
                        if ui.button("Cancel").clicked() || esc {
                            rename_cancel = true;
                        }
                    });
                });
        }
        if let Some((f, n)) = rename_apply {
            self.rename = None;
            self.do_rename(ctx, f, n);
        } else if rename_cancel {
            self.rename = None;
        }

        // ── confirm removal ──
        let mut do_remove: Option<Vec<String>> = None;
        let mut cancel_remove = false;
        if let Some(items) = &self.confirm_remove {
            egui::Window::new("Remove from archive")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("Remove {} selected item(s) from the archive?", items.len()));
                    ui.label("The archive will be rewritten. This cannot be undone.");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Remove").clicked() {
                            do_remove = Some(items.clone());
                        }
                        if ui.button("Cancel").clicked() {
                            cancel_remove = true;
                        }
                    });
                });
        }
        if let Some(items) = do_remove {
            self.confirm_remove = None;
            self.remove_selected(ctx, items);
        } else if cancel_remove {
            self.confirm_remove = None;
        }

        // ── preview window ──
        let mut close_preview = false;
        if let Some((name, text)) = &self.preview {
            let mut open = true;
            egui::Window::new(format!("Preview – {name}"))
                .open(&mut open)
                .default_size([640.0, 420.0])
                .show(ctx, |ui| {
                    egui::ScrollArea::both().show(ui, |ui| {
                        let mut t = text.as_str();
                        ui.add(
                            egui::TextEdit::multiline(&mut t)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY),
                        );
                    });
                });
            close_preview = !open;
        }
        if close_preview {
            self.preview = None;
        }

        // ── drag-over overlay ──
        if self.dnd_hovering.load(Ordering::Relaxed)
            || ctx.input(|i| !i.raw.hovered_files.is_empty())
        {
            let screen = ctx.screen_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("drop_overlay"),
            ));
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(170));
            painter.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                "Drop archive to open",
                egui::FontId::proportional(28.0),
                Color32::WHITE,
            );
        }
    }
}
