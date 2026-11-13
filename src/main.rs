#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Cable Mixer: mixes ONE microphone + ONE program's audio into VB-Cable.
//! Apps then pick "CABLE Output" as their microphone.
//!
//! Needs: VB-Cable installed, Windows 11 (or a recent Windows 10) for per-app capture.

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;
use sysinfo::{Pid, ProcessesToUpdate, System};
use wasapi::*;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const RATE: usize = 48_000;
const CH: usize = 2;
/// Safety cap per source (drops oldest audio if the two clocks drift apart).
const MAX_QUEUE: usize = RATE * CH * 40 / 1000;
/// Gain smoothing per frame (prevents clicks when moving sliders / muting).
const GAIN_STEP: f32 = 0.0005;

// ───────────────────────── helpers ─────────────────────────

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct AtomicF32(AtomicU32);
impl AtomicF32 {
    fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

// ───────────────────────── settings ─────────────────────────

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
struct Settings {
    mic_id: String,
    mic_name: String,
    mic_gain: f32, // percent
    mic_mute: bool,
    app_name: String, // exe name, e.g. "spotify.exe"
    app_gain: f32,    // percent
    app_mute: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic_id: String::new(),
            mic_name: String::new(),
            mic_gain: 100.0,
            mic_mute: false,
            app_name: String::new(),
            app_gain: 100.0,
            app_mute: false,
        }
    }
}

fn settings_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("CableMixer").join("settings.json")
}

fn load_settings() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_settings(s: &Settings) {
    let p = settings_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(p, json);
    }
}

// ───────────────────────── shared audio state ─────────────────────────

struct Channel {
    queue: Mutex<VecDeque<f32>>,
    gain: AtomicF32, // 0.0..=2.0
    mute: AtomicBool,
    status: Mutex<String>,
}

impl Channel {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            gain: AtomicF32::new(1.0),
            mute: AtomicBool::new(false),
            status: Mutex::new(String::new()),
        }
    }
    fn push(&self, samples: &[f32]) {
        let mut q = lock(&self.queue);
        q.extend(samples.iter().copied());
        let excess = q.len().saturating_sub(MAX_QUEUE);
        if excess > 0 {
            q.drain(..excess);
        }
    }
    fn clear(&self) {
        lock(&self.queue).clear();
    }
    fn set_status(&self, s: impl Into<String>) {
        *lock(&self.status) = s.into();
    }
}

#[derive(Default, Clone)]
struct Lists {
    mics: Vec<(String, String)>, // (device id, name)
    procs: Vec<(String, u32)>,   // (exe name, root pid)
}

struct Shared {
    mic: Channel,
    app: Channel,
    out_status: Mutex<String>,
    lists: Mutex<Lists>,
    quit: AtomicBool,
}

// ───────────────────────── capture ─────────────────────────

enum Source {
    Mic(String),
    App(u32),
}

