//! Thumbnail loader: a small pool of worker threads reads from the disk cache
//! or downloads, decodes and downsizes; the UI thread turns results into
//! textures. Bounded concurrency, bounded GPU memory (LRU of textures).

use crate::cache::{self, Limits};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use std::{
    collections::HashMap,
    io::Read,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    time::Duration,
};

const WORKERS: usize = 6;
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Textures kept in memory (~0.9 MB each at 640×360). Visible cards are ~20.
const MAX_TEXTURES: usize = 160;
/// Run cache eviction every this many new files.
const ENFORCE_EVERY: u32 = 200;

/// Which thumbnail to fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Variant {
    /// 320×180
    Standard,
    /// 1280×720 source, stored downsized to 640 wide; old videos fall back to standard.
    High,
}

impl Variant {
    fn tag(self) -> &'static str {
        match self {
            Variant::Standard => "sd",
            Variant::High => "hi",
        }
    }
    /// Try in order; WebP is ~half the size of the JPEG for the same image.
    fn urls(self, id: &str) -> Vec<String> {
        let sd = [
            format!("https://i.ytimg.com/vi_webp/{id}/mqdefault.webp"),
            format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"),
        ];
        match self {
            Variant::Standard => sd.to_vec(),
            Variant::High => {
                let mut v = vec![
                    format!("https://i.ytimg.com/vi_webp/{id}/hq720.webp"),
                    format!("https://i.ytimg.com/vi/{id}/hq720.jpg"),
                ];
                v.extend(sd);
                v
            }
        }
    }
    fn max_width(self) -> u32 {
        match self {
            Variant::Standard => 320,
            Variant::High => 640,
        }
    }
}

enum Slot {
    Pending,
    Ready(TextureHandle, u64),
    Failed,
}

type Key = (String, Variant);
type Done = (Key, Result<ColorImage, String>);

pub struct Thumbs {
    slots: HashMap<Key, Slot>,
    req_tx: Sender<Key>,
    done_rx: Receiver<Done>,
    frame: u64,
}

impl Thumbs {
    pub fn new(ctx: &egui::Context, limits: Arc<Limits>) -> Self {
        let (req_tx, req_rx) = channel::<Key>();
        let (done_tx, done_rx) = channel::<Done>();
        let req_rx = Arc::new(Mutex::new(req_rx));
        let writes = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .http_status_as_error(true)
            .user_agent("ytmini")
            .build()
            .into();
        for _ in 0..WORKERS {
            let (req_rx, done_tx, ctx, agent) = (req_rx.clone(), done_tx.clone(), ctx.clone(), agent.clone());
            let (limits, writes) = (limits.clone(), writes.clone());
            std::thread::Builder::new()
                .name("thumbs".into())
                .spawn(move || loop {
                    // Hold the lock only while waiting for the next key.
                    let next = req_rx.lock().ok().and_then(|rx| rx.recv().ok());
                    let Some(key) = next else { break }; // app is shutting down
                    let result = load(&agent, &key, || {
                        let n = writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if n % ENFORCE_EVERY == 0 {
                            cache::enforce(&limits);
                        }
                    });
                    if done_tx.send((key, result)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                })
                .expect("spawning thumbnail worker");
        }
        Thumbs { slots: HashMap::new(), req_tx, done_rx, frame: 0 }
    }

    /// Upload finished loads as textures and trim the LRU. Call once per frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        self.frame += 1;
        while let Ok((key, result)) = self.done_rx.try_recv() {
            let slot = match result {
                Ok(img) => {
                    let name = format!("{}-{}", key.0, key.1.tag());
                    Slot::Ready(ctx.load_texture(name, img, TextureOptions::LINEAR), self.frame)
                }
                Err(_) => Slot::Failed,
            };
            self.slots.insert(key, slot);
        }
        let ready = self.slots.values().filter(|s| matches!(s, Slot::Ready(..))).count();
        if ready > MAX_TEXTURES {
            let mut ages: Vec<(u64, Key)> = self
                .slots
                .iter()
                .filter_map(|(k, s)| match s {
                    Slot::Ready(_, used) => Some((*used, k.clone())),
                    _ => None,
                })
                .collect();
            ages.sort_by_key(|a| a.0);
            for (_, k) in ages.into_iter().take(ready - MAX_TEXTURES) {
                self.slots.remove(&k); // dropping the handle frees the texture
            }
        }
    }

    /// The texture if it's ready; otherwise queues the load (once).
    pub fn get(&mut self, id: &str, variant: Variant) -> Option<&TextureHandle> {
        let key = (id.to_string(), variant);
        match self.slots.get_mut(&key) {
            None => {
                self.slots.insert(key.clone(), Slot::Pending);
                let _ = self.req_tx.send(key);
                None
            }
            Some(Slot::Ready(_, used)) => {
                *used = self.frame;
                match self.slots.get(&key) {
                    Some(Slot::Ready(t, _)) => Some(t),
                    _ => None,
                }
            }
            Some(Slot::Pending | Slot::Failed) => None,
        }
    }

    /// Forget everything in memory (after the disk cache is cleared).
    pub fn clear(&mut self) {
        self.slots.clear();
    }
}

fn load(agent: &ureq::Agent, key: &Key, on_write: impl FnOnce()) -> Result<ColorImage, String> {
    let (id, variant) = key;
    let disk_key = format!("{id}-{}", variant.tag());
    if let Some(bytes) = cache::read_thumb(&disk_key) {
        let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
        return Ok(to_color_image(fit_width(img, variant.max_width())));
    }
    let downloaded = download_first(agent, &variant.urls(id))?;
    let img = image::load_from_memory(&downloaded).map_err(|e| e.to_string())?;
    let oversized = img.width() > variant.max_width();
    let img = fit_width(img, variant.max_width());
    // Store what we display: a 1280×720 WebP averages ~120 KB, the same image
    // at 640×360 as JPEG ~40 KB — 3× more thumbnails per MB of cache.
    // Stored under the requested variant even if we fell back to a smaller
    // source, so old videos without hq720 don't cost a 404 every time.
    let stored = if oversized { encode_jpeg(&img).unwrap_or(downloaded) } else { downloaded };
    cache::write_thumb(&disk_key, &stored);
    on_write();
    Ok(to_color_image(img))
}

fn fit_width(img: image::DynamicImage, max_w: u32) -> image::DynamicImage {
    if img.width() <= max_w {
        return img;
    }
    let h = img.height() * max_w / img.width();
    img.resize_exact(max_w, h, image::imageops::FilterType::Triangle)
}

fn encode_jpeg(img: &image::DynamicImage) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80).encode_image(&img.to_rgb8()).ok()?;
    Some(out)
}

fn to_color_image(img: image::DynamicImage) -> ColorImage {
    let rgba = img.to_rgba8();
    ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw())
}

fn download_first(agent: &ureq::Agent, urls: &[String]) -> Result<Vec<u8>, String> {
    let mut last_err = String::from("no urls");
    for url in urls {
        match agent.get(url).call() {
            Ok(mut resp) => {
                let mut bytes = Vec::new();
                resp.body_mut().as_reader().take(MAX_BYTES).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                return Ok(bytes);
            }
            Err(e) => last_err = e.to_string(), // 404 → try the next one
        }
    }
    Err(last_err)
}
