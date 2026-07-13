//! bcclink — standalone netplay for Mega Man Battle Chip Challenge (US).
//!
//! An egui window over an mgba core, with SDL3 for audio + gamepads (the
//! same split Tango uses) and exactly one trick: the game's link-cable comm
//! library is replaced by a WebRTC data channel paired through tango's
//! matchmaking server (see [`bcclink::hooks`] / [`bcclink::link`] /
//! [`bcclink::net`]). There is no lobby and no autopilot. The UI is two
//! screens: a setup screen (pick ROM + save, start the game) and the game
//! screen, whose top bar holds the link code and connection status. Trade a
//! link code with your opponent, Connect, and walk to **PET → Transmit**
//! in-game when you want to battle. The connect screen waits until the
//! opponent is standing in theirs, exactly like two consoles waiting on a
//! real cable.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use bcclink::emu::{self, BCC_US_CRC32, SCREEN_H, SCREEN_W};
use bcclink::{audio, link, net};
use eframe::egui;
use tokio_util::sync::CancellationToken;

const DEFAULT_MATCHMAKING_ENDPOINT: &str = "wss://matchmaking.tango.n1gp.net";

const OK_COLOR: egui::Color32 = egui::Color32::from_rgb(0x40, 0xc0, 0x40);
const ERROR_COLOR: egui::Color32 = egui::Color32::from_rgb(0xd0, 0x60, 0x40);

#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(default)]
struct Config {
    rom_path: Option<PathBuf>,
    save_path: Option<PathBuf>,
    link_code: String,
    matchmaking_endpoint: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rom_path: None,
            save_path: None,
            link_code: String::new(),
            matchmaking_endpoint: DEFAULT_MATCHMAKING_ENDPOINT.to_owned(),
        }
    }
}

impl Config {
    fn path() -> Option<PathBuf> {
        let dirs = directories_next::ProjectDirs::from("", "", "bcclink")?;
        Some(dirs.config_dir().join("config.json"))
    }

    fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, serde_json::to_vec_pretty(self).unwrap()) {
            log::warn!("config save: {e}");
        }
    }
}

fn keyboard_mask(input: &egui::InputState) -> u32 {
    const MAP: [(egui::Key, u32); 10] = [
        (egui::Key::Z, 1 << 0),         // A
        (egui::Key::X, 1 << 1),         // B
        (egui::Key::Backspace, 1 << 2), // Select
        (egui::Key::Enter, 1 << 3),     // Start
        (egui::Key::ArrowRight, 1 << 4),
        (egui::Key::ArrowLeft, 1 << 5),
        (egui::Key::ArrowUp, 1 << 6),
        (egui::Key::ArrowDown, 1 << 7),
        (egui::Key::S, 1 << 8), // R
        (egui::Key::A, 1 << 9), // L
    ];
    MAP.iter()
        .filter(|(key, _)| input.key_down(*key))
        .fold(0, |mask, (_, bit)| mask | bit)
}

fn gamepad_bit(button: sdl3::gamepad::Button) -> Option<u32> {
    use sdl3::gamepad::Button;
    Some(match button {
        Button::South => 1 << 0,
        Button::East => 1 << 1,
        Button::Back => 1 << 2,
        Button::Start => 1 << 3,
        Button::DPadRight => 1 << 4,
        Button::DPadLeft => 1 << 5,
        Button::DPadUp => 1 << 6,
        Button::DPadDown => 1 << 7,
        Button::RightShoulder => 1 << 8,
        Button::LeftShoulder => 1 << 9,
        _ => return None,
    })
}

/// SDL3, initialized main-thread-only for audio + gamepads (no video — the
/// window is egui's).
struct Sdl {
    sdl: sdl3::Sdl,
    gamepads: sdl3::GamepadSubsystem,
    pump: sdl3::EventPump,
    open_gamepads: Vec<sdl3::gamepad::Gamepad>,
}

impl Sdl {
    fn init() -> anyhow::Result<Self> {
        // Per the SDL3 gamepad docs: needed on Windows so the joystick
        // subsystem polls without a video subsystem hooked into the
        // message loop.
        sdl3::hint::set("SDL_JOYSTICK_THREAD", "1");
        sdl3::hint::set(
            "SDL_AUDIO_DEVICE_SAMPLE_FRAMES",
            &audio::SAMPLES.to_string(),
        );
        sdl3::hint::set("SDL_APP_NAME", "bcclink");
        let sdl = sdl3::init().map_err(|e| anyhow::anyhow!("sdl3 init: {e}"))?;
        let gamepads = sdl
            .gamepad()
            .map_err(|e| anyhow::anyhow!("sdl3 gamepad: {e}"))?;
        let pump = sdl
            .event_pump()
            .map_err(|e| anyhow::anyhow!("sdl3 event pump: {e}"))?;
        Ok(Self {
            sdl,
            gamepads,
            pump,
            open_gamepads: Vec::new(),
        })
    }

