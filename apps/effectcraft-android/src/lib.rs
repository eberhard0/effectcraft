//! EffectCraft on Android.
//!
//! Runs the same [`effectcraft_ui_egui::EffectcraftApp`] as the desktop app inside a
//! `GameActivity` (android-activity's `game-activity` backend, which eframe needs for the soft
//! keyboard and accesskit). Built with `cargo ndk` into `android/app/src/main/jniLibs`, then
//! packaged by the Gradle project in `android/`.
//!
//! Differences from the desktop app:
//! - no TCP control server, no cpal audio output (previews play silently);
//! - File › Open / Import ask `MainActivity.pickOpen()` (Storage Access Framework); the bytes come
//!   back on a Java thread through `nativeDeliverFile` into an inbox, are copied into the app's
//!   private `Imports/` folder and opened or imported from there on the next frame, like the
//!   web build;
//! - projects save to the private `Projects/` folder (no dialog: the suggested name is the file
//!   name) and a copy goes to `Downloads/EffectCraft/<name>` through
//!   `MainActivity.saveToDownloads` (MediaStore); Render Queue outputs go there too;
//! - settings, shortcuts, auto-saves and the panel layout live in the app's private files
//!   directory.

#![cfg(target_os = "android")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use android_activity::AndroidApp;
use effectcraft_ui_egui::EffectcraftApp;
use jni::objects::{JByteArray, JObject, JString};
use jni::{Env, EnvUnowned, JavaVM, jni_sig, jni_str};
use serde_json::json;

const LOG_TAG: &str = "effectcraft";

/// Files the Kotlin side delivers (name, bytes); the app opens them on its next frame.
static INBOX: Mutex<Vec<(String, Vec<u8>)>> = Mutex::new(Vec::new());
/// The egui context, to wake the app when a file arrives from a Java thread.
static CTX: OnceLock<egui::Context> = OnceLock::new();
/// The process's Java VM (set once) and the current activity (a reference android-activity
/// owns, stored as an address; 0 = none).
static VM: OnceLock<JavaVM> = OnceLock::new();
static ACTIVITY: Mutex<usize> = Mutex::new(0);

/// The activity's entry point, called by android-activity's GameActivity glue on its own thread.
/// It returns when the activity is destroyed.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    static LOGGER: OnceLock<()> = OnceLock::new();
    LOGGER.get_or_init(|| {
        android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag(LOG_TAG));
        effectcraft_engine::logging::install_panic_hook();
    });
    // SAFETY: `vm_as_ptr` is the process's JavaVM, valid for the life of the process.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let _ = VM.set(vm);
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = app.activity_as_ptr() as usize;

    let data_dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    log::info!("EffectCraft {} starting; data in {}", env!("CARGO_PKG_VERSION"), data_dir.display());
    // The font menus list the platform fonts (/system/fonts): read their names while the window opens.
    effectcraft_engine::text::fonts::scan_system_in_background();
    let options = eframe::NativeOptions {
        android_app: Some(app),
        // eframe saves egui panel/window sizes here on exit.
        persistence_path: Some(data_dir.join("ui.ron")),
        wgpu_options: wgpu_options(),
        ..Default::default()
    };
    let result = eframe::run_native(
        "EffectCraft",
        options,
        Box::new(move |cc| {
            let _ = CTX.set(cc.egui_ctx.clone());
            match &cc.wgpu_render_state {
                Some(rs) => log::info!("graphics: {:?}", rs.adapter.get_info()),
                None => log::info!("graphics: no wgpu device"),
            }
            // This shell owns the shared device's handlers (as the desktop app does).
            let gpu_failures = effectcraft_ui_egui::gpu_failure::GpuFailureBridge::new(&cc.egui_ctx);
            if let Some(rs) = &cc.wgpu_render_state {
                let errors = gpu_failures.clone();
                rs.device.on_uncaptured_error(Arc::new(move |error| {
                    let message = format!("uncaptured GPU error: {error}");
                    if errors.report(&message, false) {
                        log::error!("{message}");
                    }
                }));
                let lost = gpu_failures.clone();
                rs.device.set_device_lost_callback(move |reason, message| {
                    lost.report(&format!("GPU device lost ({reason:?}): {message}"), true);
                });
            }
            let mut session = effectcraft_host::session();
            session.check_footage_on_open = true;
            session.autosave.background = true;
            // Render Queue outputs go to Downloads/EffectCraft through MediaStore instead of
            // the (private) file system.
            session.exporter = Some(Arc::new(effectcraft_host::FileExporter { sink: Some(export_sink()) }));
            // Settings, shortcut presets, Roto Brush models and the crash-recovery sentinel
            // live in the app's private files directory (there is no HOME on Android).
            let config = data_dir.join("config");
            session.models_dir = Some(config.join("models"));
            let plugins = config.join("Plug-ins");
            if plugins.is_dir() {
                let _ = session.execute("effect.plugins.load", json!({"folder": plugins.to_string_lossy()}));
            }
            session.config = Some(Arc::new(effectcraft_engine::config::DirConfig::new(config)));
            session.load_settings();
            let recovery = session.begin_recovery();
            let show_home = session.prefs.startup.show_home_on_launch;
            let mut app = EffectcraftApp::new(session);
            app.set_gpu_failure_bridge(gpu_failures);
            app.ui.start_screen = show_home;
            if let Some(r) = recovery {
                app.offer_recovery(r);
            }
            install_hooks(&mut app, &data_dir);
            log::info!("app created; the window shows after its first frame");
            Ok(Box::new(Android { app, data_dir, drawn: false }))
        }),
    );
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = 0;
    if let Err(e) = result {
        log::error!("EffectCraft stopped: {e}");
        // winit allows one event loop per process: when Android recreates the activity in the
        // same process, end the process so the next launch starts clean instead of a blank window.
        std::process::exit(0);
    }
}

