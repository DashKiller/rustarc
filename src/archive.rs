//! Archive back-end.
//!
//! Read (list / extract / test): 7z, RAR, ZIP, TAR, TAR.GZ, TAR.BZ2, TAR.XZ, TAR.ZST,
//! and single-file .gz / .bz2 / .xz / .zst.
//! Create: 7z, ZIP, TAR, TAR.GZ, TAR.BZ2, TAR.XZ, TAR.ZST (RAR creation is proprietary).

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

use sevenz_rust::{Password, SevenZArchiveEntry, SevenZReader, SevenZWriter};

pub type Progress<'a> = &'a dyn Fn(String);

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

// ───────────────────────────── formats ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Comp {
    None,
    Gz,
    Bz2,
    Xz,
    Zst,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    SevenZ,
    Rar,
    Zip,
    Tar(Comp),
    Single(Comp),
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::SevenZ => "7-Zip",
            Format::Rar => "RAR",
            Format::Zip => "ZIP",
            Format::Tar(Comp::None) => "TAR",
            Format::Tar(Comp::Gz) => "TAR.GZ",
            Format::Tar(Comp::Bz2) => "TAR.BZ2",
            Format::Tar(Comp::Xz) => "TAR.XZ",
            Format::Tar(Comp::Zst) => "TAR.ZST",
            Format::Single(Comp::Gz) => "GZ",
            Format::Single(Comp::Bz2) => "BZ2",
            Format::Single(Comp::Xz) => "XZ",
            Format::Single(Comp::Zst) => "ZST",
            Format::Single(Comp::None) => "File",
        }
    }

    /// Detect by magic bytes (so misnamed files still work), then by extension.
    pub fn detect(path: &Path) -> Option<Format> {
        let mut magic = [0u8; 8];
        let n = File::open(path)
            .ok()
            .and_then(|mut f| f.read(&mut magic).ok())
            .unwrap_or(0);
        let m = &magic[..n];

        if m.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
            return Some(Format::SevenZ);
        }
        if m.starts_with(b"Rar!") {
            return Some(Format::Rar);
        }
        if m.starts_with(b"PK\x03\x04") || m.starts_with(b"PK\x05\x06") {
            return Some(Format::Zip);
        }

        let comp = if m.starts_with(&[0x1F, 0x8B]) {
            Comp::Gz
        } else if m.starts_with(b"BZh") {
            Comp::Bz2
        } else if m.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
            Comp::Xz
        } else if m.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
            Comp::Zst
        } else {
            Comp::None
        };

        if let Ok(mut r) = open_reader(path, comp) {
            if is_tar_stream(&mut r) {
                return Some(Format::Tar(comp));
            }
        }
        if comp != Comp::None {
            return Some(Format::Single(comp));
        }

        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "7z" => Some(Format::SevenZ),
            "rar" => Some(Format::Rar),
            "zip" => Some(Format::Zip),
            "tar" => Some(Format::Tar(Comp::None)),
            _ => None,
        }
    }
}

fn is_tar_stream(r: &mut dyn Read) -> bool {
    let mut buf = Vec::with_capacity(512);
    if r.take(512).read_to_end(&mut buf).is_err() {
        return false;
    }
    buf.len() >= 262 && &buf[257..262] == b"ustar"
}

/// Open a file through the matching decompressor.
fn open_reader(path: &Path, comp: Comp) -> io::Result<Box<dyn Read>> {
    let file = File::open(path)?;
    Ok(match comp {
        Comp::None => Box::new(BufReader::new(file)),
        Comp::Gz => Box::new(flate2::read::MultiGzDecoder::new(BufReader::new(file))),
        Comp::Bz2 => Box::new(bzip2::read::BzDecoder::new(BufReader::new(file))),
        Comp::Xz => Box::new(xz2::read::XzDecoder::new(BufReader::new(file))),
        Comp::Zst => Box::new(zstd::stream::read::Decoder::new(file)?),
    })
}

// ───────────────────────────── entries ─────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub packed: Option<u64>,
    pub crc: Option<u32>,
    pub method: Option<String>,
    pub mode: Option<String>,
    pub owner: Option<String>,
    pub group: Option<String>,
    pub date: Option<String>,
    pub version: Option<String>,
}

fn norm(s: &str) -> String {
    let s = s.replace('\\', "/");
    s.trim_start_matches("./").to_string()
}

fn pw(p: &str) -> Password {
    if p.is_empty() {
        Password::empty()
    } else {
        Password::from(p)
    }
}

/// Does `name` match one of the selected entries (or live inside a selected folder)?
fn wanted(name: &str, sel: Option<&HashSet<String>>) -> bool {
    match sel {
        None => true,
        Some(set) => set.iter().any(|s| {
            let s = s.trim_end_matches('/');
            name == s || name.starts_with(&format!("{s}/"))
        }),
    }
}

/// Where an entry goes on disk. With a selection, each selected item keeps only its own
/// name (plus everything below it when it is a folder), not the folders above it.
fn rel_name(name: &str, sel: Option<&HashSet<String>>) -> String {
    let Some(set) = sel else { return name.to_string() };
    let mut best: Option<&str> = None;
    for s in set {
        let s = s.trim_end_matches('/');
        if (name == s || name.starts_with(&format!("{s}/"))) && best.map_or(true, |b| s.len() < b.len()) {
            best = Some(s);
        }
    }
    match best {
        Some(s) => name[s.rfind('/').map(|i| i + 1).unwrap_or(0)..].to_string(),
        None => name.to_string(),
    }
}

/// Join `name` onto `base`, refusing absolute paths and `..` (zip-slip protection).
fn safe_join(base: &Path, name: &str) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for c in Path::new(name).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Name of the content of a single-file compressed file: "notes.txt.gz" -> "notes.txt".
fn single_name(path: &Path) -> String {
    let fname = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".into());
    let lower = fname.to_lowercase();
    for ext in [".gz", ".bz2", ".xz", ".zst"] {
        if lower.ends_with(ext) && fname.len() > ext.len() {
            return fname[..fname.len() - ext.len()].to_string();
        }
    }
    format!("{fname}.out")
}