    /// Drain SDL's event queue (gamepad hotplug + buttons), maintaining the
    /// held-buttons mask.
    fn pump_gamepads(&mut self, mask: &mut u32) {
        use sdl3::event::Event;
        while let Some(event) = self.pump.poll_event() {
            match event {
                Event::ControllerDeviceAdded { which, .. } => {
                    match self
                        .gamepads
                        .open(sdl3::sys::joystick::SDL_JoystickID(which))
                    {
                        Ok(gamepad) => self.open_gamepads.push(gamepad),
                        Err(e) => log::warn!("gamepad open: {e}"),
                    }
                }
                Event::ControllerButtonDown { button, .. } => {
                    if let Some(bit) = gamepad_bit(button) {
                        *mask |= bit;
                    }
                }
                Event::ControllerButtonUp { button, .. } => {
                    if let Some(bit) = gamepad_bit(button) {
                        *mask &= !bit;
                    }
                }
                _ => {}
            }
        }
    }
}

struct App {
    cfg: Config,
    rt: tokio::runtime::Runtime,
    sdl: Option<Sdl>,
    gamepad_mask: u32,

    link: Arc<link::Link>,
    emu: Option<emu::Emu>,
    _audio: Option<audio::Backend>,
    rom_crc32: Option<u32>,

    status: Arc<Mutex<net::Status>>,
    cancel: Option<CancellationToken>,

    screen: Option<egui::TextureHandle>,
    rgba: Vec<u8>,
    error: Option<String>,
    /// One-shot: focus the link code field when the game screen appears, so
    /// starting the game flows straight into typing the code.
    link_code_focus: bool,
}

impl App {
    fn new() -> anyhow::Result<Self> {
        let sdl = match Sdl::init() {
            Ok(sdl) => Some(sdl),
            Err(e) => {
                log::warn!("SDL unavailable, no audio/gamepads: {e}");
                None
            }
        };
        Ok(Self {
            cfg: Config::load(),
            rt: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?,
            sdl,
            gamepad_mask: 0,
            link: Arc::new(link::Link::new()),
            emu: None,
            _audio: None,
            rom_crc32: None,
            status: Arc::new(Mutex::new(net::Status::Idle)),
            cancel: None,
            screen: None,
            rgba: vec![0u8; (SCREEN_W * SCREEN_H * 4) as usize],
            error: None,
            link_code_focus: false,
        })
    }

