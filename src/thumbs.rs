use crate::config::data_dir;
use crate::gcode::{self, Thumbnail};
use crate::station::Wake;
use crate::toolpath;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

pub const WIDTH: usize = 192;
pub const HEIGHT: usize = 144;

pub struct Thumbs {
    ask: Sender<PathBuf>,
    done: Receiver<(PathBuf, Option<Thumbnail>)>,
}

impl Thumbs {
    pub fn spawn(wake: Wake) -> Thumbs {
        let (ask, asked) = channel::<PathBuf>();
        let (tell, done) = channel();
        let dir = cache_dir();
        std::thread::Builder::new()
            .name("thumbnails".into())
            .spawn(move || {
                for path in asked {
                    let thumb = gcode::thumbnail(&path).or_else(|| cached_or_drawn(&dir, &path));
                    if tell.send((path, thumb)).is_err() {
                        return;
                    }
                    wake();
                }
            })
            .ok();
        Thumbs { ask, done }
    }

    pub fn request(&self, path: &Path) {
        let _ = self.ask.send(path.to_path_buf());
    }

    pub fn ready(&self) -> Vec<(PathBuf, Option<Thumbnail>)> {
        self.done.try_iter().collect()
    }
}

pub fn cache_dir() -> PathBuf {
    data_dir().join("thumbs")
}

fn cached_or_drawn(dir: &Path, path: &Path) -> Option<Thumbnail> {
    let file = dir.join(format!("{:016x}.png", key(path)?));
    if let Some(thumb) = load(&file) {
        return Some(thumb);
    }
    let thumb = toolpath::snapshot(path, WIDTH, HEIGHT).ok()?;
    let _ = std::fs::create_dir_all(dir);
    let _ = image::save_buffer(
        &file,
        &thumb.rgba,
        thumb.width as u32,
        thumb.height as u32,
        image::ColorType::Rgba8,
    );
    Some(thumb)
}

fn load(file: &Path) -> Option<Thumbnail> {
    let img = image::open(file).ok()?.to_rgba8();
    Some(Thumbnail {
        width: img.width() as usize,
        height: img.height() as usize,
        rgba: img.into_raw(),
    })
}

fn key(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let text = format!("{}\n{}\n{modified}\n{WIDTH}x{HEIGHT}", path.display(), meta.len());
    Some(
        text.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_part_is_drawn_once_and_then_read_back() {
        let dir = std::env::temp_dir().join(format!("printfarm-thumbs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("part.gcode");
        let mut body = String::from("G90\nM83\nG1 X0 Y0 Z0.2\n");
        for layer in 0..10 {
            body.push_str(&format!(";LAYER_CHANGE\nG1 Z{:.1}\n", 0.2 + layer as f32 * 0.2));
            body.push_str("G1 X0 Y0\nG1 X20 Y0 E1\nG1 X20 Y20 E1\nG1 X0 Y20 E1\nG1 X0 Y0 E1\n");
        }
        std::fs::write(&part, body).unwrap();
        let cache = dir.join("cache");
        let drawn = cached_or_drawn(&cache, &part).unwrap();
        assert_eq!((drawn.width, drawn.height), (WIDTH, HEIGHT));
        let background = &drawn.rgba[..4];
        assert!(drawn.rgba.chunks(4).any(|px| px != background));
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
        let again = cached_or_drawn(&cache, &part).unwrap();
        assert_eq!(again.rgba, drawn.rgba);
    }
}