// ───────────────────────────── listing ─────────────────────────────

/// "2026-10-30 06:01" in local time.
fn fmt_unix(secs: i64) -> Option<String> {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(secs, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
}

/// MS-DOS packed date/time -> "2026-10-30 06:01".
fn fmt_dos(t: u32) -> Option<String> {
    let (min, hour) = ((t >> 5) & 0x3F, (t >> 11) & 0x1F);
    let (day, mon, year) = ((t >> 16) & 0x1F, (t >> 21) & 0x0F, 1980 + (t >> 25));
    if day == 0 || mon == 0 {
        return None;
    }
    Some(format!("{year:04}-{mon:02}-{day:02} {hour:02}:{min:02}"))
}

/// "-rwxr-xr-x" style string. `kind` is 'd', 'l' or '-'.
fn unix_mode_str(mode: u32, kind: char) -> String {
    let mut s = String::with_capacity(10);
    s.push(kind);
    for (i, c) in "rwxrwxrwx".chars().enumerate() {
        s.push(if mode & (1 << (8 - i)) != 0 { c } else { '-' });
    }
    s
}

fn unix_kind(mode: u32, is_dir: bool) -> char {
    match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        _ if is_dir => 'd',
        _ => '-',
    }
}

/// 7z attributes: DOS letters, plus a unix mode when the unix extension bit is set.
fn mode_7z(a: u32) -> String {
    let mut s: String = [(0x10u32, 'D'), (0x01, 'R'), (0x02, 'H'), (0x04, 'S'), (0x20, 'A')]
        .iter()
        .filter(|(b, _)| a & *b != 0)
        .map(|(_, c)| *c)
        .collect();
    if a & 0x8000 != 0 {
        let m = a >> 16;
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&unix_mode_str(m, unix_kind(m, false)));
    }
    s
}

/// RAR (Windows host) attributes in the same style `unrar l` / Ark print them.
fn mode_rar_win(a: u32) -> String {
    let c = |bit: u32, ch: char| if a & bit != 0 { ch } else { '.' };
    [c(0x2000, 'I'), c(0x800, 'C'), c(0x20, 'A'), c(0x10, 'D'), c(0x4, 'S'), c(0x2, 'H'), c(0x1, 'R')]
        .iter()
        .collect()
}

pub fn list(path: &Path, password: &str) -> Result<(Format, Vec<Entry>), String> {
    let fmt = Format::detect(path).ok_or("Unsupported or unrecognised archive format")?;
    let entries = match fmt {
        Format::SevenZ => list_7z(path, password)?,
        Format::Rar => list_rar(path, password)?,
        Format::Zip => list_zip(path)?,
        Format::Tar(c) => list_tar(path, c)?,
        Format::Single(c) => {
            let mut r = open_reader(path, c).map_err(es)?;
            let size = io::copy(&mut r, &mut io::sink()).map_err(es)?;
            vec![Entry { name: single_name(path), size, ..Default::default() }]
        }
    };
    Ok((fmt, entries))
}

fn list_7z(path: &Path, password: &str) -> Result<Vec<Entry>, String> {
    let reader = SevenZReader::open(path, pw(password)).map_err(es)?;
    Ok(reader
        .archive()
        .files
        .iter()
        .map(|f| {
            let date = if f.has_last_modified_date {
                // FILETIME: 100 ns ticks since 1601-01-01
                fmt_unix((f.last_modified_date.to_raw() / 10_000_000) as i64 - 11_644_473_600)
            } else {
                None
            };
            Entry {
                name: norm(f.name()),
                size: f.size(),
                is_dir: f.is_directory(),
                crc: if f.has_crc && !f.is_directory() { Some(f.crc as u32) } else { None },
                mode: if f.has_windows_attributes { Some(mode_7z(f.windows_attributes)) } else { None },
                date,
                ..Default::default()
            }
        })
        .collect())
}

fn rar_archive<'a>(path: &'a PathBuf, password: &'a str) -> unrar::Archive<'a> {
    if password.is_empty() {
        unrar::Archive::new(path)
    } else {
        unrar::Archive::with_password(path, password)
    }
}

fn list_rar(path: &Path, password: &str) -> Result<Vec<Entry>, String> {
    let p = path.to_path_buf();
    let it = rar_archive(&p, password).open_for_listing().map_err(es)?;
    // The unrar crate drops packed size, version, dictionary and attributes, so we read
    // those from the raw archive headers and merge them in by file name.
    let meta = rar_meta(path);
    let mut out = Vec::new();
    for h in it {
        let h = h.map_err(es)?;
        let is_dir = h.is_directory();
        let name = norm(&h.filename.to_string_lossy());
        let mut e = Entry {
            size: h.unpacked_size as u64,
            is_dir,
            crc: if is_dir { None } else { Some(h.file_crc) },
            method: Some(format!("-m{}", h.method.saturating_sub(0x30))),
            date: fmt_dos(h.file_time),
            ..Default::default()
        };
        if let Some(m) = meta.get(&name) {
            e.packed = if is_dir { None } else { Some(m.packed) };
            e.mode = Some(m.mode.clone());
            e.version = Some(m.version.clone());
            e.method = Some(m.method.clone());
            if m.date.is_some() {
                e.date = m.date.clone();
            }
        }
        e.name = name;
        out.push(e);
    }
    Ok(out)
}

struct RarMeta {
    packed: u64,
    version: String,
    mode: String,
    method: String,
    date: Option<String>,
}

fn dict_str(kb: u64) -> String {
    if kb >= 1024 {
        format!("{}m", kb / 1024)
    } else {
        format!("{kb}k")
    }
}