struct Capture {
    stop: Arc<AtomicBool>,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn start_capture(sh: Arc<Shared>, src: Source, is_mic: bool) -> Capture {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let _ = thread::Builder::new().name("capture".into()).spawn(move || {
        let _ = initialize_mta();
        let ch = if is_mic { &sh.mic } else { &sh.app };
        while !stop2.load(Ordering::Relaxed) && !sh.quit.load(Ordering::Relaxed) {
            match capture_loop(ch, &src, &stop2) {
                Ok(()) => break,
                Err(e) => {
                    ch.set_status(format!("waiting… ({e})"));
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
    });
    Capture { stop }
}

fn capture_loop(ch: &Channel, src: &Source, stop: &AtomicBool) -> Res<()> {
    let fmt = WaveFormat::new(32, 32, SampleType::Float, RATE, CH, None);
    let mut client = match src {
        Source::Mic(id) => {
            let en = DeviceEnumerator::new()?;
            en.get_device(id)?.get_iaudioclient()?
        }
        Source::App(pid) => AudioClient::new_application_loopback_client(*pid, true)?,
    };
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 0,
    };
    client.initialize_client(&fmt, Direction::Capture, mode, None)?;
    let h_event = client.set_get_eventhandle()?;
    let cap = client.get_audiocaptureclient()?;
    client.start_stream()?;
    ch.set_status("capturing");

    let mut bytes: VecDeque<u8> = VecDeque::new();
    let mut samples: Vec<f32> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        // Timeouts are normal for app capture when the app is silent.
        let _ = h_event.wait_for_event(100);
        loop {
            let n = cap.get_next_packet_size()?.unwrap_or(0);
            if n == 0 {
                break;
            }
            bytes.clear();
            cap.read_from_device_to_deque(&mut bytes)?;
            samples.clear();
            while bytes.len() >= 4 {
                let b = [
                    bytes.pop_front().unwrap(),
                    bytes.pop_front().unwrap(),
                    bytes.pop_front().unwrap(),
                    bytes.pop_front().unwrap(),
                ];
                samples.push(f32::from_le_bytes(b));
            }
            ch.push(&samples);
        }
    }
    client.stop_stream()?;
    Ok(())
}

// ───────────────────────── render (mix → CABLE Input) ─────────────────────────

fn render_thread(sh: Arc<Shared>) {
    let _ = initialize_mta();
    while !sh.quit.load(Ordering::Relaxed) {
        if let Err(e) = render_loop(&sh) {
            *lock(&sh.out_status) = format!("output error: {e} (retrying)");
            thread::sleep(Duration::from_secs(1));
        }
    }
}

fn render_loop(sh: &Shared) -> Res<()> {
    let en = DeviceEnumerator::new()?;
    let mut cable = None;
    for d in &en.get_device_collection(Direction::Render)? {
        let d = d?;
        if d.get_friendlyname()?.contains("CABLE Input") {
            cable = Some(d);
            break;
        }
    }
    let dev = cable.ok_or("VB-Cable not found (no \"CABLE Input\" playback device)")?;
    let mut client = dev.get_iaudioclient()?;
    let fmt = WaveFormat::new(32, 32, SampleType::Float, RATE, CH, None);
    let (def_time, _min) = client.get_device_period()?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: def_time,
    };
    client.initialize_client(&fmt, Direction::Render, mode, None)?;
    let h_event = client.set_get_eventhandle()?;
    let render = client.get_audiorenderclient()?;

    let mut out: VecDeque<u8> = VecDeque::new();
    let space = client.get_available_space_in_frames()? as usize;
    out.extend(std::iter::repeat(0u8).take(space * CH * 4));
    render.write_to_device_from_deque(space, &mut out, None)?;
    client.start_stream()?;
    *lock(&sh.out_status) = "running".into();

    let (mut mic_g, mut app_g) = (0.0f32, 0.0f32);
    while !sh.quit.load(Ordering::Relaxed) {
        h_event.wait_for_event(1000)?;
        let frames = client.get_available_space_in_frames()? as usize;
        if frames == 0 {
            continue;
        }
        let mic_t = if sh.mic.mute.load(Ordering::Relaxed) { 0.0 } else { sh.mic.gain.get() };
        let app_t = if sh.app.mute.load(Ordering::Relaxed) { 0.0 } else { sh.app.gain.get() };
        {
            let mut mq = lock(&sh.mic.queue);
            let mut aq = lock(&sh.app.queue);
            for _ in 0..frames {
                mic_g += (mic_t - mic_g).clamp(-GAIN_STEP, GAIN_STEP);
                app_g += (app_t - app_g).clamp(-GAIN_STEP, GAIN_STEP);
                for _ in 0..CH {
                    let m = mq.pop_front().unwrap_or(0.0);
                    let a = aq.pop_front().unwrap_or(0.0);
                    let v = (m * mic_g + a * app_g).clamp(-1.0, 1.0);
                    out.extend(v.to_le_bytes());
                }
            }
        }
        render.write_to_device_from_deque(frames, &mut out, None)?;
    }
    client.stop_stream()?;
    Ok(())
}

// ───────────────────────── device / process scanner ─────────────────────────
// COM work stays off the UI thread (winit owns that thread's COM apartment).

fn scanner_thread(sh: Arc<Shared>) {
    let _ = initialize_mta();
    let Ok(mut sys) = System::new() else {
        return;
    };
    while !sh.quit.load(Ordering::Relaxed) {
        if let Ok(lists) = scan_once(&mut sys) {
            *lock(&sh.lists) = lists;
        }
        for _ in 0..20 {
            if sh.quit.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn scan_once(sys: &mut System) -> Res<Lists> {
    let en = DeviceEnumerator::new()?;

    let mut mics = Vec::new();
    for d in &en.get_device_collection(Direction::Capture)? {
        let d = d?;
        let name = d.get_friendlyname()?;
        if name.contains("CABLE") {
            continue; // never offer the virtual cable as the mic (feedback loop)
        }
        mics.push((d.get_id()?, name));
    }

    let me = std::process::id();
    let mut pids = BTreeSet::new();
    for d in &en.get_device_collection(Direction::Render)? {
        let d = d?;
        let Ok(mgr) = d.get_iaudiosessionmanager() else { continue };
        let Ok(sessions) = mgr.get_audiosessionenumerator() else { continue };
        let Ok(count) = sessions.get_count() else { continue };
        for i in 0..count {
            if let Ok(s) = sessions.get_session(i) {
                if let Ok(pid) = s.get_process_id() {
                    if pid != 0 && pid != me {
                        pids.insert(pid);
                    }
                }
            }
        }
    }

    sys.refresh_processes(ProcessesToUpdate::All, true);
    let mut procs: BTreeMap<String, u32> = BTreeMap::new();
    for pid in pids {
        let Some(p) = sys.process(Pid::from_u32(pid)) else { continue };
        let name = p.name().to_string_lossy().to_string();
        procs.entry(name).or_insert_with(|| root_pid(sys, pid));
    }

    Ok(Lists {
        mics,
        procs: procs.into_iter().collect(),
    })
}

/// Walk up to the top-most ancestor with the same exe name (e.g. the main
/// chrome.exe), so capturing its tree includes all child/audio processes.
fn root_pid(sys: &System, pid: u32) -> u32 {
    let mut cur = pid;
    for _ in 0..16 {
        let Some(p) = sys.process(Pid::from_u32(cur)) else { break };
        let Some(parent) = p.parent() else { break };
        let Some(pp) = sys.process(parent) else { break };
        if pp.name() == p.name() {
            cur = parent.as_u32();
        } else {
            break;
        }
    }
    cur
}

// ───────────────────────── UI ─────────────────────────

struct App {
    sh: Arc<Shared>,
    settings: Settings,
    saved: Settings,
    mic_cap: Option<(String, Capture)>,
    app_cap: Option<(u32, Capture)>,
}

impl App {
    fn new() -> Self {
        let settings = load_settings();
        let sh = Arc::new(Shared {
            mic: Channel::new(),
            app: Channel::new(),
            out_status: Mutex::new("starting…".into()),
            lists: Mutex::new(Lists::default()),
            quit: AtomicBool::new(false),
        });
        {
            let s = sh.clone();
            thread::Builder::new().name("scanner".into()).spawn(move || scanner_thread(s)).ok();
            let s = sh.clone();
            thread::Builder::new().name("render".into()).spawn(move || render_thread(s)).ok();
        }
        Self {
            sh,
            saved: settings.clone(),
            settings,
            mic_cap: None,
            app_cap: None,
        }
    }

    /// Start/stop/restart captures so they match the saved choices.
    fn reconcile(&mut self, lists: &Lists) {
        let want_mic = lists
            .mics
            .iter()
            .any(|m| m.0 == self.settings.mic_id)
            .then(|| self.settings.mic_id.clone());
        let have_mic = self.mic_cap.as_ref().map(|c| c.0.clone());
        if want_mic != have_mic {
            self.mic_cap = None;
            self.sh.mic.clear();
            self.sh.mic.set_status("");
            if let Some(id) = want_mic {
                let cap = start_capture(self.sh.clone(), Source::Mic(id.clone()), true);
                self.mic_cap = Some((id, cap));
            }
        }

        let want_app = lists
            .procs
            .iter()
            .find(|p| p.0 == self.settings.app_name)
            .map(|p| p.1);
        let have_app = self.app_cap.as_ref().map(|c| c.0);
        if want_app != have_app {
            self.app_cap = None;
            self.sh.app.clear();
            self.sh.app.set_status("");
            if let Some(pid) = want_app {
                let cap = start_capture(self.sh.clone(), Source::App(pid), false);
                self.app_cap = Some((pid, cap));
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.sh.quit.store(true, Ordering::Relaxed);
    }
}

const LIVE_GREEN: egui::Color32 = egui::Color32::from_rgb(34, 160, 70);
const MUTED_RED: egui::Color32 = egui::Color32::from_rgb(200, 45, 45);

fn section_heading(ui: &mut egui::Ui, title: &str, muted: bool) {
    let color = if muted { MUTED_RED } else { LIVE_GREEN };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("●").size(28.0).color(color));
        ui.heading(title);
    });
}

/// Big, VR-friendly volume slider + percentage + LIVE/MUTED toggle button.
fn volume_row(ui: &mut egui::Ui, gain: &mut f32, mute: &mut bool) {
    ui.horizontal(|ui| {
        let btn_w = 130.0;
        let pct_w = 64.0;
        let height = 54.0;
        let gaps = 3.0 * ui.spacing().item_spacing.x;
        ui.spacing_mut().slider_width = (ui.available_width() - btn_w - pct_w - gaps).max(80.0);
        ui.add(egui::Slider::new(gain, 0.0..=200.0).show_value(false).trailing_fill(true));
        ui.add_sized(
            [pct_w, height],
            egui::Label::new(egui::RichText::new(format!("{:.0}%", *gain)).size(22.0)),
        );
        let (text, color) = if *mute { ("MUTED", MUTED_RED) } else { ("LIVE", LIVE_GREEN) };
        let button = egui::Button::new(
            egui::RichText::new(text).size(24.0).strong().color(egui::Color32::WHITE),
        )
        .fill(color)
        .min_size(egui::vec2(btn_w, height));
        if ui.add(button).clicked() {
            *mute = !*mute;
        }
    });
}

fn status_line(ui: &mut egui::Ui, label: &str, text: &str) {
    if !text.is_empty() {
        ui.small(format!("{label}: {text}"));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_millis(500));

        let lists = lock(&self.sh.lists).clone();
        self.reconcile(&lists);

        let s = &mut self.settings;
        egui::CentralPanel::default().show(ui, |ui| {
            // ── Microphone ──
            section_heading(ui, "Microphone", s.mic_mute);
            let mic_label = if s.mic_id.is_empty() {
                "— none —".to_string()
            } else if let Some(m) = lists.mics.iter().find(|m| m.0 == s.mic_id) {
                m.1.clone()
            } else {
                format!("{} (not found)", s.mic_name)
            };
            egui::ComboBox::from_id_salt("mic")
                .width(340.0)
                .selected_text(mic_label)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(s.mic_id.is_empty(), "— none —").clicked() {
                        s.mic_id.clear();
                        s.mic_name.clear();
                    }
                    for (id, name) in &lists.mics {
                        if ui.selectable_label(s.mic_id == *id, name.as_str()).clicked() {
                            s.mic_id = id.clone();
                            s.mic_name = name.clone();
                        }
                    }
                });
            volume_row(ui, &mut s.mic_gain, &mut s.mic_mute);
            status_line(ui, "mic", &lock(&self.sh.mic.status));

            ui.add_space(14.0);
            ui.separator();
            ui.add_space(8.0);

            // ── Program audio ──
            section_heading(ui, "Program audio", s.app_mute);
            let running = lists.procs.iter().any(|p| p.0 == s.app_name);
            let app_label = if s.app_name.is_empty() {
                "— none —".to_string()
            } else if running {
                s.app_name.clone()
            } else {
                format!("{} (not running — will attach when it starts)", s.app_name)
            };
            egui::ComboBox::from_id_salt("app")
                .width(340.0)
                .selected_text(app_label)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(s.app_name.is_empty(), "— none —").clicked() {
                        s.app_name.clear();
                    }
                    for (name, _pid) in &lists.procs {
                        if ui.selectable_label(s.app_name == *name, name.as_str()).clicked() {
                            s.app_name = name.clone();
                        }
                    }
                });
            ui.small("Only apps that have played audio show up in this list.");
            volume_row(ui, &mut s.app_gain, &mut s.app_mute);
            status_line(ui, "program", &lock(&self.sh.app.status));

            ui.add_space(14.0);
            ui.separator();
            status_line(ui, "output → CABLE Input", &lock(&self.sh.out_status));
            ui.small("In your chat/recording app, choose \"CABLE Output\" as the microphone.");
        });

        // Apply to the audio engine.
        self.sh.mic.gain.set(self.settings.mic_gain / 100.0);
        self.sh.mic.mute.store(self.settings.mic_mute, Ordering::Relaxed);
        self.sh.app.gain.set(self.settings.app_gain / 100.0);
        self.sh.app.mute.store(self.settings.app_mute, Ordering::Relaxed);

        // Remember last choice (skip while a slider is being dragged).
        let dragging = ui.input(|i| i.pointer.any_down());
        if !dragging && self.settings != self.saved {
            save_settings(&self.settings);
            self.saved = self.settings.clone();
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([680.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native("Cable Mixer", options, Box::new(|cc| {
            cc.egui_ctx.set_zoom_factor(1.5); // Ctrl +/- also zooms at runtime
            cc.egui_ctx.all_styles_mut(|style| {
                for (_, font) in style.text_styles.iter_mut() {
                    font.size *= 1.2;
                }
                style.spacing.interact_size.y = 44.0;
                style.spacing.slider_rail_height = 16.0;
                style.spacing.item_spacing = egui::vec2(10.0, 10.0);
            });
            Ok(Box::new(App::new()))
        }))
}
