//! Thumbnails made on a small pool of worker threads, kept as textures in a
//! bounded cache. The newest request is served first (what is on screen
//! now matters more than what scrolled away).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use egui::{ColorImage, TextureHandle, TextureOptions};

const WORKERS: usize = 3;
const MAX_QUEUED: usize = 160;
const MAX_CACHED: usize = 500;

type Key = (PathBuf, u32);

/// What is known about a thumbnail.
#[derive(Clone)]
pub enum Thumb {
    /// Being made.
    Loading,
    /// Ready.
    Ready(TextureHandle),
    /// There is none (a video, an unknown or undecodable file).
    None,
}

enum Entry {
    Queued,
    Ready(TextureHandle),
    None,
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<Key>,
    done: Vec<(Key, Option<ColorImage>)>,
    closed: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
}

fn lock(m: &Mutex<Queue>) -> std::sync::MutexGuard<'_, Queue> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The thumbnail cache and its workers.
pub struct Thumbnails {
    shared: Arc<Shared>,
    cache: HashMap<Key, (Entry, u64)>,
    order: VecDeque<(Key, u64)>,
    generation: u64,
}

impl Thumbnails {
    /// Start the workers.
    pub fn new(ctx: &egui::Context) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
        });
        for i in 0..WORKERS {
            let shared = shared.clone();
            let ctx = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("diskrescue-thumb-{i}"))
                .spawn(move || worker(&shared, &ctx));
            if let Err(e) = spawned {
                log::warn!("cannot start a thumbnail worker: {e}");
            }
        }
        Thumbnails {
            shared,
            cache: HashMap::new(),
            order: VecDeque::new(),
            generation: 0,
        }
    }

    /// Turn finished work into textures. Call once per frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let done = std::mem::take(&mut lock(&self.shared.queue).done);
        for (key, img) in done {
            let entry = match img {
                Some(img) => Entry::Ready(ctx.load_texture(
                    format!("thumb:{}:{}", key.0.display(), key.1),
                    img,
                    TextureOptions::LINEAR,
                )),
                None => Entry::None,
            };
            if let Some(slot) = self.cache.get_mut(&key) {
                slot.0 = entry;
            }
        }
        self.evict();
    }

    fn evict(&mut self) {
        let mut budget = self.order.len();
        while self.order.len() > MAX_CACHED && budget > 0 {
            budget -= 1;
            let Some((k, generation)) = self.order.pop_front() else {
                break;
            };
            match self.cache.get(&k) {
                Some((_, g)) if *g != generation => {}
                Some((Entry::Queued, _)) => self.order.push_back((k, generation)),
                Some(_) => {
                    self.cache.remove(&k);
                }
                None => {}
            }
        }
    }

    /// The thumbnail of `path`, at most `size` pixels on its longer side;
    /// asks for it when it is not known yet.
    pub fn get(&mut self, path: &Path, size: u32) -> Thumb {
        let key = (path.to_path_buf(), size);
        match self.cache.get(&key) {
            Some((Entry::Ready(t), _)) => return Thumb::Ready(t.clone()),
            Some((Entry::None, _)) => return Thumb::None,
            Some((Entry::Queued, _)) => return Thumb::Loading,
            None => {}
        }
        self.generation += 1;
        self.cache
            .insert(key.clone(), (Entry::Queued, self.generation));
        self.order.push_back((key.clone(), self.generation));
        let dropped = {
            let mut q = lock(&self.shared.queue);
            q.pending.push_back(key);
            let mut dropped = Vec::new();
            while q.pending.len() > MAX_QUEUED {
                if let Some(old) = q.pending.pop_front() {
                    dropped.push(old);
                }
            }
            dropped
        };
        for k in dropped {
            // Scrolled away before it was made: ask again when it is back.
            self.cache.remove(&k);
        }
        self.shared.wake.notify_one();
        Thumb::Loading
    }

    /// True while thumbnails are being made.
    pub fn busy(&self) -> bool {
        self.cache.values().any(|(e, _)| matches!(e, Entry::Queued))
    }
}

impl Drop for Thumbnails {
    fn drop(&mut self) {
        lock(&self.shared.queue).closed = true;
        self.shared.wake.notify_all();
    }
}

fn worker(shared: &Shared, ctx: &egui::Context) {
    loop {
        let key = {
            let mut q = lock(&shared.queue);
            loop {
                if q.closed {
                    return;
                }
                if let Some(k) = q.pending.pop_back() {
                    break k;
                }
                q = shared.wake.wait(q).unwrap_or_else(|p| p.into_inner());
            }
        };
        let img = diskrescue_app::results::thumbnail(&key.0, key.1).map(|rgba| {
            let size = [rgba.width() as usize, rgba.height() as usize];
            ColorImage::from_rgba_unmultiplied(size, rgba.as_raw())
        });
        lock(&shared.queue).done.push((key, img));
        ctx.request_repaint();
    }
}
