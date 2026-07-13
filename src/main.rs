//! Ring — standalone netplay for Mega Man Battle Chip Challenge (US) and
//! its JP original, Rockman EXE Battle Chip GP.
//!
//! An iced window over an mgba core, with SDL3 for audio + gamepads (the
//! same split Tango uses) and exactly one trick: the game's link-cable comm
//! library is replaced by a WebRTC data channel paired through tango's
//! matchmaking server (see [`ring_bcc::hooks`] / [`ring_bcc::link`] /
//! [`ring_bcc::net`]). There is no lobby and no autopilot. The UI is two
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

use ring_bcc::emu::{self, SCREEN_H, SCREEN_W};
use ring_bcc::{audio, link, net};
use iced::widget::{button, column, container, image, row, text, text_input};
use iced::{Element, Length, Subscription, Task};
use tokio_util::sync::CancellationToken;

const DEFAULT_MATCHMAKING_ENDPOINT: &str = "wss://matchmaking.tango.n1gp.net";

// Design tokens, matching tango's dark theme so the two apps feel related.
const BG: iced::Color = iced::Color::from_rgb(0.055, 0.063, 0.067); // #0e1011
const SURFACE: iced::Color = iced::Color::from_rgb(0.086, 0.098, 0.106); // #16191b
const EDGE: iced::Color = iced::Color::from_rgb(0.149, 0.161, 0.173); // #26292c
const TEXT: iced::Color = iced::Color::from_rgb(0.925, 0.933, 0.929); // #eceeed
const ACCENT: iced::Color = iced::Color::from_rgb(0.298, 0.686, 0.314); // #4caf50
const WARNING: iced::Color = iced::Color::from_rgb(1.0, 0.710, 0.278); // #ffb547
const DANGER: iced::Color = iced::Color::from_rgb(1.0, 0.322, 0.322); // #ff5252
const WEAK: iced::Color = iced::Color::from_rgb(0.604, 0.627, 0.651); // #9aa0a6

const TEXT_TITLE: f32 = 22.0;
const TEXT_BODY: f32 = 13.0;
const TEXT_CAPTION: f32 = 11.0;

fn theme(_app: &App) -> iced::Theme {
    iced::Theme::custom(
        "Ring".to_owned(),
        iced::theme::Palette {
            background: BG,
            text: TEXT,
            primary: ACCENT,
            success: ACCENT,
            warning: WARNING,
            danger: DANGER,
        },
    )
}

/// The setup card and the game screen's top bar share this raised-surface
/// look.
fn surface(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(SURFACE.into()),
        border: iced::Border {
            color: EDGE,
            width: 1.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    }
}

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
        let dirs = directories_next::ProjectDirs::from("", "", "ring")?;
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