fn read_vint<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        v |= ((b[0] & 0x7F) as u64) << (7 * i);
        if b[0] & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "bad vint"))
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// Walk the raw RAR block headers (RAR 1.5-4.x and 5.0+) and collect per-file info.
/// Any parse problem (e.g. encrypted headers) just ends the walk with what we have.
fn rar_meta(path: &Path) -> HashMap<String, RarMeta> {
    let mut out = HashMap::new();
    let Ok(mut f) = File::open(path) else { return out };
    let mut sig = [0u8; 8];
    if f.read_exact(&mut sig).is_err() {
        return out;
    }
    if sig.starts_with(b"Rar!\x1A\x07\x01\x00") {
        let _ = rar5_walk(&mut f, &mut out);
    } else if sig.starts_with(b"Rar!\x1A\x07\x00") {
        if f.seek(SeekFrom::Start(7)).is_ok() {
            let _ = rar4_walk(&mut f, &mut out);
        }
    }
    out
}

fn rar5_walk(f: &mut File, out: &mut HashMap<String, RarMeta>) -> io::Result<()> {
    loop {
        let mut crc = [0u8; 4];
        if f.read_exact(&mut crc).is_err() {
            break;
        }
        let hsize = read_vint(f)?;
        if hsize == 0 || hsize > (1 << 22) {
            break;
        }
        let mut hdr = vec![0u8; hsize as usize];
        f.read_exact(&mut hdr)?;
        let mut c = &hdr[..];
        let typ = read_vint(&mut c)?;
        let flags = read_vint(&mut c)?;
        if flags & 1 != 0 {
            read_vint(&mut c)?; // extra area size
        }
        let data_size = if flags & 2 != 0 { read_vint(&mut c)? } else { 0 };

        match typ {
            2 => {
                let fflags = read_vint(&mut c)?;
                let _unp = read_vint(&mut c)?;
                let attr = read_vint(&mut c)?;
                let mtime = if fflags & 2 != 0 { Some(read_u32(&mut c)?) } else { None };
                if fflags & 4 != 0 {
                    read_u32(&mut c)?;
                }
                let info = read_vint(&mut c)?;
                let host = read_vint(&mut c)?;
                let nlen = read_vint(&mut c)? as usize;
                if nlen <= c.len() {
                    let name = norm(&String::from_utf8_lossy(&c[..nlen]));
                    let is_dir = fflags & 1 != 0;
                    let ver = info & 0x3F;
                    let version = match ver {
                        0 => "RAR 5.0(v50)".to_string(),
                        1 => "RAR 7.0(v70)".to_string(),
                        v => format!("RAR (v{v})"),
                    };
                    let method = if is_dir {
                        "-m0".to_string()
                    } else {
                        format!("-m{} -md={}", (info >> 7) & 7, dict_str(128u64 << ((info >> 10) & 0xF)))
                    };
                    let mode = if host == 1 {
                        unix_mode_str(attr as u32, unix_kind(attr as u32, is_dir))
                    } else {
                        mode_rar_win(attr as u32)
                    };
                    out.insert(
                        name,
                        RarMeta {
                            packed: data_size,
                            version,
                            mode,
                            method,
                            date: mtime.and_then(|t| fmt_unix(t as i64)),
                        },
                    );
                }
            }
            4 | 5 => break, // encryption header / end of archive
            _ => {}
        }
        f.seek(SeekFrom::Current(data_size as i64))?;
    }
    Ok(())
}

fn rar4_walk(f: &mut File, out: &mut HashMap<String, RarMeta>) -> io::Result<()> {
    loop {
        let mut base = [0u8; 7];
        if f.read_exact(&mut base).is_err() {
            break;
        }
        let typ = base[2];
        let flags = u16::from_le_bytes([base[3], base[4]]);
        let head_size = u16::from_le_bytes([base[5], base[6]]) as u64;
        if head_size < 7 || typ == 0x7B || (typ == 0x73 && flags & 0x0080 != 0) {
            break; // end block / encrypted headers
        }
        let mut rest = vec![0u8; (head_size - 7) as usize];
        f.read_exact(&mut rest)?;

        let mut add_size = 0u64;
        if typ == 0x74 && rest.len() >= 25 {
            let u32at = |o: usize| u32::from_le_bytes([rest[o], rest[o + 1], rest[o + 2], rest[o + 3]]);
            let mut packed = u32at(0) as u64;
            let host = rest[8];
            let ftime = u32at(13);
            let ver = rest[17] as u32;
            let meth = rest[18];
            let nsize = u16::from_le_bytes([rest[19], rest[20]]) as usize;
            let attr = u32at(21);
            let mut off = 25;
            if flags & 0x100 != 0 && rest.len() >= 33 {
                packed |= (u32at(25) as u64) << 32;
                off = 33;
            }
            add_size = packed;
            if off + nsize <= rest.len() {
                let raw = &rest[off..off + nsize];
                let raw = raw.split(|&b| b == 0).next().unwrap_or(raw); // drop unicode tail
                let name = norm(&String::from_utf8_lossy(raw));
                let is_dir = flags & 0xE0 == 0xE0;
                let method = if is_dir {
                    "-m0".to_string()
                } else {
                    format!(
                        "-m{} -md={}",
                        (meth as u32).saturating_sub(0x30),
                        dict_str(64u64 << ((flags >> 5) & 7))
                    )
                };
                let mode = if host == 3 {
                    unix_mode_str(attr, unix_kind(attr, is_dir))
                } else {
                    mode_rar_win(attr)
                };
                out.insert(
                    name,
                    RarMeta {
                        packed,
                        version: format!("RAR {}.{}(v{})", ver / 10, ver % 10, ver),
                        mode,
                        method,
                        date: fmt_dos(ftime),
                    },
                );
            }
        } else if flags & 0x8000 != 0 && rest.len() >= 4 {
            add_size = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as u64;
        }
        f.seek(SeekFrom::Current(add_size as i64))?;
    }
    Ok(())
}

