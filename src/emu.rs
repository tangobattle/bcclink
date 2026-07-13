//! Emulator bring-up: an mgba core with the BCC link traps installed,
//! running on mgba's own thread, audio-synced (the audio backend's
//! consumption paces emulation — see [`crate::audio`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::hooks;
use crate::link::Link;

pub const SCREEN_W: u32 = 240;
pub const SCREEN_H: u32 = 160;

/// A supported ROM: identified by full-file CRC32, hooked via its offsets
/// table. The netplay hello exchanges CRCs but doesn't gate on them — US↔JP
/// crossplay is allowed (see [`crate::net`]).
pub struct Game {
    pub crc32: u32,
    pub title: &'static str,
    pub offsets: &'static hooks::Offsets,
}

pub static GAMES: [Game; 2] = [
    Game {
        crc32: 0x26be44fd,
        title: "Mega Man Battle Chip Challenge (US)",
        offsets: &hooks::A89E_00,
    },
    Game {
        crc32: 0x9217fb18,
        title: "Rockman EXE Battle Chip GP (JP)",
        offsets: &hooks::A89J_00,
    },
];

pub fn identify(rom: &[u8]) -> Option<&'static Game> {
    let crc = crc32fast::hash(rom);
    GAMES.iter().find(|game| game.crc32 == crc)
}

/// For "unsupported ROM" error messages.
pub fn supported_titles() -> String {
    GAMES
        .iter()
        .map(|game| game.title)
        .collect::<Vec<_>>()
        .join(" or ")
}

pub struct Emu {
    pub handle: mgba::thread::Handle,
    /// Raw BGR555 out of mgba, 2 bytes/pixel.
    pub vbuf: Arc<Mutex<Vec<u8>>>,
    pub dirty: Arc<AtomicBool>,
    _thread: mgba::thread::Thread,
}

pub fn start(
    rom: Vec<u8>,
    save_file: std::fs::File,
    game: &'static Game,
    link: Arc<Link>,
) -> anyhow::Result<Emu> {
    let mut core = mgba::core::Core::new_gba(
        "bcclink",
        &mgba::core::Options {
            // Emulation is paced by the audio stream's consumption; without
            // this the core free-runs as fast as the host allows.
            audio_sync: true,
            ..Default::default()
        },
    )?;
    core.enable_video_buffer();
    core.as_mut().load_rom(mgba::vfile::VFile::from_vec(rom))?;
    core.as_mut().load_save(mgba::vfile::VFile::from_file(save_file))?;
    core.set_traps(hooks::traps(game.offsets, link));

    let vbuf = Arc::new(Mutex::new(vec![0u8; (SCREEN_W * SCREEN_H * 2) as usize]));
    let dirty = Arc::new(AtomicBool::new(false));

    let thread = mgba::thread::Thread::new(core);
    thread.set_frame_callback({
        let vbuf = vbuf.clone();
        let dirty = dirty.clone();
        move |_core, video_buffer, _thread_handle| {
            vbuf.lock().unwrap().copy_from_slice(video_buffer);
            dirty.store(true, Ordering::Release);
        }
    });
    thread.start().map_err(|e| anyhow::anyhow!("mgba thread start: {e:?}"))?;
    let handle = thread.handle();
    handle.lock_audio().sync_mut().set_fps_target(60.0);

    Ok(Emu {
        handle,
        vbuf,
        dirty,
        _thread: thread,
    })
}