    fn play(&mut self) {
        self.error = None;
        let result = (|| -> anyhow::Result<()> {
            let rom_path = self
                .cfg
                .rom_path
                .clone()
                .ok_or_else(|| anyhow::anyhow!("pick a ROM"))?;
            let save_path = self
                .cfg
                .save_path
                .clone()
                .ok_or_else(|| anyhow::anyhow!("pick a save"))?;
            let rom = std::fs::read(&rom_path)?;
            let crc = crc32fast::hash(&rom);
            if crc != BCC_US_CRC32 {
                anyhow::bail!(
                    "{} doesn't look like Battle Chip Challenge (US): crc32 {crc:08x}, expected {BCC_US_CRC32:08x}",
                    rom_path.display()
                );
            }
            let save_file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&save_path)?;
            let emu = emu::start(rom, save_file, self.link.clone())?;
            if let Some(sdl) = &self.sdl {
                match audio::Backend::new(&sdl.sdl, emu.handle.clone()) {
                    Ok(backend) => self._audio = Some(backend),
                    // Audio paces emulation; without it the core would hang
                    // at the sync high-water. Treat as fatal.
                    Err(e) => anyhow::bail!("audio: {e}"),
                }
            } else {
                anyhow::bail!("SDL audio unavailable; can't run");
            }
            self.rom_crc32 = Some(crc);
            self.emu = Some(emu);
            Ok(())
        })();
        if let Err(e) = result {
            self._audio = None;
            self.error = Some(e.to_string());
        } else {
            self.link_code_focus = true;
            self.cfg.save();
        }
    }

    /// Back to the setup screen: drop the connection, then the audio backend
    /// (its callbacks pull from the emulator), then the emulator itself
    /// (dropping it ends and joins the mgba thread; the save file is already
    /// written through).
    fn stop(&mut self) {
        self.disconnect();
        self._audio = None;
        self.emu = None;
        self.screen = None;
        self.rom_crc32 = None;
    }

    fn connect(&mut self) {
        let Some(crc) = self.rom_crc32 else { return };
        self.disconnect();
        self.error = None;
        let cancel = CancellationToken::new();
        net::spawn_connect(
            self.rt.handle(),
            net::ConnectParams {
                endpoint: self.cfg.matchmaking_endpoint.clone(),
                link_code: self.cfg.link_code.clone(),
                rom_crc32: crc,
            },
            self.link.clone(),
            self.status.clone(),
            cancel.clone(),
        );
        self.cancel = Some(cancel);
        self.cfg.save();
    }

    fn disconnect(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
            // Make an in-battle game back out through its own comm-error
            // path rather than wait forever at a barrier.
            self.link.set_error();
            *self.status.lock().unwrap() = net::Status::Idle;
        }
    }

    fn update_screen(&mut self, ctx: &egui::Context) {
        let Some(emu) = &self.emu else { return };
        if !emu.dirty.swap(false, Ordering::Acquire) && self.screen.is_some() {
            return;
        }
        {
            let vbuf = emu.vbuf.lock().unwrap();
            for (dst, src) in self.rgba.chunks_exact_mut(4).zip(vbuf.chunks_exact(2)) {
                let v = u16::from_le_bytes([src[0], src[1]]);
                dst[0] = ((v & 0x1f) << 3) as u8;
                dst[1] = (((v >> 5) & 0x1f) << 3) as u8;
                dst[2] = (((v >> 10) & 0x1f) << 3) as u8;
                dst[3] = 0xff;
            }
        }
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [SCREEN_W as usize, SCREEN_H as usize],
            &self.rgba,
        );
        match &mut self.screen {
            Some(texture) => texture.set(image, egui::TextureOptions::NEAREST),
            None => {
                self.screen = Some(ctx.load_texture("screen", image, egui::TextureOptions::NEAREST))
            }
        }
    }

    /// The pre-game screen: pick files, start. Everything else lives on the
    /// game screen's top bar.
    fn setup_ui(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.heading("bcclink");
            ui.label("link play for Mega Man Battle Chip Challenge (US)");
            ui.add_space(16.0);

            egui::Grid::new("setup")
                .num_columns(2)
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    let file_button = |ui: &mut egui::Ui, path: &Option<PathBuf>, empty: &str| {
                        let label = path
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| empty.to_owned());
                        ui.add(egui::Button::new(label).min_size(egui::vec2(240.0, 0.0)))
                    };

                    ui.label("ROM");
                    if file_button(ui, &self.cfg.rom_path, "choose the US ROM…")
                        .on_hover_text("Battle Chip Challenge (US)")
                        .clicked()
                    {
                        self.pick_rom();
                    }
                    ui.end_row();

                    ui.label("Save");
                    if file_button(ui, &self.cfg.save_path, "choose a save…")
                        .on_hover_text("created if it doesn't exist yet")
                        .clicked()
                    {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("GBA save", &["sav"])
                            .set_title("Save file (created if missing)")
                            .set_file_name(
                                self.cfg
                                    .save_path
                                    .as_ref()
                                    .and_then(|p| p.file_name())
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "bcc.sav".to_owned()),
                            )
                            .save_file()
                        {
                            self.cfg.save_path = Some(path);
                        }
                    }
                    ui.end_row();
                });

            ui.add_space(12.0);
            let can_play = self.cfg.rom_path.is_some() && self.cfg.save_path.is_some();
            if ui
                .add_enabled(
                    can_play,
                    egui::Button::new("▶  Start game").min_size(egui::vec2(160.0, 32.0)),
                )
                .clicked()
            {
                self.play();
            }
            if !can_play {
                ui.add_space(4.0);
                ui.weak("pick a ROM to begin — the save defaults to sit next to it");
            }

            if let Some(error) = &self.error {
                ui.add_space(8.0);
                ui.colored_label(ERROR_COLOR, error);
            }

            ui.add_space(24.0);
            ui.collapsing("Advanced", |ui| {
                ui.horizontal(|ui| {
                    ui.label("Matchmaking server:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.matchmaking_endpoint)
                            .desired_width(280.0),
                    );
                    if ui.button("reset").clicked() {
                        self.cfg.matchmaking_endpoint = DEFAULT_MATCHMAKING_ENDPOINT.to_owned();
                    }
                });
            });
            ui.add_space(8.0);
            ui.weak(
                "Keys: arrows = D-pad, Z/X = A/B, A/S = L/R, Enter = Start, Backspace = Select",
            );
        });
    }

    fn pick_rom(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("GBA ROM", &["gba"])
            .set_title("Battle Chip Challenge (US) ROM")
            .pick_file()
        else {
            return;
        };
        // Validate right here so a wrong pick is flagged at the picker, not
        // at Start.
        match std::fs::read(&path) {
            Ok(rom) if crc32fast::hash(&rom) == BCC_US_CRC32 => {
                // Default the save next to the ROM so the common case is one
                // click.
                if self.cfg.save_path.is_none() {
                    self.cfg.save_path = Some(path.with_extension("sav"));
                }
                self.cfg.rom_path = Some(path);
                self.error = None;
            }
            Ok(_) => {
                self.error = Some(format!(
                    "{} doesn't look like Battle Chip Challenge (US)",
                    path.display()
                ));
            }
            Err(e) => self.error = Some(format!("{}: {e}", path.display())),
        }
    }

    /// The game screen's top bar: link code, connection status, stop.
    fn session_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Link code:");
            let code_edit = ui.add(
                egui::TextEdit::singleline(&mut self.cfg.link_code)
                    .hint_text("make one up, share it")
                    .desired_width(160.0),
            );
            if std::mem::take(&mut self.link_code_focus) {
                code_edit.request_focus();
            }
            let status = self.status.lock().unwrap().clone();
            let connecting = self.cancel.is_some()
                && !matches!(status, net::Status::Idle | net::Status::Lost(_));
            if connecting {
                if ui.button("Disconnect").clicked() {
                    self.disconnect();
                }
            } else {
                let can_connect = !self.cfg.link_code.trim().is_empty();
                let entered =
                    code_edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui
                    .add_enabled(can_connect, egui::Button::new("Connect"))
                    .clicked()
                    || (can_connect && entered)
                {
                    self.connect();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⏹ Stop").on_hover_text("back to setup").clicked() {
                    self.stop();
                    return;
                }
                ui.separator();
                ui.with_layout(
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| match status {
                        net::Status::Idle => {
                            ui.weak("enter the code you agreed with your opponent, then Connect");
                        }
                        net::Status::Signaling => {
                            ui.spinner();
                            ui.weak("contacting matchmaking server…");
                        }
                        net::Status::WaitingForPeer => {
                            ui.spinner();
                            ui.weak("waiting for your opponent to enter the same code…");
                        }
                        net::Status::Connected { side } => {
                            ui.colored_label(
                                OK_COLOR,
                                format!(
                                    "linked — you are {} — go to PET -> Transmit in-game",
                                    if side == 0 { "P1" } else { "P2" }
                                ),
                            );
                        }
                        net::Status::Lost(reason) => {
                            ui.colored_label(ERROR_COLOR, reason);
                        }
                    },
                );
            });
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut gamepad_mask = self.gamepad_mask;
        if let Some(sdl) = &mut self.sdl {
            sdl.pump_gamepads(&mut gamepad_mask);
        }
        self.gamepad_mask = gamepad_mask;

        // Keys go to the game unless egui wants them (a focused text field).
        let kb_mask = if ctx.wants_keyboard_input() {
            0
        } else {
            ctx.input(keyboard_mask)
        };
        if let Some(emu) = &self.emu {
            emu.handle.set_keys(kb_mask | self.gamepad_mask);
            if emu.handle.has_crashed() {
                self.stop();
                self.error = Some("emulator crashed".to_owned());
            }
        }

        self.update_screen(ctx);

        if self.emu.is_none() {
            egui::CentralPanel::default().show(ctx, |ui| self.setup_ui(ui));
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }

        egui::TopBottomPanel::top("session").show(ctx, |ui| {
            ui.add_space(4.0);
            self.session_ui(ui);
            ui.add_space(4.0);
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                if let Some(texture) = &self.screen {
                    let avail = ui.available_size();
                    let scale = (avail.x / SCREEN_W as f32)
                        .min(avail.y / SCREEN_H as f32)
                        .floor()
                        .max(1.0);
                    let size = egui::vec2(SCREEN_W as f32 * scale, SCREEN_H as f32 * scale);
                    ui.add_space(((avail.y - size.y) / 2.0).max(0.0));
                    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        ui.image((texture.id(), size));
                    });
                }
            });

        // The emulator produces frames continuously; repaint at display rate
        // to show them.
        ctx.request_repaint();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.disconnect();
        self.cfg.save();
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let app = App::new()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("bcclink")
            .with_inner_size([SCREEN_W as f32 * 3.0 + 16.0, SCREEN_H as f32 * 3.0 + 110.0]),
        ..Default::default()
    };
    eframe::run_native("bcclink", options, Box::new(move |_cc| Ok(Box::new(app))))
        .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