fn list_zip(path: &Path) -> Result<Vec<Entry>, String> {
    let mut z = zip::ZipArchive::new(File::open(path).map_err(es)?).map_err(es)?;
    let mut out = Vec::with_capacity(z.len());
    for i in 0..z.len() {
        // by_index_raw does not need the password
        let f = z.by_index_raw(i).map_err(es)?;
        let is_dir = f.is_dir();
        out.push(Entry {
            name: norm(f.name()),
            size: f.size(),
            is_dir,
            packed: if is_dir { None } else { Some(f.compressed_size()) },
            crc: if is_dir { None } else { Some(f.crc32()) },
            method: if is_dir { None } else { Some(format!("{:?}", f.compression())) },
            mode: f.unix_mode().map(|m| unix_mode_str(m, unix_kind(m, is_dir))),
            date: f
                .last_modified()
                .map(|d| format!("{:04}-{:02}-{:02} {:02}:{:02}", d.year(), d.month(), d.day(), d.hour(), d.minute())),
            ..Default::default()
        });
    }
    Ok(out)
}

fn list_tar(path: &Path, comp: Comp) -> Result<Vec<Entry>, String> {
    let mut ar = tar::Archive::new(open_reader(path, comp).map_err(es)?);
    let mut out = Vec::new();
    for e in ar.entries().map_err(es)? {
        let e = e.map_err(es)?;
        let name = norm(&e.path().map_err(es)?.to_string_lossy());
        let h = e.header();
        let ty = h.entry_type();
        let kind = if ty.is_dir() {
            'd'
        } else if ty.is_symlink() {
            'l'
        } else {
            '-'
        };
        out.push(Entry {
            name,
            size: h.size().unwrap_or(0),
            is_dir: ty.is_dir(),
            mode: h.mode().ok().map(|m| unix_mode_str(m, kind)),
            owner: h
                .username()
                .ok()
                .flatten()
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or_else(|| h.uid().ok().map(|u| u.to_string())),
            group: h
                .groupname()
                .ok()
                .flatten()
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or_else(|| h.gid().ok().map(|g| g.to_string())),
            date: h.mtime().ok().and_then(|t| fmt_unix(t as i64)),
            ..Default::default()
        });
    }
    Ok(out)
}

// ──────────────────────────── extraction ───────────────────────────

/// Extract everything (`sel == None`) or only the selected names.
pub fn extract(
    path: &Path,
    dest: &Path,
    sel: Option<&HashSet<String>>,
    password: &str,
    progress: Progress,
) -> Result<String, String> {
    fs::create_dir_all(dest).map_err(es)?;
    let count = match Format::detect(path).ok_or("Unsupported archive format")? {
        Format::SevenZ => extract_7z(path, dest, sel, password, progress)?,
        Format::Rar => extract_rar(path, dest, sel, password, progress)?,
        Format::Zip => extract_zip(path, dest, sel, password, progress)?,
        Format::Tar(c) => extract_tar(path, c, dest, sel, progress)?,
        Format::Single(c) => {
            let name = single_name(path);
            if !wanted(&name, sel) {
                0
            } else {
                progress(format!("Extracting {name}"));
                let target = safe_join(dest, &name).ok_or("Unsafe file name")?;
                let mut r = open_reader(path, c).map_err(es)?;
                let mut out = File::create(target).map_err(es)?;
                io::copy(&mut r, &mut out).map_err(es)?;
                1
            }
        }
    };
    Ok(format!("Extracted {count} item(s) to {}", dest.display()))
}

fn extract_7z(
    path: &Path,
    dest: &Path,
    sel: Option<&HashSet<String>>,
    password: &str,
    progress: Progress,
) -> Result<usize, String> {
    let mut reader = SevenZReader::open(path, pw(password)).map_err(es)?;
    let mut count = 0usize;
    reader
        .for_each_entries(|entry, data| {
            let name = norm(entry.name());
            if !wanted(&name, sel) {
                io::copy(data, &mut io::sink())?; // must drain solid blocks
                return Ok(true);
            }
            let Some(target) = safe_join(dest, &rel_name(&name, sel)) else {
                io::copy(data, &mut io::sink())?;
                return Ok(true); // skip unsafe path
            };
            progress(format!("Extracting {name}"));
            if entry.is_directory() {
                fs::create_dir_all(&target)?;
            } else {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut out = File::create(&target)?;
                io::copy(data, &mut out)?;
            }
            count += 1;
            Ok(true)
        })
        .map_err(es)?;
    Ok(count)
}

fn extract_rar(
    path: &Path,
    dest: &Path,
    sel: Option<&HashSet<String>>,
    password: &str,
    progress: Progress,
) -> Result<usize, String> {
    let p = path.to_path_buf();
    let mut archive = rar_archive(&p, password).open_for_processing().map_err(es)?;
    let mut count = 0usize;
    while let Some(header) = archive.read_header().map_err(es)? {
        let name = norm(&header.entry().filename.to_string_lossy());
        archive = if wanted(&name, sel) {
            progress(format!("Extracting {name}"));
            count += 1;
            if sel.is_some() {
                match safe_join(dest, &rel_name(&name, sel)) {
                    Some(target) if header.entry().is_directory() => {
                        fs::create_dir_all(&target).map_err(es)?;
                        header.skip().map_err(es)?
                    }
                    Some(target) => {
                        if let Some(parent) = target.parent() {
                            fs::create_dir_all(parent).map_err(es)?;
                        }
                        header.extract_to(&target).map_err(es)?
                    }
                    None => header.skip().map_err(es)?,
                }
            } else {
                header.extract_with_base(dest).map_err(es)?
            }
        } else {
            header.skip().map_err(es)?
        };
    }
    Ok(count)
}