/// The window's wgpu device asks for the adapter's own limits (textures up to 16384 px)
/// instead of eframe's fixed set, as the desktop app does.
fn wgpu_options() -> eframe::WgpuConfiguration {
    use eframe::egui_wgpu::WgpuSetup;
    let mut config = eframe::WgpuConfiguration::default();
    if let WgpuSetup::CreateNew(setup) = &mut config.wgpu_setup {
        setup.device_descriptor = Arc::new(|adapter| eframe::wgpu::DeviceDescriptor {
            label: Some("EffectCraft device"),
            required_limits: device_limits(adapter.limits()),
            ..Default::default()
        });
    }
    config
}

fn device_limits(adapter: eframe::wgpu::Limits) -> eframe::wgpu::Limits {
    eframe::wgpu::Limits { max_texture_dimension_2d: adapter.max_texture_dimension_2d.min(16384), ..adapter }
}

/// The platform pickers in place of the desktop's file dialogs.
fn install_hooks(app: &mut EffectcraftApp, data_dir: &Path) {
    // The picker is asynchronous: the files arrive later through the inbox.
    app.hooks.pick_files = Some(Box::new(|_exts: &[&str]| {
        if let Err(e) = pick_open() {
            log::error!("couldn't open the file picker: {e}");
        }
        Vec::new()
    }));
    app.hooks.pick_open_project = Some(Box::new(|| {
        if let Err(e) = pick_open() {
            log::error!("couldn't open the file picker: {e}");
        }
        None
    }));
    // No save dialog: the suggested name becomes the file name in the private Projects folder.
    let projects = data_dir.join("Projects");
    app.hooks.pick_save = Some(Box::new(move |name: &str| {
        let name = file_name(if name.is_empty() { "Untitled Project.ecproj" } else { name });
        let name = if name.to_ascii_lowercase().ends_with(".ecproj") { name } else { format!("{name}.ecproj") };
        Some(projects.join(name).to_string_lossy().to_string())
    }));
    // Other saved files (shortcut presets…): the private Exports folder.
    let exports = data_dir.join("Exports");
    app.hooks.pick_save_file = Some(Box::new(move |name: &str, ext: &str| {
        let name = file_name(if name.is_empty() { "untitled" } else { name });
        let name = if ext.is_empty() || name.to_ascii_lowercase().ends_with(&format!(".{}", ext.to_ascii_lowercase())) { name } else { format!("{name}.{ext}") };
        Some(exports.join(name).to_string_lossy().to_string())
    }));
}

/// Render Queue outputs: each finished file goes to Downloads/EffectCraft (called from the
/// render threads; the JNI call attaches them to the VM).
fn export_sink() -> Arc<effectcraft_export::Sink> {
    Arc::new(|path: &str, data: Vec<u8>| {
        let name = file_name(path);
        if let Err(e) = save_to_downloads(&name, &data) {
            log::error!("couldn't save {name} to Downloads: {e}");
        }
    })
}

/// The eframe app: the shared EffectCraft UI plus the Android host's per-frame duties.
struct Android {
    app: EffectcraftApp,
    data_dir: PathBuf,
    /// The first frame was drawn (logged once).
    drawn: bool,
}

