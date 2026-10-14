use std::collections::HashMap;
use std::os::fd::AsFd;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

use memmap2::MmapMut;
use rustix::fs::{MemfdFlags, memfd_create};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    protocol::{wl_output::WlOutput, wl_registry::WlRegistry},
};
use wayland_protocols_wlr::gamma_control::v1::client::{
    zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1,
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};

#[derive(Default)]
struct AppState {
    outputs: Vec<WlOutput>,
    gamma_manager: Option<ZwlrGammaControlManagerV1>,
    gamma_controls: Vec<ZwlrGammaControlV1>,
    gamma_sizes: HashMap<ZwlrGammaControlV1, u32>,
}

impl Dispatch<WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wayland_client::protocol::wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_registry::Event::Global { name, interface, .. } = event
        {
            if interface == "wl_output" {
                state.outputs.push(registry.bind::<WlOutput, _, _>(name, 1, qh, ()));
            } else if interface == "zwlr_gamma_control_manager_v1" {
                state.gamma_manager =
                    Some(registry.bind::<ZwlrGammaControlManagerV1, _, _>(name, 1, qh, ()));
            }
        }
    }
}

impl Dispatch<WlOutput, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: <WlOutput as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &ZwlrGammaControlManagerV1,
        _: <ZwlrGammaControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrGammaControlV1, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrGammaControlV1,
        event: <ZwlrGammaControlV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => {
                state.gamma_sizes.insert(proxy.clone(), size);
            }
            zwlr_gamma_control_v1::Event::Failed => {
                log::warn!("wl_output rejected gamma control; removing from state.");
                state.gamma_sizes.remove(proxy);
            }
            _ => {}
        }
    }
}

/// Default timeout for Wayland thread initialization.
pub(crate) const NIGHT_MODE_INIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(1000);

/// Manages a background worker that applies color temperatures for night mode.
///
/// Two backends are supported, chosen automatically in [`NightModeManager::new`]:
/// - `wlr-gamma-control` (Sway, Hyprland, River, other wlroots compositors)
/// - KDE Plasma Night Light (KWin does not implement `wlr-gamma-control`)
pub(crate) struct NightModeManager {
    sender: Option<mpsc::Sender<f64>>,
    worker_handle: Option<thread::JoinHandle<()>>,
}

impl NightModeManager {
    /// Initializes a new NightModeManager. Tries the `wlr-gamma-control` backend
    /// first and falls back to the KDE backend when running under Plasma.
    pub(crate) fn new() -> Result<Self, Box<dyn std::error::Error>> {
        match Self::new_wlr() {
            Ok(manager) => Ok(manager),
            Err(wlr_err) => {
                if kde_backend_available() {
                    log::info!(
                        "wlr-gamma-control unavailable ({wlr_err}); using KDE Night Light backend"
                    );
                    Ok(Self::new_kde())
                } else {
                    Err(wlr_err)
                }
            }
        }
    }

    /// Spawns a background thread to lock and manipulate the `wlr_gamma_control_v1`
    /// Wayland protocol outputs.
    fn new_wlr() -> Result<Self, Box<dyn std::error::Error>> {
        let (tx, rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::channel();

        // Initial temperature
        let initial_temp = 6500.0;
        let current_temp =
            std::sync::Arc::new(std::sync::atomic::AtomicU32::new(initial_temp as u32));
        let thread_temp = std::sync::Arc::clone(&current_temp);

        let handle = thread::spawn(move || {
            if let Err(e) = run_wayland_thread(rx, init_tx, initial_temp, thread_temp) {
                log::error!("Night mode wayland thread exited: {}", e);
            }
        });

        // Block on initialization status from Wayland thread
        match init_rx.recv_timeout(NIGHT_MODE_INIT_TIMEOUT) {
            Ok(Ok(())) => Ok(NightModeManager { sender: Some(tx), worker_handle: Some(handle) }),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(format!("Night mode wayland thread initialization failed: {}", e).into()),
        }
    }

    /// Spawns the KDE worker thread. It only touches KWin's config when a
    /// temperature is actually requested, never at startup.
    fn new_kde() -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || run_kde_thread(rx));
        NightModeManager { sender: Some(tx), worker_handle: Some(handle) }
    }

    /// Sends a new color temperature (in Kelvin) down the channel to the worker thread.
    pub(crate) fn set_temperature(&self, temp: f64) -> Result<(), mpsc::SendError<f64>> {
        if let Some(tx) = &self.sender {
            match tx.send(temp) {
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            }
        } else {
            Err(mpsc::SendError(temp))
        }
    }
}

impl Drop for NightModeManager {
    fn drop(&mut self) {
        let _ = self.sender.take();
        if let Some(handle) = self.worker_handle.take()
            && let Err(e) = handle.join()
        {
            log::error!("Failed to join night mode worker thread: {:?}", e);
        }
    }
}