fn extract_zip(
    path: &Path,
    dest: &Path,
    sel: Option<&HashSet<String>>,
    password: &str,
    progress: Progress,
) -> Result<usize, String> {
    let mut z = zip::ZipArchive::new(File::open(path).map_err(es)?).map_err(es)?;
    let mut count = 0usize;
    for i in 0..z.len() {
        let res = if password.is_empty() {
            z.by_index(i)
        } else {
            z.by_index_decrypt(i, password.as_bytes())
        };
        let mut file = res.map_err(es)?;
        let name = norm(file.name());
        if !wanted(&name, sel) {
            continue;
        }
        let Some(target) = safe_join(dest, &rel_name(&name, sel)) else { continue };
        progress(format!("Extracting {name}"));
        if file.is_dir() {
            fs::create_dir_all(&target).map_err(es)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(es)?;
            }
            let mut out = File::create(&target).map_err(es)?;
            io::copy(&mut file, &mut out).map_err(es)?;
        }
        count += 1;
    }
    Ok(count)
}

fn extract_tar(
    path: &Path,
    comp: Comp,
    dest: &Path,
    sel: Option<&HashSet<String>>,
    progress: Progress,
) -> Result<usize, String> {
    let mut ar = tar::Archive::new(open_reader(path, comp).map_err(es)?);
    let mut count = 0usize;
    for e in ar.entries().map_err(es)? {
        let mut e = e.map_err(es)?;
        let name = norm(&e.path().map_err(es)?.to_string_lossy());
        if !wanted(&name, sel) {
            continue;
        }
        progress(format!("Extracting {name}"));
        if sel.is_some() {
            let Some(target) = safe_join(dest, &rel_name(&name, sel)) else { continue };
            if e.header().entry_type().is_dir() {
                fs::create_dir_all(&target).map_err(es)?;
            } else {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(es)?;
                }
                e.unpack(&target).map_err(es)?;
            }
        } else {
            // unpack_in refuses paths that would escape `dest`
            e.unpack_in(dest).map_err(es)?;
        }
        count += 1;
    }
    Ok(count)
}

// ────────────────────────────── testing ────────────────────────────

/// Read every entry and verify checksums without writing anything.
pub fn test(path: &Path, password: &str, progress: Progress) -> Result<String, String> {
    let n = match Format::detect(path).ok_or("Unsupported archive format")? {
        Format::SevenZ => {
            let mut reader = SevenZReader::open(path, pw(password)).map_err(es)?;
            let mut n = 0usize;
            reader
                .for_each_entries(|entry, data| {
                    progress(format!("Testing {}", entry.name()));
                    io::copy(data, &mut io::sink())?;
                    n += 1;
                    Ok(true)
                })
                .map_err(es)?;
            n
        }
        Format::Rar => {
            let p = path.to_path_buf();
            let mut archive = rar_archive(&p, password).open_for_processing().map_err(es)?;
            let mut n = 0usize;
            while let Some(header) = archive.read_header().map_err(es)? {
                progress(format!("Testing {}", header.entry().filename.display()));
                archive = header.test().map_err(es)?;
                n += 1;
            }
            n
        }
        Format::Zip => {
            let mut z = zip::ZipArchive::new(File::open(path).map_err(es)?).map_err(es)?;
            for i in 0..z.len() {
                let res = if password.is_empty() {
                    z.by_index(i)
                } else {
                    z.by_index_decrypt(i, password.as_bytes())
                };
                let mut f = res.map_err(es)?;
                progress(format!("Testing {}", f.name()));
                io::copy(&mut f, &mut io::sink()).map_err(es)?; // CRC checked at EOF
            }
            z.len()
        }
        Format::Tar(c) => {
            let mut ar = tar::Archive::new(open_reader(path, c).map_err(es)?);
            let mut n = 0usize;
            for e in ar.entries().map_err(es)? {
                let mut e = e.map_err(es)?;
                io::copy(&mut e, &mut io::sink()).map_err(es)?;
                n += 1;
            }
            n
        }
        Format::Single(c) => {
            let mut r = open_reader(path, c).map_err(es)?;
            io::copy(&mut r, &mut io::sink()).map_err(es)?;
            1
        }
    };
    Ok(format!("OK – {n} item(s) verified"))
}

// ───────────────────────────── creation ────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    SevenZ,
    Zip,
    Tar(Comp),
}

/// Choose the output format from the file name the user typed.
pub fn kind_from_name(p: &Path) -> Option<Kind> {
    let n = p.file_name()?.to_string_lossy().to_lowercase();
    let ends = |xs: &[&str]| xs.iter().any(|x| n.ends_with(x));
    if ends(&[".7z"]) {
        Some(Kind::SevenZ)
    } else if ends(&[".zip"]) {
        Some(Kind::Zip)
    } else if ends(&[".tar.gz", ".tgz"]) {
        Some(Kind::Tar(Comp::Gz))
    } else if ends(&[".tar.bz2", ".tbz2", ".tbz"]) {
        Some(Kind::Tar(Comp::Bz2))
    } else if ends(&[".tar.xz", ".txz"]) {
        Some(Kind::Tar(Comp::Xz))
    } else if ends(&[".tar.zst", ".tzst"]) {
        Some(Kind::Tar(Comp::Zst))
    } else if ends(&[".tar"]) {
        Some(Kind::Tar(Comp::None))
    } else {
        None
    }
}

/// If the name has no known archive extension, default to .7z.
pub fn normalize_dest(p: PathBuf) -> PathBuf {
    if kind_from_name(&p).is_some() {
        p
    } else {
        let mut s = p.into_os_string();
        s.push(".7z");
        PathBuf::from(s)
    }
}

pub fn create(sources: &[PathBuf], dest: &Path, progress: Progress) -> Result<String, String> {
    let kind = kind_from_name(dest).ok_or("Unknown output format (use .7z, .zip, .tar.gz, …)")?;
    create_kind(kind, sources, dest, progress)?;
    Ok(format!("Created {}", dest.display()))
}