/// GBA joyflag bit for a physical key, or `None` if unbound. Physical
/// codes (layout-independent), so the mapping stays put on non-QWERTY.
fn key_bit(code: iced::keyboard::key::Code) -> Option<u32> {
    use iced::keyboard::key::Code;
    Some(match code {
        Code::KeyZ => 1 << 0,         // A
        Code::KeyX => 1 << 1,         // B
        Code::Backspace => 1 << 2,    // Select
        Code::Enter => 1 << 3,        // Start
        Code::ArrowRight => 1 << 4,
        Code::ArrowLeft => 1 << 5,
        Code::ArrowUp => 1 << 6,
        Code::ArrowDown => 1 << 7,
        Code::KeyS => 1 << 8, // R
        Code::KeyA => 1 << 9, // L
        _ => return None,
    })
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
/// window is iced's).
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
        sdl3::hint::set("SDL_APP_NAME", "Ring");
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

#[derive(Debug, Clone)]
enum Message {
    /// The emulator finished a frame (via the vblank notify stream): pump
    /// gamepads, push held keys, refresh the screen texture.
    Frame,
    PickRom,
    RomPicked(Option<PathBuf>),
    PickSave,
    SavePicked(Option<PathBuf>),
    Play,
    Stop,
    LinkCodeChanged(String),
    EndpointChanged(String),
    EndpointReset,
    ToggleAdvanced,
    Connect,
    Disconnect,
    KeyDown(u32),
    KeyUp(u32),
    CloseRequested(iced::window::Id),
}

struct App {
    cfg: Config,
    rt: tokio::runtime::Runtime,
    sdl: Option<Sdl>,
    gamepad_mask: u32,
    kb_mask: u32,

    link: Arc<link::Link>,
    emu: Option<emu::Emu>,
    _audio: Option<audio::Backend>,
    game: Option<&'static emu::Game>,
    /// Woken by the emulator's frame callback; drives the [`Message::Frame`]
    /// subscription stream. App-lifetime so the subscription identity is
    /// stable across sessions.
    frame_notify: Arc<tokio::sync::Notify>,

    status: Arc<Mutex<net::Status>>,
    cancel: Option<CancellationToken>,

    screen: Option<image::Handle>,
    rgba: Vec<u8>,
    error: Option<String>,
    advanced: bool,
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let sdl = match Sdl::init() {
            Ok(sdl) => Some(sdl),
            Err(e) => {
                log::warn!("SDL unavailable, no audio/gamepads: {e}");
                None
            }
        };
        let app = Self {
            cfg: Config::load(),
            rt: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime"),
            sdl,
            gamepad_mask: 0,
            kb_mask: 0,
            link: Arc::new(link::Link::new()),
            emu: None,
            _audio: None,
            game: None,
            frame_notify: Arc::new(tokio::sync::Notify::new()),
            status: Arc::new(Mutex::new(net::Status::Idle)),
            cancel: None,
            screen: None,
            rgba: vec![0u8; (SCREEN_W * SCREEN_H * 4) as usize],
            error: None,
            advanced: false,
        };
        (app, Task::none())
    }

    fn play(&mut self) -> Task<Message> {
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
            let game = emu::identify(&rom).ok_or_else(|| {
                anyhow::anyhow!(
                    "{} doesn't look like {}: game code {}",
                    rom_path.display(),
                    emu::supported_titles(),
                    emu::header_code(&rom)
                )
            })?;
            let save_file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&save_path)?;
            let emu = emu::start(
                rom,
                save_file,
                game,
                self.link.clone(),
                self.frame_notify.clone(),
            )?;
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
            self.game = Some(game);
            self.emu = Some(emu);
            Ok(())
        })();
        if let Err(e) = result {
            self._audio = None;
            self.error = Some(e.to_string());
            Task::none()
        } else {
            self.cfg.save();
            // Starting the game flows straight into typing the link code.
            iced::widget::operation::focus("link-code")
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
        self.game = None;
    }

    fn connect(&mut self) {
        let Some(game) = self.game else { return };
        if self.cfg.link_code.trim().is_empty() {
            return;
        }
        self.disconnect();
        self.error = None;
        let cancel = CancellationToken::new();
        net::spawn_connect(
            self.rt.handle(),
            net::ConnectParams {
                endpoint: self.cfg.matchmaking_endpoint.clone(),
                link_code: self.cfg.link_code.clone(),
                game_code: game.code,
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

    /// True while a connect task is running and hasn't failed — the state
    /// where the top bar shows Disconnect instead of Connect.
    fn connecting(&self) -> bool {
        self.cancel.is_some()
            && !matches!(
                *self.status.lock().unwrap(),
                net::Status::Idle | net::Status::Lost(_)
            )
    }

    fn on_frame(&mut self) {
        let mut gamepad_mask = self.gamepad_mask;
        if let Some(sdl) = &mut self.sdl {
            sdl.pump_gamepads(&mut gamepad_mask);
        }
        self.gamepad_mask = gamepad_mask;

        let Some(emu) = &self.emu else { return };
        emu.handle.set_keys(self.kb_mask | self.gamepad_mask);
        if emu.handle.has_crashed() {
            self.stop();
            self.error = Some("emulator crashed".to_owned());
            return;
        }
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
        self.screen = Some(image::Handle::from_rgba(
            SCREEN_W,
            SCREEN_H,
            self.rgba.clone(),
        ));
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Frame => {
                self.on_frame();
                Task::none()
            }
            Message::PickRom => Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .add_filter("GBA ROM", &["gba"])
                        .set_title("Battle Chip Challenge (US) / Battle Chip GP (JP) ROM")
                        .pick_file()
                        .await
                        .map(|f| f.path().to_path_buf())
                },
                Message::RomPicked,
            ),
            Message::RomPicked(Some(path)) => {
                // Validate right here so a wrong pick is flagged at the
                // picker, not at Start.
                match std::fs::read(&path) {
                    Ok(rom) if emu::identify(&rom).is_some() => {
                        // Default the save next to the ROM so the common
                        // case is one click.
                        if self.cfg.save_path.is_none() {
                            self.cfg.save_path = Some(path.with_extension("sav"));
                        }
                        self.cfg.rom_path = Some(path);
                        self.error = None;
                    }
                    Ok(_) => {
                        self.error = Some(format!(
                            "{} doesn't look like {}",
                            path.display(),
                            emu::supported_titles()
                        ));
                    }
                    Err(e) => self.error = Some(format!("{}: {e}", path.display())),
                }
                Task::none()
            }
            Message::PickSave => {
                let file_name = self
                    .cfg
                    .save_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "bcc.sav".to_owned());
                Task::perform(
                    async move {
                        rfd::AsyncFileDialog::new()
                            .add_filter("GBA save", &["sav"])
                            .set_title("Save file (created if missing)")
                            .set_file_name(file_name)
                            .pick_file()
                            .await
                            .map(|f| f.path().to_path_buf())
                    },
                    Message::SavePicked,
                )
            }
            Message::SavePicked(Some(path)) => {
                self.cfg.save_path = Some(path);
                Task::none()
            }
            Message::RomPicked(None) | Message::SavePicked(None) => Task::none(),
            Message::Play => self.play(),
            Message::Stop => {
                self.stop();
                Task::none()
            }
            Message::LinkCodeChanged(code) => {
                self.cfg.link_code = code;
                Task::none()
            }
            Message::EndpointChanged(endpoint) => {
                self.cfg.matchmaking_endpoint = endpoint;
                Task::none()
            }
            Message::EndpointReset => {
                self.cfg.matchmaking_endpoint = DEFAULT_MATCHMAKING_ENDPOINT.to_owned();
                Task::none()
            }
            Message::ToggleAdvanced => {
                self.advanced = !self.advanced;
                Task::none()
            }
            Message::Connect => {
                if !self.connecting() {
                    self.connect();
                }
                Task::none()
            }
            Message::Disconnect => {
                self.disconnect();
                Task::none()
            }
            Message::KeyDown(bit) => {
                self.kb_mask |= bit;
                Task::none()
            }
            Message::KeyUp(bit) => {
                self.kb_mask &= !bit;
                Task::none()
            }
            Message::CloseRequested(id) => {
                self.disconnect();
                self.stop();
                self.cfg.save();
                iced::window::close(id)
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            iced::event::listen_with(key_message),
            iced::window::close_requests().map(Message::CloseRequested),
        ];
        if self.emu.is_some() {
            subs.push(Subscription::run_with(
                FrameTag {
                    notify: self.frame_notify.clone(),
                },
                frame_stream,
            ));
        }
        Subscription::batch(subs)
    }

    fn view(&self) -> Element<'_, Message> {
        if self.emu.is_none() {
            self.setup_view()
        } else {
            self.game_view()
        }
    }

    /// The pre-game screen: a centered card — pick files, start. Everything
    /// else lives on the game screen's top bar.
    fn setup_view(&self) -> Element<'_, Message> {
        let file_row = |label: &'static str, path: &Option<PathBuf>, empty: &str, msg: Message| {
            let name = path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned());
            let chosen = name.is_some();
            row![
                text(label).size(TEXT_CAPTION).color(WEAK).width(36),
                button(
                    text(name.unwrap_or_else(|| empty.to_owned()))
                        .size(TEXT_BODY)
                        .color(if chosen { TEXT } else { WEAK })
                )
                .style(button::secondary)
                .padding([6, 12])
                .width(Length::Fill)
                .on_press(msg),
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center)
        };

        let can_play = self.cfg.rom_path.is_some() && self.cfg.save_path.is_some();

        let mut card = column![
            text("Ring").size(TEXT_TITLE),
            text("link play for Battle Chip Challenge / Battle Chip GP")
                .size(TEXT_CAPTION)
                .color(WEAK),
        ]
        .spacing(4)
        .align_x(iced::Alignment::Center);

        card = card.push(iced::widget::space().height(12));
        card = card.push(
            column![
                file_row("ROM", &self.cfg.rom_path, "choose a ROM…", Message::PickRom),
                file_row("Save", &self.cfg.save_path, "choose a save…", Message::PickSave),
            ]
            .spacing(8)
            .width(Length::Fill),
        );
        card = card.push(iced::widget::space().height(8));
        card = card.push(
            button(text("Start game").size(15.0).center().width(Length::Fill))
                .padding([9, 12])
                .width(Length::Fill)
                .on_press_maybe(can_play.then_some(Message::Play)),
        );
        card = card.push(
            if let Some(error) = &self.error {
                text(error.clone()).size(TEXT_CAPTION).color(DANGER)
            } else if !can_play {
                text("pick a ROM to begin — the save defaults to sit next to it")
                    .size(TEXT_CAPTION)
                    .color(WEAK)
            } else {
                text("").size(TEXT_CAPTION)
            }
            .center()
            .width(Length::Fill),
        );

        card = card.push(iced::widget::space().height(4));
        card = card.push(
            button(
                text(if self.advanced {
                    "advanced ▾"
                } else {
                    "advanced ▸"
                })
                .size(TEXT_CAPTION)
                .color(WEAK),
            )
            .style(button::text)
            .padding(0)
            .on_press(Message::ToggleAdvanced),
        );
        if self.advanced {
            card = card.push(
                row![
                    text("matchmaking server").size(TEXT_CAPTION).color(WEAK),
                    text_input(DEFAULT_MATCHMAKING_ENDPOINT, &self.cfg.matchmaking_endpoint)
                        .size(TEXT_CAPTION)
                        .padding([5, 8])
                        .on_input(Message::EndpointChanged)
                        .width(Length::Fill),
                    button(text("reset").size(TEXT_CAPTION))
                        .style(button::secondary)
                        .padding([5, 10])
                        .on_press(Message::EndpointReset),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            );
        }

        let footer = text("arrows = D-pad · Z/X = A/B · A/S = L/R · Enter = Start · Backspace = Select")
            .size(TEXT_CAPTION)
            .color(WEAK);

        container(
            column![
                container(card.width(360)).padding(24).style(surface),
                footer,
            ]
            .spacing(12)
            .align_x(iced::Alignment::Center),
        )
        .center(Length::Fill)
        .into()
    }

    /// The game screen: top bar (link code, connection status, stop) over
    /// the integer-scaled screen on black.
    fn game_view(&self) -> Element<'_, Message> {
        let status = self.status.lock().unwrap().clone();
        let connecting = self.connecting();

        let mut bar = row![].spacing(10).align_y(iced::Alignment::Center);
        bar = bar.push(text("link code").size(TEXT_CAPTION).color(WEAK));
        {
            let mut code_edit = text_input("make one up, share it", &self.cfg.link_code)
                .id("link-code")
                .size(TEXT_BODY)
                .padding([5, 8])
                .on_input(Message::LinkCodeChanged)
                .width(150);
            if !connecting {
                code_edit = code_edit.on_submit(Message::Connect);
            }
            bar = bar.push(code_edit);
        }
        if connecting {
            bar = bar.push(
                button(text("Disconnect").size(TEXT_BODY))
                    .style(button::secondary)
                    .padding([5, 12])
                    .on_press(Message::Disconnect),
            );
        } else {
            let can_connect = !self.cfg.link_code.trim().is_empty();
            bar = bar.push(
                button(text("Connect").size(TEXT_BODY))
                    .padding([5, 12])
                    .on_press_maybe(can_connect.then_some(Message::Connect)),
            );
        }
        let (dot, line, color) = match status {
            net::Status::Idle => (
                "○",
                "enter a shared code, then Connect".to_owned(),
                WEAK,
            ),
            net::Status::Signaling => ("◌", "contacting server…".to_owned(), WARNING),
            net::Status::WaitingForPeer => ("◌", "waiting for opponent…".to_owned(), WARNING),
            net::Status::Connected {
                side,
                cross_version,
            } => (
                "●",
                format!(
                    "linked{} — you are P{} — PET → Transmit in-game",
                    if cross_version { " US↔JP" } else { "" },
                    side + 1
                ),
                ACCENT,
            ),
            net::Status::Lost(reason) => ("●", reason, DANGER),
        };
        bar = bar.push(
            row![
                text(dot).size(TEXT_CAPTION).color(color),
                text(line).size(TEXT_CAPTION).color(color),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        );
        bar = bar.push(iced::widget::space().width(Length::Fill));
        bar = bar.push(
            button(text("Stop").size(TEXT_BODY))
                .style(button::danger)
                .padding([5, 12])
                .on_press(Message::Stop),
        );

        let screen: Element<'_, Message> = if let Some(handle) = &self.screen {
            let handle = handle.clone();
            iced::widget::responsive(move |size| {
                let scale = (size.width / SCREEN_W as f32)
                    .min(size.height / SCREEN_H as f32)
                    .floor()
                    .max(1.0);
                container(
                    image(handle.clone())
                        .filter_method(image::FilterMethod::Nearest)
                        .width(SCREEN_W as f32 * scale)
                        .height(SCREEN_H as f32 * scale),
                )
                .center(Length::Fill)
                .into()
            })
            .into()
        } else {
            iced::widget::space().into()
        };

        column![
            container(bar).padding([8, 12]).width(Length::Fill).style(
                |_: &iced::Theme| container::Style {
                    background: Some(SURFACE.into()),
                    ..Default::default()
                }
            ),
            container(screen)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_| container::Style {
                    background: Some(iced::Color::BLACK.into()),
                    ..Default::default()
                }),
        ]
        .into()
    }
}

