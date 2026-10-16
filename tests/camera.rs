use printfarm::camera::Camera;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn jpeg(shade: u8) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(64, 48, image::Rgb([shade, 100, 200]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut out).encode_image(&img).unwrap();
    out
}

fn serve(stream: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                line.clear();
            }
            if stream {
                let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n");
                for n in 0..50u8 {
                    let frame = jpeg(n * 5);
                    let head = format!(
                        "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                        frame.len()
                    );
                    if conn
                        .write_all(head.as_bytes())
                        .and_then(|_| conn.write_all(&frame))
                        .and_then(|_| conn.write_all(b"\r\n"))
                        .is_err()
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            } else {
                let frame = jpeg(10);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    frame.len()
                );
                let _ = conn.write_all(head.as_bytes()).and_then(|_| conn.write_all(&frame));
            }
        }
    });
    format!("http://{addr}/cam")
}

fn frames_from(url: String, want: u64) -> (usize, usize, u64) {
    let cam = Camera::start(url, Arc::new(|| {}));
    let t = Instant::now();
    loop {
        if let Some(f) = &cam.feed.lock().unwrap().frame
            && f.seq >= want
        {
            return (f.width, f.height, f.seq);
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "no frames: {:?}",
            cam.feed.lock().unwrap().error
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn reads_a_snapshot() {
    let (w, h, _) = frames_from(serve(false), 1);
    assert_eq!((w, h), (64, 48));
}

#[test]
fn reads_an_mjpeg_stream() {
    let (w, h, seq) = frames_from(serve(true), 3);
    assert_eq!((w, h), (64, 48));
    assert!(seq >= 3);
}

#[test]
fn says_why_a_camera_is_missing() {
    let cam = Camera::start("http://127.0.0.1:9/nothing".into(), Arc::new(|| {}));
    let t = Instant::now();
    while cam.feed.lock().unwrap().error.is_none() {
        assert!(t.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(50));
    }
}