fn create_kind(kind: Kind, sources: &[PathBuf], dest: &Path, progress: Progress) -> Result<(), String> {
    match kind {
        Kind::SevenZ => create_7z(sources, dest, progress),
        Kind::Zip => create_zip(sources, dest, progress),
        Kind::Tar(comp) => create_tar(sources, dest, comp, progress),
    }
}

// ───────────────────────── adding to an existing archive ─────────────────────────

fn kind_of(fmt: Format) -> Option<Kind> {
    match fmt {
        Format::SevenZ => Some(Kind::SevenZ),
        Format::Zip => Some(Kind::Zip),
        Format::Tar(c) => Some(Kind::Tar(c)),
        Format::Rar | Format::Single(_) => None,
    }
}

/// Can files be added to archives of this format? (RAR cannot be written.)
pub fn can_add(fmt: Format) -> bool {
    kind_of(fmt).is_some()
}

fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
    }
    Ok(())
}

/// Rewrite an archive through a work folder: unpack, let `edit` change the files, pack into a
/// temporary file, then swap it in. The original is only replaced after a successful write.
fn rewrite_with<F>(
    archive: &Path,
    kind: Kind,
    password: &str,
    progress: Progress,
    edit: F,
) -> Result<(), String>
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    let fname = archive
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or("Invalid archive path")?;
    let work = archive.with_file_name(format!(".{fname}.rustarc-work"));
    let tmp = archive.with_file_name(format!(".{fname}.rustarc-tmp"));
    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_file(&tmp);

    let result = (|| -> Result<(), String> {
        fs::create_dir_all(&work).map_err(es)?;
        extract(archive, &work, None, password, progress)?;
        edit(&work)?;

        let mut top: Vec<PathBuf> = fs::read_dir(&work)
            .map_err(es)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        top.sort();
        progress("Writing archive…".to_string());
        create_kind(kind, &top, &tmp, progress)?;
        fs::rename(&tmp, archive).map_err(es)
    })();

    let _ = fs::remove_dir_all(&work);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn password_note(password: &str) -> &'static str {
    if password.is_empty() {
        ""
    } else {
        " (password protection was not kept)"
    }
}

/// Add files/folders to an archive, optionally inside the folder `into` ("" = top level).
/// Items with the same name as existing ones replace them.
///  * ZIP: appended in place when nothing clashes.
///  * 7z / TAR*: rewritten through a work folder (see `rewrite_with`).
pub fn add_files(
    archive: &Path,
    sources: &[PathBuf],
    into: &str,
    password: &str,
    progress: Progress,
) -> Result<String, String> {
    let fmt = Format::detect(archive).ok_or("Unsupported archive format")?;
    let kind = kind_of(fmt).ok_or("Adding files is not supported for this archive type")?;
    let into = into.trim_matches('/').to_string();

    if kind == Kind::Zip && try_zip_append(archive, sources, &into, progress)? {
        return Ok(format!("Added {} item(s)", sources.len()));
    }

    rewrite_with(archive, kind, password, progress, |work| {
        let target = if into.is_empty() {
            work.to_path_buf()
        } else {
            safe_join(work, &into).ok_or("Invalid target folder")?
        };
        fs::create_dir_all(&target).map_err(es)?;
        for src in sources {
            let name = src_name(src)?;
            progress(format!("Adding {name}"));
            copy_recursive(src, &target.join(&name)).map_err(es)?;
        }
        Ok(())
    })?;
    Ok(format!("Added {} item(s){}", sources.len(), password_note(password)))
}

/// Remove the selected files/folders (a selected folder takes its contents with it).
///  * ZIP: surviving entries are copied as-is into a new file (no recompression).
///  * 7z / TAR*: rewritten through a work folder.
pub fn remove_items(
    archive: &Path,
    remove: &HashSet<String>,
    password: &str,
    progress: Progress,
) -> Result<String, String> {
    let fmt = Format::detect(archive).ok_or("Unsupported archive format")?;
    let kind = kind_of(fmt).ok_or("Removing files is not supported for this archive type")?;

    if kind == Kind::Zip {
        remove_zip(archive, remove, progress)?;
        return Ok(format!("Removed {} item(s)", remove.len()));
    }

    rewrite_with(archive, kind, password, progress, |work| {
        for item in remove {
            let Some(path) = safe_join(work, item) else { continue };
            progress(format!("Removing {item}"));
            if path.is_dir() {
                fs::remove_dir_all(&path).map_err(es)?;
            } else if path.exists() {
                fs::remove_file(&path).map_err(es)?;
            }
        }
        Ok(())
    })?;
    Ok(format!("Removed {} item(s){}", remove.len(), password_note(password)))
}