/// Converts a given color temperature in Kelvin to RGB multipliers between 0.0 and 1.0.
/// Includes clamping to mathematically guard against invalid log functions.
fn color_temp_to_rgb(temp: f64) -> (f64, f64, f64) {
    // Clamp temperature to a sensible range (1000K to 40000K) to ensure
    // we never take the log of a non-positive number.
    let temp = temp.clamp(1000.0, 40000.0) / 100.0;

    let red = if temp <= 66.0 {
        255.0
    } else {
        let r = temp - 60.0;
        let r = 329.698727446 * (r.powf(-0.1332047592));
        r.clamp(0.0, 255.0)
    };

    let green = if temp <= 66.0 {
        let g = temp;
        let g = 99.4708025861 * g.ln() - 161.1195681661;
        g.clamp(0.0, 255.0)
    } else {
        let g = temp - 60.0;
        let g = 288.1221695283 * (g.powf(-0.0755148492));
        g.clamp(0.0, 255.0)
    };

    let blue = if temp >= 66.0 {
        255.0
    } else if temp <= 19.0 {
        0.0
    } else {
        let b = temp - 10.0;
        let b = 138.5177312231 * b.ln() - 305.0447927307;
        b.clamp(0.0, 255.0)
    };

    (red / 255.0, green / 255.0, blue / 255.0)
}

/// Runs the blocking event queue in the background. Listens for temperature changes
/// via MPSC and translates them into live Wayland gamma adjustments.
fn run_wayland_thread(
    rx: mpsc::Receiver<f64>,
    init_tx: mpsc::Sender<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    initial_temp: f64,
    current_temp: std::sync::Arc<std::sync::atomic::AtomicU32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(e.into()));
            return Err("Failed to connect to wayland".into());
        }
    };

    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let _registry = display.get_registry(&qh, ());
    let mut state = AppState::default();

    // Roundtrip to get globals
    if let Err(e) = event_queue.roundtrip(&mut state) {
        let _ = init_tx.send(Err(e.into()));
        return Err("Failed roundtrip".into());
    }

    let manager = match state.gamma_manager.as_ref() {
        Some(m) => m,
        None => {
            let _ = init_tx.send(Err("zwlr_gamma_control_manager_v1 not available".into()));
            return Err("zwlr_gamma_control_manager_v1 not available".into());
        }
    };

    for output in &state.outputs {
        let control = manager.get_gamma_control(output, &qh, ());
        state.gamma_controls.push(control);
    }

    // Roundtrip to get gamma sizes
    if let Err(e) = event_queue.roundtrip(&mut state) {
        let _ = init_tx.send(Err(e.into()));
        return Err("Failed roundtrip for sizes".into());
    }

    // Inform main thread that initialization succeeded.
    let _ = init_tx.send(Ok(()));

    // Apply initial temperature, or use an incoming slider update if one arrived during boot
    let applied_temp = match rx.recv_timeout(std::time::Duration::from_millis(50)) {
        Ok(temp) => temp,
        Err(_) => initial_temp,
    };

    // Keep File Descriptors for the applied gamma ramps alive for the compositor's lifetime
    let mut _active_gamma_files = Vec::new();
    match apply_gamma_ramps(&state, applied_temp) {
        Ok(files) => {
            let _ = event_queue.roundtrip(&mut state);
            _active_gamma_files = files;
            current_temp.store(applied_temp.round() as u32, std::sync::atomic::Ordering::SeqCst);
        }
        Err(e) => {
            log::warn!("Failed to apply initial gamma ramps (temp: {}): {}", applied_temp, e);
        }
    }

    loop {
        // We do a brief dispatch to handle ping/pongs but mainly block on rx with a timeout.
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(temp) => match apply_gamma_ramps(&state, temp) {
                Ok(files) => {
                    let _ = event_queue.roundtrip(&mut state);
                    _active_gamma_files = files;
                    current_temp.store(temp.round() as u32, std::sync::atomic::Ordering::SeqCst);
                }
                Err(e) => {
                    log::warn!("Failed to apply gamma ramps: {}", e);
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Keep connection alive, dispatch any pending events from compositor
                let _ = event_queue.dispatch_pending(&mut state);
                let _ = event_queue.flush();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break; // UI dropped the sender, exit thread
            }
        }
    }

    // Explicitly destroy all gamma controls before the thread dies
    // This hands the lock back to the compositor so we don't permanently brick Night Mode
    for control in &state.gamma_controls {
        control.destroy();
    }

    // Flush the destroy commands to the Wayland socket immediately
    let _ = event_queue.roundtrip(&mut state);

    Ok(())
}