impl Android {
    /// Picked files: copy them into the private Imports folder, then open the project or import
    /// the media (the engine reads footage from the file system).
    fn open_inbox(&mut self) {
        let files = std::mem::take(&mut *INBOX.lock().unwrap_or_else(PoisonError::into_inner));
        if files.is_empty() {
            return;
        }
        let dir = self.data_dir.join("Imports");
        let mut paths = Vec::new();
        for (name, bytes) in files {
            let path = unique_path(&dir, &file_name(&name));
            match write_atomic(&path, &bytes) {
                Ok(()) => paths.push(path.to_string_lossy().to_string()),
                Err(e) => self.app.ui.status = format!("Couldn't store {name}: {e}"),
            }
        }
        let (projects, media): (Vec<String>, Vec<String>) = paths.into_iter().partition(|p| p.to_ascii_lowercase().ends_with(".ecproj"));
        if let Some(p) = projects.last()
            && let Err(e) = self.app.session.execute("file.open", json!({"path": p}))
        {
            self.app.ui.status = e.to_string();
        }
        if !media.is_empty() {
            match self.app.session.execute("file.import", json!({"paths": media})) {
                Ok(r) => {
                    if let Some(errs) = r["errors"].as_array().filter(|e| !e.is_empty()) {
                        self.app.ui.status = errs.iter().filter_map(|e| e.as_str()).collect::<Vec<_>>().join("; ");
                    }
                }
                Err(e) => self.app.ui.status = e.to_string(),
            }
        }
    }
}

impl eframe::App for Android {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.open_inbox();
        self.app.logic(ctx, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
        if !self.drawn {
            self.drawn = true;
            log::info!("first frame drawn");
        }
    }

    fn on_exit(&mut self) {
        self.app.on_exit();
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.app.raw_input_hook(ctx, raw_input);
    }
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
}

/// `dir/name`, or `dir/name (2).ext`… when that file exists.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (2..10_000).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap_or(first)
}

/// Write `bytes` to `path` through a temporary file, so a crash mid-write keeps the old file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ---- Calls into MainActivity (Kotlin) ----------------------------------------------------------

/// Run `f` with a JNI environment on this thread and the current activity.
fn with_activity<T>(f: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> jni::errors::Result<T>) -> Result<T, String> {
    let vm = VM.get().ok_or("the Java VM is not available")?;
    let raw = *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner);
    if raw == 0 {
        return Err("the activity is not running".to_string());
    }
    let raw = raw as jni::sys::jobject;
    vm.attach_current_thread(|env| -> jni::errors::Result<T> {
        // SAFETY: the reference comes from android-activity's `activity_as_ptr`, which keeps it
        // valid while the activity runs (ACTIVITY is cleared when `run_native` returns). `Cast`
        // neither owns nor deletes it.
        let activity = unsafe { env.as_cast_raw::<JObject>(&raw)? };
        f(env, &activity)
    })
    .map_err(|e| e.to_string())
}

/// `MainActivity.pickOpen()`: show the system file picker; the result comes through the inbox.
fn pick_open() -> Result<(), String> {
    with_activity(|env, activity| {
        env.call_method(activity, jni_str!("pickOpen"), jni_sig!("()V"), &[])?;
        Ok(())
    })
}

/// `MainActivity.saveToDownloads(name, bytes)`: `null` on success, else the error message.
fn save_to_downloads(name: &str, bytes: &[u8]) -> Result<(), String> {
    with_activity(|env, activity| {
        let jname = JString::from_str(env, name)?;
        let jbytes = env.byte_array_from_slice(bytes)?;
        let ret = env
            .call_method(activity, jni_str!("saveToDownloads"), jni_sig!("(Ljava/lang/String;[B)Ljava/lang/String;"), &[(&jname).into(), (&jbytes).into()])?
            .l()?;
        if ret.is_null() {
            return Ok(Ok(()));
        }
        let message = env.cast_local::<JString>(ret)?;
        Ok(Err(message.to_string()))
    })?
}

// ---- Calls from MainActivity (Kotlin) ----------------------------------------------------------

/// `MainActivity.nativeDeliverFile(name, bytes)`: a picked file's contents, from a Java thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_iameberhard_effectcraft_MainActivity_nativeDeliverFile<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _this: JObject<'caller>,
    name: JString<'caller>,
    bytes: JByteArray<'caller>,
) {
    let outcome = unowned_env.with_env(|env| -> jni::errors::Result<()> {
        let name = name.to_string();
        let bytes = env.convert_byte_array(&bytes)?;
        log::info!("received {name} ({} bytes)", bytes.len());
        INBOX.lock().unwrap_or_else(PoisonError::into_inner).push((name, bytes));
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
        Ok(())
    });
    outcome.resolve::<jni::errors::LogErrorAndDefault>()
}