fn remove_zip(archive: &Path, remove: &HashSet<String>, progress: Progress) -> Result<(), String> {
    let fname = archive
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or("Invalid archive path")?;
    let tmp = archive.with_file_name(format!(".{fname}.rustarc-tmp"));
    let _ = fs::remove_file(&tmp);

    let result = (|| -> Result<(), String> {
        let mut old = zip::ZipArchive::new(File::open(archive).map_err(es)?).map_err(es)?;
        let mut w = zip::ZipWriter::new(File::create(&tmp).map_err(es)?);
        for i in 0..old.len() {
            let f = old.by_index_raw(i).map_err(es)?;
            let name = norm(f.name());
            if wanted(&name, Some(remove)) {
                progress(format!("Removing {name}"));
                continue;
            }
            w.raw_copy_file(f).map_err(es)?;
        }
        w.finish().map_err(es)?;
        fs::rename(&tmp, archive).map_err(es)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Append to a ZIP without rewriting it. Returns Ok(false) if a name already exists
/// (the caller then rewrites the archive so the new items replace the old ones).
fn try_zip_append(
    archive: &Path,
    sources: &[PathBuf],
    into: &str,
    progress: Progress,
) -> Result<bool, String> {
    let existing: HashSet<String> = list_zip(archive)?
        .into_iter()
        .map(|e| e.name.trim_end_matches('/').to_string())
        .collect();
    let prefix = if into.is_empty() { String::new() } else { format!("{into}/") };
    let mut names = Vec::new();
    for s in sources {
        let n = format!("{prefix}{}", src_name(s)?);
        if existing.contains(&n) {
            return Ok(false);
        }
        names.push(n);
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(archive)
        .map_err(es)?;
    let mut w = zip::ZipWriter::new_append(file).map_err(es)?;
    for (s, n) in sources.iter().zip(names) {
        add_zip(&mut w, s, n, progress)?;
    }
    w.finish().map_err(es)?;
    Ok(true)
}

fn src_name(src: &Path) -> Result<String, String> {
    src.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| "Invalid source path".to_string())
}

fn sorted_children(path: &Path) -> Result<Vec<fs::DirEntry>, String> {
    let mut v: Vec<_> = fs::read_dir(path)
        .map_err(es)?
        .filter_map(Result::ok)
        .collect();
    v.sort_by_key(|c| c.file_name());
    Ok(v)
}

// ── 7z ──

fn create_7z(sources: &[PathBuf], dest: &Path, progress: Progress) -> Result<(), String> {
    let mut w = SevenZWriter::create(dest).map_err(es)?;
    for src in sources {
        add_7z(&mut w, src, src_name(src)?, progress)?;
    }
    w.finish().map_err(es)?;
    Ok(())
}

fn add_7z(
    w: &mut SevenZWriter<File>,
    path: &Path,
    name: String,
    progress: Progress,
) -> Result<(), String> {
    let meta = fs::metadata(path).map_err(es)?;
    progress(format!("Adding {name}"));
    let entry = SevenZArchiveEntry::from_path(path, name.clone());
    if meta.is_dir() {
        w.push_archive_entry::<&[u8]>(entry, None).map_err(es)?;
        for c in sorted_children(path)? {
            let child = format!("{}/{}", name, c.file_name().to_string_lossy());
            add_7z(w, &c.path(), child, progress)?;
        }
    } else {
        let f = File::open(path).map_err(es)?;
        w.push_archive_entry(entry, Some(f)).map_err(es)?;
    }
    Ok(())
}

// ── zip ──

fn create_zip(sources: &[PathBuf], dest: &Path, progress: Progress) -> Result<(), String> {
    let mut w = zip::ZipWriter::new(File::create(dest).map_err(es)?);
    for src in sources {
        add_zip(&mut w, src, src_name(src)?, progress)?;
    }
    w.finish().map_err(es)?;
    Ok(())
}

fn add_zip(
    w: &mut zip::ZipWriter<File>,
    path: &Path,
    name: String,
    progress: Progress,
) -> Result<(), String> {
    let meta = fs::metadata(path).map_err(es)?;
    progress(format!("Adding {name}"));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(meta.len() >= u32::MAX as u64);
    if meta.is_dir() {
        w.add_directory(format!("{name}/"), opts).map_err(es)?;
        for c in sorted_children(path)? {
            let child = format!("{}/{}", name, c.file_name().to_string_lossy());
            add_zip(w, &c.path(), child, progress)?;
        }
    } else {
        w.start_file(name, opts).map_err(es)?;
        let mut f = File::open(path).map_err(es)?;
        io::copy(&mut f, w).map_err(es)?;
    }
    Ok(())
}

// ── tar (+ compression) ──

fn create_tar(
    sources: &[PathBuf],
    dest: &Path,
    comp: Comp,
    progress: Progress,
) -> Result<(), String> {
    let file = File::create(dest).map_err(es)?;
    match comp {
        Comp::None => {
            let mut f = build_tar(file, sources, progress)?;
            f.flush().map_err(es)?;
        }
        Comp::Gz => {
            let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            build_tar(enc, sources, progress)?.finish().map_err(es)?;
        }
        Comp::Bz2 => {
            let enc = bzip2::write::BzEncoder::new(file, bzip2::Compression::default());
            build_tar(enc, sources, progress)?.finish().map_err(es)?;
        }
        Comp::Xz => {
            let enc = xz2::write::XzEncoder::new(file, 6);
            build_tar(enc, sources, progress)?.finish().map_err(es)?;
        }
        Comp::Zst => {
            let enc = zstd::stream::write::Encoder::new(file, 3).map_err(es)?;
            build_tar(enc, sources, progress)?.finish().map_err(es)?;
        }
    }
    Ok(())
}

fn build_tar<W: Write>(w: W, sources: &[PathBuf], progress: Progress) -> Result<W, String> {
    let mut b = tar::Builder::new(w);
    for src in sources {
        add_tar(&mut b, src, src_name(src)?, progress)?;
    }
    b.into_inner().map_err(es)
}

fn add_tar<W: Write>(
    b: &mut tar::Builder<W>,
    path: &Path,
    name: String,
    progress: Progress,
) -> Result<(), String> {
    let meta = fs::metadata(path).map_err(es)?;
    progress(format!("Adding {name}"));
    if meta.is_dir() {
        b.append_dir(&name, path).map_err(es)?;
        for c in sorted_children(path)? {
            let child = format!("{}/{}", name, c.file_name().to_string_lossy());
            add_tar(b, &c.path(), child, progress)?;
        }
    } else {
        b.append_path_with_name(path, &name).map_err(es)?;
    }
    Ok(())
}


// ───────────────────── rename / cut / copy / paste / open with ─────────────────────

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let mut n = 1;
    loop {
        let cand = if n == 1 {
            format!("{stem} (copy){ext}")
        } else {
            format!("{stem} (copy {n}){ext}")
        };
        let p = dir.join(&cand);
        if !p.exists() {
            return p;
        }
        n += 1;
    }
}

/// Rename a file or folder inside the archive (`new_name` is just the new last component).
pub fn rename_item(
    archive: &Path,
    old_full: &str,
    new_name: &str,
    password: &str,
    progress: Progress,
) -> Result<String, String> {
    let fmt = Format::detect(archive).ok_or("Unsupported archive format")?;
    let kind = kind_of(fmt).ok_or("Renaming is not supported for this archive type")?;
    let old = old_full.trim_matches('/').to_string();
    let new_name = new_name.trim().to_string();
    if new_name.is_empty()
        || new_name == "."
        || new_name == ".."
        || new_name.contains('/')
        || new_name.contains('\\')
    {
        return Err("Invalid name".into());
    }
    let parent = old.rfind('/').map(|i| old[..i].to_string()).unwrap_or_default();
    let new_full = if parent.is_empty() {
        new_name.clone()
    } else {
        format!("{parent}/{new_name}")
    };
    if new_full == old {
        return Ok("Name unchanged".into());
    }

    if kind == Kind::Zip {
        rename_zip(archive, &old, &new_full)?;
        return Ok(format!("Renamed to {new_name}"));
    }

    rewrite_with(archive, kind, password, progress, |work| {
        let from = safe_join(work, &old).ok_or("Invalid path")?;
        let to = safe_join(work, &new_full).ok_or("Invalid name")?;
        if to.exists() {
            return Err(format!("\"{new_name}\" already exists"));
        }
        fs::rename(&from, &to).map_err(es)
    })?;
    Ok(format!("Renamed to {new_name}{}", password_note(password)))
}

/// ZIP: copy every entry as-is into a new file, renaming the matching ones.
fn rename_zip(archive: &Path, old: &str, new_full: &str) -> Result<(), String> {
    let fname = archive
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or("Invalid archive path")?;
    let tmp = archive.with_file_name(format!(".{fname}.rustarc-tmp"));
    let _ = fs::remove_file(&tmp);

    let result = (|| -> Result<(), String> {
        let mut src = zip::ZipArchive::new(File::open(archive).map_err(es)?).map_err(es)?;
        for i in 0..src.len() {
            let f = src.by_index_raw(i).map_err(es)?;
            if norm(f.name()).trim_end_matches('/') == new_full {
                return Err(format!("\"{new_full}\" already exists"));
            }
        }
        let mut w = zip::ZipWriter::new(File::create(&tmp).map_err(es)?);
        let old_prefix = format!("{old}/");
        for i in 0..src.len() {
            let f = src.by_index_raw(i).map_err(es)?;
            let name = norm(f.name());
            let trimmed = name.trim_end_matches('/');
            let slash = if name.ends_with('/') { "/" } else { "" };
            let renamed = if trimmed == old {
                Some(format!("{new_full}{slash}"))
            } else if let Some(rest) = trimmed.strip_prefix(&old_prefix) {
                Some(format!("{new_full}/{rest}{slash}"))
            } else {
                None
            };
            match renamed {
                Some(n) => w.raw_copy_file_rename(f, n).map_err(es)?,
                None => w.raw_copy_file(f).map_err(es)?,
            }
        }
        w.finish().map_err(es)?;
        fs::rename(&tmp, archive).map_err(es)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Paste items (cut = move, otherwise copy) into `dest_folder` ("" = top level) of the same archive.
pub fn paste_items(
    archive: &Path,
    items: &[String],
    dest_folder: &str,
    cut: bool,
    password: &str,
    progress: Progress,
) -> Result<String, String> {
    let fmt = Format::detect(archive).ok_or("Unsupported archive format")?;
    let kind = kind_of(fmt).ok_or("Pasting is not supported for this archive type")?;
    let dest_folder = dest_folder.trim_matches('/').to_string();

    rewrite_with(archive, kind, password, progress, |work| {
        let dest_dir = if dest_folder.is_empty() {
            work.to_path_buf()
        } else {
            safe_join(work, &dest_folder).ok_or("Invalid folder")?
        };
        fs::create_dir_all(&dest_dir).map_err(es)?;
        for item in items {
            let from = safe_join(work, item).ok_or("Invalid path")?;
            if !from.exists() {
                continue;
            }
            let name = from
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or("Invalid name")?;
            if dest_dir.starts_with(&from) && dest_dir != from.parent().unwrap_or(work) {
                return Err("Cannot paste a folder into itself".into());
            }
            progress(format!("{} {name}", if cut { "Moving" } else { "Copying" }));
            if cut {
                let to = dest_dir.join(&name);
                if to == from {
                    continue;
                }
                if to.exists() {
                    return Err(format!("\"{name}\" already exists in the target folder"));
                }
                fs::rename(&from, &to).map_err(es)?;
            } else {
                copy_recursive(&from, &unique_path(&dest_dir, &name)).map_err(es)?;
            }
        }
        Ok(())
    })?;
    Ok(format!(
        "{} {} item(s){}",
        if cut { "Moved" } else { "Copied" },
        items.len(),
        password_note(password)
    ))
}

fn file_uri(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// "Open With…": ask the desktop portal to show its application chooser. Falls back to the
/// default application when the portal (or `gdbus`) is not available.
pub fn open_with_chooser(path: &Path) -> Result<String, String> {
    use std::process::Command;
    let uri = file_uri(path);
    let shown = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.portal.Desktop",
            "--object-path",
            "/org/freedesktop/portal/desktop",
            "--method",
            "org.freedesktop.portal.OpenURI.OpenURI",
            "",
            &uri,
            "{'ask': <true>}",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if shown {
        return Ok("Choose an application…".into());
    }
    Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map_err(|e| format!("Could not start xdg-open: {e}"))?;
    Ok("The \"Open With\" chooser is not available here; opened with the default application.".into())
}