/// Stable subscription identity for the frame stream; the `notify` payload
/// carries the wake handle through to [`frame_stream`].
struct FrameTag {
    notify: Arc<tokio::sync::Notify>,
}

impl std::hash::Hash for FrameTag {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        "emu-frame".hash(h);
    }
}

fn frame_stream(tag: &FrameTag) -> impl futures::Stream<Item = Message> {
    futures::stream::unfold(tag.notify.clone(), |notify| async move {
        notify.notified().await;
        Some((Message::Frame, notify))
    })
}

/// Keyboard → game-input events. Presses only count when no widget captured
/// them (typing a link code must not press game buttons); releases always
/// count, so a key can't stick if focus moves mid-hold.
fn key_message(
    event: iced::Event,
    status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<Message> {
    use iced::keyboard::key::Physical;
    use iced::keyboard::Event as KeyEvent;
    match event {
        iced::Event::Keyboard(KeyEvent::KeyPressed {
            physical_key: Physical::Code(code),
            ..
        }) if status == iced::event::Status::Ignored => key_bit(code).map(Message::KeyDown),
        iced::Event::Keyboard(KeyEvent::KeyReleased {
            physical_key: Physical::Code(code),
            ..
        }) => key_bit(code).map(Message::KeyUp),
        _ => None,
    }
}

/// Decode the embedded `assets/icon.png` into an iced `window::Icon`.
/// Returns `None` on any failure — a corrupt asset just leaves the OS
/// default icon, no need to escalate. (Windows also gets the exe's embedded
/// ICON resource via build.rs; macOS ignores this, its icon would come from
/// an app bundle.)
fn load_window_icon() -> Option<iced::window::Icon> {
    // `::image` — the crate; plain `image` is the iced widget imported above.
    let img = ::image::load_from_memory(include_bytes!("../assets/icon.png"))
        .ok()?
        .into_rgba8();
    let (w, h) = img.dimensions();
    iced::window::icon::from_rgba(img.into_raw(), w, h).ok()
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    iced::application(App::new, App::update, App::view)
        // vsync off, same as tango: the emulator paces frames off audio;
        // an Immediate present keeps input→photon latency minimal.
        .settings(iced::Settings {
            vsync: false,
            default_text_size: iced::Pixels(TEXT_BODY),
            ..iced::Settings::default()
        })
        .title("Ring")
        .theme(theme)
        .subscription(App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(
                SCREEN_W as f32 * 3.0 + 16.0,
                SCREEN_H as f32 * 3.0 + 110.0,
            ),
            // OS-level window icon (title bar + taskbar).
            icon: load_window_icon(),
            // Close goes through Message::CloseRequested so the config gets
            // saved and the session torn down in order.
            exit_on_close_request: false,
            ..Default::default()
        })
        .run()
        .map_err(|e| anyhow::anyhow!("iced: {e}"))
}
