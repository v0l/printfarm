use crate::station::Wake;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SNAPSHOT_EVERY: Duration = Duration::from_secs(1);
const STREAM_EVERY: Duration = Duration::from_millis(200);
const RETRY_AFTER: Duration = Duration::from_secs(5);
const MAX_FRAME: usize = 8 << 20;

pub struct Frame {
    pub seq: u64,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

#[derive(Default)]
pub struct Feed {
    pub frame: Option<Frame>,
    pub error: Option<String>,
}

pub struct Camera {
    pub url: String,
    pub feed: Arc<Mutex<Feed>>,
    stop: Arc<AtomicBool>,
}

impl Camera {
    pub fn start(url: String, wake: Wake) -> Camera {
        let feed = Arc::new(Mutex::new(Feed::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (f, s, u) = (feed.clone(), stop.clone(), url.clone());
        let _ = std::thread::Builder::new()
            .name("camera".into())
            .spawn(move || watch(&u, &f, &s, &wake));
        Camera { url, feed, stop }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn watch(url: &str, feed: &Mutex<Feed>, stop: &AtomicBool, wake: &Wake) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_recv_body(Some(Duration::from_secs(300)))
        .timeout_recv_response(Some(Duration::from_secs(10)))
        .build()
        .into();
    let mut seq = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let result = fetch(&agent, url, stop, &mut |jpeg| {
            seq += 1;
            match decode(jpeg, seq) {
                Ok(frame) => {
                    if let Ok(mut f) = feed.lock() {
                        f.frame = Some(frame);
                        f.error = None;
                    }
                    wake();
                }
                Err(e) => {
                    if let Ok(mut f) = feed.lock() {
                        f.error = Some(e);
                    }
                }
            }
        });
        match result {
            Ok(()) => sleep(stop, SNAPSHOT_EVERY),
            Err(e) => {
                if let Ok(mut f) = feed.lock() {
                    f.error = Some(e);
                }
                wake();
                sleep(stop, RETRY_AFTER);
            }
        }
    }
}

fn sleep(stop: &AtomicBool, d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn fetch(
    agent: &ureq::Agent,
    url: &str,
    stop: &AtomicBool,
    frame: &mut dyn FnMut(&[u8]),
) -> Result<(), String> {
    let mut resp = agent.get(url).call().map_err(|e| format!("{url}: {e}"))?;
    let kind = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut body = resp.body_mut().with_config().limit(u64::MAX).reader();
    if !kind.starts_with("multipart/") {
        let mut bytes = Vec::new();
        body.take(MAX_FRAME as u64).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        frame(&bytes);
        return Ok(());
    }
    let mut buf = Vec::with_capacity(1 << 18);
    let mut chunk = [0u8; 1 << 15];
    let mut last = Instant::now() - STREAM_EVERY;
    let mut seen = false;
    while !stop.load(Ordering::Relaxed) {
        let n = match body.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(_) if seen => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Some((start, end)) = jpeg_span(&buf) {
            if last.elapsed() >= STREAM_EVERY {
                last = Instant::now();
                seen = true;
                frame(&buf[start..end]);
            }
            buf.drain(..end);
        }
        if buf.len() > MAX_FRAME {
            buf.clear();
        }
    }
    Ok(())
}

pub fn jpeg_span(buf: &[u8]) -> Option<(usize, usize)> {
    let start = buf.windows(2).position(|w| w == [0xFF, 0xD8])?;
    let end = buf[start + 2..].windows(2).position(|w| w == [0xFF, 0xD9])? + start + 4;
    Some((start, end))
}

fn decode(bytes: &[u8], seq: u64) -> Result<Frame, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("camera image: {e}"))?;
    let img = if img.width() > 1280 { img.thumbnail(1280, 1280) } else { img };
    let rgba = img.to_rgba8();
    Ok(Frame {
        seq,
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        rgba: rgba.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_frames_in_a_stream() {
        let mut s = b"--frame\r\nContent-Type: image/jpeg\r\n\r\n".to_vec();
        s.extend_from_slice(&[0xFF, 0xD8, 1, 2, 0xFF, 0xD9]);
        s.extend_from_slice(b"\r\n--frame\r\n");
        let (a, b) = jpeg_span(&s).unwrap();
        assert_eq!(&s[a..b], &[0xFF, 0xD8, 1, 2, 0xFF, 0xD9]);
        assert!(jpeg_span(&[0xFF, 0xD8, 1, 2]).is_none());
    }
}
