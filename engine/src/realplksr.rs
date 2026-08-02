//! Fixed-graph runtime for Philip Hofmann's 4xNomosWebPhoto RealPLKSR model.
//!
//! This deliberately is not a tensor framework. The loader accepts one exact
//! released checkpoint and the macOS implementation constructs only its
//! inference graph using the system MetalPerformanceShadersGraph framework.

use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_void};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, RgbImage};
use sha2::{Digest, Sha256};
use zip::ZipArchive;

const MODEL_SHA256: &str = "a9db66c9b674c6a5025b6ef3bee71a57c33b8605d8a2de0980470f89002efbbe";
const MODEL_FILE: &str = "4xNomosWebPhoto_RealPLKSR.pth";
const MODEL_URL: &str = "https://github.com/Phhofm/models/releases/download/4xNomosWebPhoto_RealPLKSR/4xNomosWebPhoto_RealPLKSR.pth";
const SCALE: u32 = 4;
const MAX_INPUT_PIXELS: u64 = 512 * 512;
const ENHANCED_CACHE_BYTES: usize = 128 * 1024 * 1024;

#[repr(C)]
struct Weight {
    data: *const u8,
    len: usize,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn sarun_realplksr_create(
        weights: *const Weight,
        count: usize,
        error: *mut c_char,
        error_len: usize,
    ) -> *mut c_void;
    fn sarun_realplksr_run(
        runtime: *mut c_void,
        input: *const f32,
        width: u32,
        height: u32,
        output: *mut f32,
        error: *mut c_char,
        error_len: usize,
    ) -> bool;
    fn sarun_realplksr_destroy(runtime: *mut c_void);
}

struct Native {
    ptr: *mut c_void,
}

unsafe impl Send for Native {}

impl Drop for Native {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        unsafe {
            sarun_realplksr_destroy(self.ptr);
        }
    }
}

/// A serialized fixed-model executor. MPSGraph owns one Metal command queue,
/// and the gateway intentionally runs one enhancement at a time.
pub struct Runtime {
    native: Mutex<Native>,
}

type CacheKey = String;

enum CacheEntry {
    Pending,
    Ready(Arc<Vec<u8>>),
    Failed,
}

struct Cache {
    entries: HashMap<CacheKey, CacheEntry>,
    ready_lru: VecDeque<CacheKey>,
    ready_bytes: usize,
    model_error: Option<String>,
}

struct Job {
    key: CacheKey,
    source: Vec<u8>,
}

pub enum Enhancement {
    /// The first request still receives the original image while Metal works.
    Pending,
    /// A later request can replace it directly from the bounded RAM cache.
    Ready(Arc<Vec<u8>>),
    /// This payload is not a small photographic raster or no model is present.
    Original,
}

