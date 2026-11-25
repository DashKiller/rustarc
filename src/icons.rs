//! Small vector icons drawn with the egui painter (no font glyphs or image files needed).

use eframe::egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Folder,
    File,
    Text,
    Image,
    Archive,
    Exe,
    Media,
    Extract,
    Add,
    Find,
    Preview,
    Open,
    Remove,
    Menu,
    ChevronDown,
    ChevronUp,
    ChevronRight,
}

/// Pick a file icon from the file name.
pub fn for_file(name: &str) -> Icon {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "nfo" | "md" | "log" | "rtf" | "doc" | "docx" | "odt" | "pdf" | "csv" | "json"
        | "xml" | "ini" | "cfg" | "conf" | "toml" | "yaml" | "yml" | "rs" | "py" | "c" | "h"
        | "cpp" | "js" | "ts" | "html" | "css" | "sh" | "diz" => Icon::Text,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "svg" | "webp" | "ico" | "tif" | "tiff" => Icon::Image,
        "zip" | "7z" | "rar" | "tar" | "gz" | "bz2" | "xz" | "zst" | "tgz" | "iso" => Icon::Archive,
        "exe" | "dll" | "msi" | "bat" | "com" | "so" | "appimage" | "bin" => Icon::Exe,
        "mp3" | "wav" | "flac" | "ogg" | "mp4" | "mkv" | "avi" | "mov" | "webm" | "m4a" => Icon::Media,
        _ => Icon::File,
    }
}

/// Draw `icon` centred in `rect` (designed on a 16x16 grid).
pub fn draw(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = rect.width().min(rect.height()) / 16.0;
    let o = rect.center() - Vec2::splat(8.0 * s);
    let pt = |x: f32, y: f32| Pos2::new(o.x + x * s, o.y + y * s);
    let st = Stroke::new((1.3 * s).max(1.0), color);
    let line = |pts: &[(f32, f32)], closed: bool| {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| pt(x, y)).collect();
        p.add(if closed { Shape::closed_line(v, st) } else { Shape::line(v, st) });
    };
    let page = || {
        line(&[(3.0, 1.0), (10.0, 1.0), (13.0, 4.0), (13.0, 15.0), (3.0, 15.0)], true);
        line(&[(10.0, 1.0), (10.0, 4.0), (13.0, 4.0)], false);
    };

    match icon {
        Icon::Folder => {
            let fill = color.gamma_multiply(0.12);
            p.rect_filled(Rect::from_min_max(pt(1.0, 4.0), pt(7.0, 7.0)), 1.0, fill);
            p.rect_filled(Rect::from_min_max(pt(1.0, 6.0), pt(15.0, 13.0)), 1.0, fill);
            line(
                &[(1.0, 4.0), (6.0, 4.0), (8.0, 6.0), (15.0, 6.0), (15.0, 13.0), (1.0, 13.0)],
                true,
            );
        }
        Icon::File => page(),
        Icon::Text => {
            page();
            line(&[(5.0, 8.0), (11.0, 8.0)], false);
            line(&[(5.0, 10.5), (11.0, 10.5)], false);
            line(&[(5.0, 13.0), (9.0, 13.0)], false);
        }
        Icon::Image => {
            page();
            p.circle_stroke(pt(7.0, 7.5), 1.3 * s, st);
            line(&[(4.5, 13.0), (7.5, 10.0), (9.5, 12.0), (11.0, 10.5), (12.0, 13.0)], false);
        }
        Icon::Archive => {
            page();
            line(&[(8.0, 3.0), (8.0, 4.5)], false);
            line(&[(8.0, 6.0), (8.0, 7.5)], false);
            line(&[(7.0, 9.0), (9.0, 9.0), (9.0, 12.5), (7.0, 12.5)], true);
        }
        Icon::Exe => {
            page();
            line(&[(5.0, 6.5), (11.0, 6.5), (11.0, 12.5), (5.0, 12.5)], true);
            line(&[(5.0, 8.5), (11.0, 8.5)], false);
        }
        Icon::Media => {
            page();
            line(&[(6.0, 6.5), (11.0, 9.5), (6.0, 12.5)], true);
        }
        Icon::Extract => {
            line(&[(2.0, 9.0), (2.0, 14.0), (14.0, 14.0), (14.0, 9.0)], false);
            line(&[(8.0, 11.0), (8.0, 2.0)], false);
            line(&[(4.5, 5.5), (8.0, 2.0), (11.5, 5.5)], false);
        }
        Icon::Add => {
            page();
            line(&[(8.0, 6.0), (8.0, 12.0)], false);
            line(&[(5.0, 9.0), (11.0, 9.0)], false);
        }
        Icon::Find => {
            p.circle_stroke(pt(6.8, 6.8), 4.5 * s, st);
            line(&[(10.2, 10.2), (14.5, 14.5)], false);
        }
        Icon::Preview => {
            page();
            p.circle_stroke(pt(8.0, 9.5), 2.4 * s, st);
        }
        Icon::Open => {
            line(&[(7.0, 3.0), (2.0, 3.0), (2.0, 14.0), (13.0, 14.0), (13.0, 9.0)], false);
            line(&[(7.5, 8.5), (14.0, 2.0)], false);
            line(&[(9.5, 2.0), (14.0, 2.0), (14.0, 6.5)], false);
        }
        Icon::Remove => {
            page();
            line(&[(5.0, 9.5), (11.0, 9.5)], false);
        }
        Icon::Menu => {
            line(&[(2.0, 4.0), (14.0, 4.0)], false);
            line(&[(2.0, 8.0), (14.0, 8.0)], false);
            line(&[(2.0, 12.0), (14.0, 12.0)], false);
        }
        Icon::ChevronDown => line(&[(4.0, 6.0), (8.0, 10.0), (12.0, 6.0)], false),
        Icon::ChevronUp => line(&[(4.0, 10.0), (8.0, 6.0), (12.0, 10.0)], false),
        Icon::ChevronRight => line(&[(6.0, 3.5), (10.5, 8.0), (6.0, 12.5)], false),
    }
}
