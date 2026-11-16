//! Thumbnail loader: a small pool of worker threads downloads and decodes JPEGs,
//! the UI thread turns them into textures. Bounded concurrency (unlike one
//! thread per image), in-memory cache keyed by URL.

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

enum Slot {
    Pending,
    Ready(TextureHandle),
    Failed,
}

type Done = (String, Result<ColorImage, String>);

pub struct Thumbs {
    slots: HashMap<String, Slot>,
    req_tx: Sender<String>,
    done_rx: Receiver<Done>,
}

impl Thumbs {
    pub fn new(ctx: &egui::Context) -> Self {
        let (req_tx, req_rx) = channel::<String>();
        let (done_tx, done_rx) = channel::<Done>();
        let req_rx = Arc::new(Mutex::new(req_rx));
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .user_agent("ytmini")
            .build()
            .into();
        for _ in 0..WORKERS {
            let (req_rx, done_tx, ctx, agent) = (req_rx.clone(), done_tx.clone(), ctx.clone(), agent.clone());
            std::thread::Builder::new()
                .name("thumbs".into())
                .spawn(move || loop {
                    // Hold the lock only while waiting for the next URL.
                    let next = req_rx.lock().ok().and_then(|rx| rx.recv().ok());
                    let Some(url) = next else { break }; // app is shutting down
                    let result = download(&agent, &url);
                    if done_tx.send((url, result)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                })
                .expect("spawning thumbnail worker");
        }
        Thumbs { slots: HashMap::new(), req_tx, done_rx }
    }

    /// Upload finished downloads as textures. Call once per frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((url, result)) = self.done_rx.try_recv() {
            let slot = match result {
                Ok(img) => Slot::Ready(ctx.load_texture(&url, img, TextureOptions::LINEAR)),
                Err(_) => Slot::Failed,
            };
            self.slots.insert(url, slot);
        }
    }

    /// The texture if it's ready; otherwise queues the download (once).
    pub fn get(&mut self, url: &str) -> Option<&TextureHandle> {
        if !self.slots.contains_key(url) {
            self.slots.insert(url.to_string(), Slot::Pending);
            let _ = self.req_tx.send(url.to_string());
            return None;
        }
        match self.slots.get(url) {
            Some(Slot::Ready(t)) => Some(t),
            Some(Slot::Pending | Slot::Failed) | None => None,
        }
    }
}

fn download(agent: &ureq::Agent, url: &str) -> Result<ColorImage, String> {
    let mut resp = agent.get(url).call().map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    resp.body_mut()
        .as_reader()
        .take(MAX_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, img.as_raw()))
}