/// One background Metal worker plus a 128 MiB process-lifetime image cache.
/// It never writes enhanced images to disk.
pub struct Enhancer {
    cache: Arc<Mutex<Cache>>,
    sender: Option<mpsc::Sender<Job>>,
    worker: Option<thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Enhancer {
    pub fn new() -> Self {
        let model = configured_model_path();
        if !model.is_file() {
            eprintln!(
                "sarun library: photo enhancement inactive; install the model with `sarun realplksr install`"
            );
            return Self {
                cache: Arc::new(Mutex::new(Cache {
                    entries: HashMap::new(),
                    ready_lru: VecDeque::new(),
                    ready_bytes: 0,
                    model_error: Some(format!(
                        "RealPLKSR model is not installed at {}",
                        model.display()
                    )),
                })),
                sender: None,
                worker: None,
                stop: Arc::new(AtomicBool::new(false)),
            };
        }
        let cache = Arc::new(Mutex::new(Cache {
            entries: HashMap::new(),
            ready_lru: VecDeque::new(),
            ready_bytes: 0,
            model_error: None,
        }));
        let (sender, receiver) = mpsc::channel::<Job>();
        let worker_cache = Arc::clone(&cache);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("realplksr".into())
            .spawn(move || {
                let runtime = match Runtime::open(&model) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        if let Ok(mut cache) = worker_cache.lock() {
                            cache.model_error = Some(error.clone());
                        }
                        eprintln!("sarun library: photo enhancement disabled: {error}");
                        return;
                    }
                };
                eprintln!(
                    "sarun library: RealPLKSR photo enhancement ready (128 MiB RAM cache)"
                );
                while let Ok(job) = receiver.recv() {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let result = runtime.enhance(&job.source);
                    let Ok(mut cache) = worker_cache.lock() else {
                        return;
                    };
                    match result {
                        Ok(bytes) => {
                            let bytes = Arc::new(bytes);
                            cache.ready_bytes += bytes.len();
                            cache
                                .entries
                                .insert(job.key.clone(), CacheEntry::Ready(Arc::clone(&bytes)));
                            cache.ready_lru.push_back(job.key);
                            while cache.ready_bytes > ENHANCED_CACHE_BYTES {
                                let Some(oldest) = cache.ready_lru.pop_front() else {
                                    break;
                                };
                                if let Some(CacheEntry::Ready(bytes)) =
                                    cache.entries.remove(&oldest)
                                {
                                    cache.ready_bytes =
                                        cache.ready_bytes.saturating_sub(bytes.len());
                                }
                            }
                        }
                        Err(error) => {
                            eprintln!("sarun library: photo enhancement skipped: {error}");
                            insert_failed(&mut cache, job.key);
                        }
                    }
                }
            })
            .ok();
        Self {
            cache,
            sender: worker.as_ref().map(|_| sender),
            worker,
            stop,
        }
    }

    pub fn image(&self, route: &str, mime: &str, source: &[u8]) -> Enhancement {
        if self.sender.is_none() || !eligible_photo_route(route, mime) {
            return Enhancement::Original;
        }
        let key = route.to_string();
        let mut cache = match self.cache.lock() {
            Ok(cache) => cache,
            Err(_) => return Enhancement::Original,
        };
        if cache.model_error.is_some() {
            return Enhancement::Original;
        }
        match cache.entries.get(&key) {
            Some(CacheEntry::Ready(bytes)) => {
                let bytes = Arc::clone(bytes);
                if let Some(position) = cache.ready_lru.iter().position(|entry| entry == &key) {
                    cache.ready_lru.remove(position);
                }
                cache.ready_lru.push_back(key);
                return Enhancement::Ready(bytes);
            }
            Some(CacheEntry::Pending) => return Enhancement::Pending,
            Some(CacheEntry::Failed) => return Enhancement::Original,
            None => {}
        }
        let dimensions_are_small = image::load_from_memory(source)
            .ok()
            .map(|image| {
                let pixels = u64::from(image.width()) * u64::from(image.height());
                pixels > 0
                    && pixels <= MAX_INPUT_PIXELS
                    && image.width() <= 512
                    && image.height() <= 512
            })
            .unwrap_or(false);
        if !dimensions_are_small {
            return Enhancement::Original;
        }
        cache.entries.insert(key.clone(), CacheEntry::Pending);
        drop(cache);
        let job = Job {
            key: key.clone(),
            source: source.to_vec(),
        };
        if self
            .sender
            .as_ref()
            .is_none_or(|sender| sender.send(job).is_err())
        {
            if let Ok(mut cache) = self.cache.lock() {
                insert_failed(&mut cache, key);
            }
            Enhancement::Original
        } else {
            Enhancement::Pending
        }
    }

    pub fn cached(&self, route: &str) -> Option<Arc<Vec<u8>>> {
        let mut cache = self.cache.lock().ok()?;
        let key = route.to_string();
        let CacheEntry::Ready(bytes) = cache.entries.get(&key)? else {
            return None;
        };
        let bytes = Arc::clone(bytes);
        if let Some(position) = cache.ready_lru.iter().position(|entry| entry == &key) {
            cache.ready_lru.remove(position);
        }
        cache.ready_lru.push_back(key);
        Some(bytes)
    }
}

impl Drop for Enhancer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn insert_failed(cache: &mut Cache, key: CacheKey) {
    const MAX_FAILURES: usize = 4096;
    let failures = cache
        .entries
        .values()
        .filter(|entry| matches!(entry, CacheEntry::Failed))
        .count();
    if failures >= MAX_FAILURES {
        if let Some(old) = cache.entries.iter().find_map(|(key, entry)| {
            matches!(entry, CacheEntry::Failed).then(|| key.clone())
        }) {
            cache.entries.remove(&old);
        }
    }
    cache.entries.insert(key, CacheEntry::Failed);
}

fn eligible_photo_route(route: &str, mime: &str) -> bool {
    let path = route.split('?').next().unwrap_or(route).to_ascii_lowercase();
    (path.ends_with(".jpg") || path.ends_with(".jpeg") || path.ends_with(".webp"))
        && (mime.starts_with("image/jpeg") || mime.starts_with("image/webp"))
}