/// Manually maps RGB mathematical structures into a shared `memfd`,
/// attaching the resulting `File` bytes directly to the Wayland output gamma controller.
#[allow(unsafe_code)]
fn apply_gamma_ramps(
    state: &AppState,
    temp: f64,
) -> Result<Vec<std::fs::File>, Box<dyn std::error::Error>> {
    let (r_mult, g_mult, b_mult) = color_temp_to_rgb(temp);
    let mut new_files = Vec::new();

    for control in &state.gamma_controls {
        let size = *state.gamma_sizes.get(control).unwrap_or(&0) as usize;
        if size == 0 {
            continue;
        }

        let bytes_per_ramp = size * 2;
        let total_bytes = bytes_per_ramp * 3;

        let fd = memfd_create("gamma_ramp", MemfdFlags::CLOEXEC)?;
        let file = std::fs::File::from(fd);
        file.set_len(total_bytes as u64)?;

        let mut mmap = unsafe { MmapMut::map_mut(&file)? };

        for i in 0..size {
            let progress = if size == 1 { 1.0 } else { i as f64 / (size - 1) as f64 };
            let val = (progress * 65535.0) as u16;

            let r = (val as f64 * r_mult) as u16;
            let g = (val as f64 * g_mult) as u16;
            let b = (val as f64 * b_mult) as u16;

            let r_bytes = r.to_ne_bytes();
            let g_bytes = g.to_ne_bytes();
            let b_bytes = b.to_ne_bytes();

            mmap[i * 2] = r_bytes[0];
            mmap[i * 2 + 1] = r_bytes[1];

            mmap[bytes_per_ramp + i * 2] = g_bytes[0];
            mmap[bytes_per_ramp + i * 2 + 1] = g_bytes[1];

            mmap[bytes_per_ramp * 2 + i * 2] = b_bytes[0];
            mmap[bytes_per_ramp * 2 + i * 2 + 1] = b_bytes[1];
        }

        mmap.flush()?;
        control.set_gamma(file.as_fd());
        new_files.push(file);
    }

    Ok(new_files)
}

// ── KDE Plasma backend ─────────────────────────────────────────────────────
//
// KWin does not implement `wlr-gamma-control`, and its D-Bus NightLight
// interface is read-only apart from a temporary `preview`. The reliable way to
// drive Night Light is to write the [NightColor] group of `kwinrc` and notify
// KWin with `kwriteconfig6 --notify`.
//
// Mapping used by this backend:
//   temperature >= 6500K  -> Active=false (neutral; NightTemperature untouched)
//   temperature <  6500K  -> NightTemperature=<K> then Active=true
//
// `Mode` is deliberately left alone. The slider only has a visible effect
// all day with `Mode=Constant`; with a schedule, KDE applies the temperature
// only during the scheduled night period.

/// Neutral temperature: at or above this, Night Light is switched off.
const KDE_NEUTRAL_KELVIN: u32 = 6500;
/// Lowest temperature KDE accepts.
const KDE_MIN_KELVIN: u32 = 1000;

/// True when running under KDE and `kwriteconfig6` can be executed.
fn kde_backend_available() -> bool {
    let is_kde = std::env::var("XDG_CURRENT_DESKTOP")
        .map(|v| v.split(':').any(|part| part.eq_ignore_ascii_case("KDE")))
        .unwrap_or(false);
    if !is_kde {
        return false;
    }
    Command::new("kwriteconfig6")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Writes one key of the `[NightColor]` group in `kwinrc` and notifies KWin.
fn kde_write(key: &str, value: &str) -> bool {
    match Command::new("kwriteconfig6")
        .args(["--file", "kwinrc", "--group", "NightColor", "--key", key, value, "--notify"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => true,
        Ok(status) => {
            log::warn!("kwriteconfig6 failed for NightColor/{key}={value}: {status}");
            false
        }
        Err(e) => {
            log::warn!("Failed to run kwriteconfig6 for NightColor/{key}={value}: {e}");
            false
        }
    }
}

/// Applies one requested temperature. `last_*` cache what this thread last
/// wrote, to avoid spawning needless processes while the slider is dragged.
fn apply_kde_temperature(temp: f64, last_active: &mut Option<bool>, last_temp: &mut Option<u32>) {
    let kelvin =
        (temp.round() as i64).clamp(KDE_MIN_KELVIN as i64, KDE_NEUTRAL_KELVIN as i64) as u32;

    if kelvin >= KDE_NEUTRAL_KELVIN {
        if *last_active != Some(false) && kde_write("Active", "false") {
            *last_active = Some(false);
        }
        return;
    }

    // Set the temperature first so enabling never flashes the previous value.
    if *last_temp != Some(kelvin) {
        if !kde_write("NightTemperature", &kelvin.to_string()) {
            return;
        }
        *last_temp = Some(kelvin);
    }
    if *last_active != Some(true) && kde_write("Active", "true") {
        *last_active = Some(true);
    }
}

/// Worker loop: waits for a request, collapses any requests that piled up
/// while the previous write was running, then applies only the latest one.
/// This gives live feedback while dragging without flooding kwriteconfig6.
fn run_kde_thread(rx: mpsc::Receiver<f64>) {
    let mut last_active: Option<bool> = None;
    let mut last_temp: Option<u32> = None;

    while let Ok(mut temp) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            temp = newer;
        }
        apply_kde_temperature(temp, &mut last_active, &mut last_temp);
    }
}