impl Runtime {
    pub fn open(path: &Path) -> Result<Self, String> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = path;
            return Err("RealPLKSR enhancement is currently available on macOS".into());
        }
        #[cfg(target_os = "macos")]
        {
            let tensors = load_checkpoint(path)?;
            let weights = tensors
                .iter()
                .map(|bytes| Weight {
                    data: bytes.as_ptr(),
                    len: bytes.len(),
                })
                .collect::<Vec<_>>();
            let mut error = [0_i8; 512];
            let ptr = unsafe {
                sarun_realplksr_create(
                    weights.as_ptr(),
                    weights.len(),
                    error.as_mut_ptr(),
                    error.len(),
                )
            };
            if ptr.is_null() {
                return Err(ffi_error(&error));
            }
            Ok(Self {
                native: Mutex::new(Native { ptr }),
            })
        }
    }

    pub fn enhance(&self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        let image = image::load_from_memory(bytes)
            .map_err(|error| format!("decode source image: {error}"))?
            .to_rgb8();
        self.enhance_rgb(&image)
    }

    fn enhance_rgb(&self, image: &RgbImage) -> Result<Vec<u8>, String> {
        let (width, height) = image.dimensions();
        if width == 0 || height == 0 {
            return Err("source image is empty".into());
        }
        if u64::from(width) * u64::from(height) > MAX_INPUT_PIXELS {
            return Err(format!(
                "source image is {width}×{height}; the photo enhancer is limited to 512×512 pixels"
            ));
        }
        let input = image
            .as_raw()
            .iter()
            .map(|&value| f32::from(value) / 255.0)
            .collect::<Vec<_>>();
        let output_width = width
            .checked_mul(SCALE)
            .ok_or_else(|| "enhanced width overflow".to_string())?;
        let output_height = height
            .checked_mul(SCALE)
            .ok_or_else(|| "enhanced height overflow".to_string())?;
        let output_len = usize::try_from(output_width)
            .ok()
            .and_then(|w| usize::try_from(output_height).ok().and_then(|h| w.checked_mul(h)))
            .and_then(|pixels| pixels.checked_mul(3))
            .ok_or_else(|| "enhanced image is too large".to_string())?;
        let mut output = vec![0.0_f32; output_len];
        #[cfg(target_os = "macos")]
        {
            let native = self
                .native
                .lock()
                .map_err(|_| "RealPLKSR runtime lock is poisoned".to_string())?;
            let mut error = [0_i8; 512];
            if !unsafe {
                sarun_realplksr_run(
                    native.ptr,
                    input.as_ptr(),
                    width,
                    height,
                    output.as_mut_ptr(),
                    error.as_mut_ptr(),
                    error.len(),
                )
            } {
                return Err(ffi_error(&error));
            }
        }
        #[cfg(not(target_os = "macos"))]
        return Err("RealPLKSR enhancement is currently available on macOS".into());

        let pixels = output
            .into_iter()
            .map(|value| (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
            .collect::<Vec<_>>();
        let enhanced = RgbImage::from_raw(output_width, output_height, pixels)
            .ok_or_else(|| "Metal returned a malformed output image".to_string())?;
        let mut encoded = Vec::new();
        JpegEncoder::new_with_quality(&mut encoded, 90)
            .encode_image(&DynamicImage::ImageRgb8(enhanced))
            .map_err(|error| format!("encode enhanced image: {error}"))?;
        Ok(encoded)
    }
}

pub fn configured_model_path() -> PathBuf {
    std::env::var_os("SARUN_REALPLKSR_MODEL").map_or_else(
        || crate::paths::data_home().join("models").join(MODEL_FILE),
        PathBuf::from,
    )
}

fn load_checkpoint(path: &Path) -> Result<Vec<Vec<u8>>, String> {
    let file = std::fs::read(path)
        .map_err(|error| format!("read RealPLKSR checkpoint {}: {error}", path.display()))?;
    let digest = format!("{:x}", Sha256::digest(&file));
    if digest != MODEL_SHA256 {
        return Err(format!(
            "{} is not the supported 4xNomosWebPhoto checkpoint (SHA-256 {digest})",
            path.display()
        ));
    }
    let cursor = Cursor::new(file);
    let mut archive =
        ZipArchive::new(cursor).map_err(|error| format!("open PyTorch checkpoint: {error}"))?;
    let root = (0..archive.len())
        .find_map(|index| {
            let name = archive.by_index(index).ok()?.name().to_string();
            name.strip_suffix("data.pkl").map(str::to_string)
        })
        .ok_or_else(|| "checkpoint has no data.pkl".to_string())?;
    let byteorder = read_zip_entry(&mut archive, &format!("{root}byteorder"))?;
    if byteorder != b"little" {
        return Err("checkpoint tensors are not little-endian".into());
    }
    let lengths = expected_tensor_lengths();
    let mut tensors = Vec::with_capacity(lengths.len());
    for (index, expected) in lengths.into_iter().enumerate() {
        let bytes = read_zip_entry(&mut archive, &format!("{root}data/{index}"))?;
        if bytes.len() != expected {
            return Err(format!(
                "checkpoint tensor {index} is {} bytes; expected {expected}",
                bytes.len()
            ));
        }
        tensors.push(bytes);
    }
    Ok(tensors)
}

fn read_zip_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, String> {
    let mut entry = archive
        .by_name(name)
        .map_err(|error| format!("checkpoint entry {name}: {error}"))?;
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read checkpoint entry {name}: {error}"))?;
    Ok(bytes)
}

fn expected_tensor_lengths() -> Vec<usize> {
    fn bytes(shape: &[usize]) -> usize {
        shape.iter().product::<usize>() * std::mem::size_of::<f32>()
    }
    let mut lengths = vec![bytes(&[64, 3, 3, 3]), bytes(&[64])];
    for _ in 0..28 {
        for shape in [
            &[128, 64, 3, 3][..],
            &[128],
            &[64, 128, 3, 3],
            &[64],
            &[16, 16, 17, 17],
            &[16],
            &[64, 64, 3, 3],
            &[64],
            &[64, 64, 1, 1],
            &[64],
            &[64],
            &[64],
        ] {
            lengths.push(bytes(shape));
        }
    }
    lengths.extend([bytes(&[48, 64, 3, 3]), bytes(&[48])]);
    lengths
}

#[cfg(target_os = "macos")]
fn ffi_error(buffer: &[c_char]) -> String {
    let bytes = buffer
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub fn cli(args: &[String]) -> i32 {
    if args == ["install"] {
        return match install_model() {
            Ok(path) => {
                println!("installed RealPLKSR model at {}", path.display());
                0
            }
            Err(error) => {
                eprintln!("realplksr: {error}");
                1
            }
        };
    }
    if args == ["status"] {
        let path = configured_model_path();
        println!(
            "{} · {}",
            if path.is_file() {
                "installed"
            } else {
                "not installed"
            },
            path.display()
        );
        return i32::from(!path.is_file());
    }
    if args.len() != 3 {
        eprintln!(
            "usage: sarun realplksr install|status\n       sarun realplksr MODEL.pth INPUT_IMAGE OUTPUT.jpg"
        );
        return 2;
    }
    let input = match std::fs::read(&args[1]) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("realplksr: read {}: {error}", args[1]);
            return 1;
        }
    };
    let runtime = match Runtime::open(Path::new(&args[0])) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("realplksr: {error}");
            return 1;
        }
    };
    let output = match runtime.enhance(&input) {
        Ok(output) => output,
        Err(error) => {
            eprintln!("realplksr: {error}");
            return 1;
        }
    };
    if let Err(error) = std::fs::write(&args[2], output) {
        eprintln!("realplksr: write {}: {error}", args[2]);
        return 1;
    }
    0
}

fn install_model() -> Result<PathBuf, String> {
    let destination = configured_model_path();
    if destination.is_file() && load_checkpoint(&destination).is_ok() {
        return Ok(destination);
    }
    let parent = destination
        .parent()
        .ok_or_else(|| "model path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temporary = parent.join(format!(".{MODEL_FILE}.partial-{}", std::process::id()));
    let status = std::process::Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--show-error",
            "--output",
            temporary
                .to_str()
                .ok_or_else(|| "model path is not UTF-8".to_string())?,
            MODEL_URL,
        ])
        .status()
        .map_err(|error| format!("start curl: {error}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("model download failed ({status})"));
    }
    if let Err(error) = load_checkpoint(&temporary) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    std::fs::rename(&temporary, &destination)
        .map_err(|error| format!("install {}: {error}", destination.display()))?;
    let attribution = parent.join("4xNomosWebPhoto_RealPLKSR.txt");
    std::fs::write(
        &attribution,
        "4xNomosWebPhoto_RealPLKSR\n\
         Author: Philip Hofmann\n\
         Architecture: RealPLKSR by musl/neosr\n\
         License: CC-BY-4.0\n\
         Source: https://github.com/Phhofm/models/releases/tag/4xNomosWebPhoto_RealPLKSR\n",
    )
    .map_err(|error| format!("write {}: {error}", attribution.display()))?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_graph_has_exactly_340_tensors() {
        let lengths = expected_tensor_lengths();
        assert_eq!(lengths.len(), 340);
        assert_eq!(lengths.iter().sum::<usize>(), 29_558_720);
    }

    #[test]
    fn only_small_photo_routes_are_candidates() {
        assert!(eligible_photo_route(
            "/lvwiki/w/media/City.jpg?w=250",
            "image/webp"
        ));
        assert!(!eligible_photo_route(
            "/lvwiki/w/media/Flag.svg?w=250",
            "image/webp"
        ));
        assert!(!eligible_photo_route(
            "/lvwiki/w/media/Speech.ogg?w=orig",
            "audio/ogg"
        ));
    }
}
