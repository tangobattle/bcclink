//! Two-core in-process proof of the app's netplay path, for every Transmit
//! mode.
//!
//! Boots two BCC cores with the app's real trap set and two [`Link`]s wired
//! back-to-back (each frame, one link's drained wire messages are delivered
//! to the other — exactly what the network task does). The app itself has no
//! menu autopilot — the player navigates — so this harness adds its own nav
//! trap on top: the input-driven A+START masher the original two-core proofs
//! used, plus a poke of the comm-mode byte to select the mode under test.
//!
//! Success:
//! - `normal` / `random` (battle modes): both cores boot to a link battle
//!   and stay bit-identical every frame to KO. `random` meaningfully tests
//!   the drvD word relay — the parent's arena roll advances its RNG, so the
//!   child only stays in sync if the parent's word (and its rng16) actually
//!   crossed the link.
//! - `guest` (the non-battle exchange): both cores complete the handshake
//!   and land on the "Guest deck registered!" result (comm program 3 with
//!   result message 5 and the peer's record delivered) — then re-enter
//!   Transmit in Normal mode and run a battle to KO, proving the guest
//!   session closes cleanly and the same link carries a second handshake.
//!
//! Run: cargo run --release -p bcclink --example selftest [-- normal|random|guest] [slot] [bcgp|cross|crossr]
//!   (`slot`: core 0 pulses L-slot and core 1 pulses R-slot during battle —
//!   asymmetric input that only stays in sync if genuinely relayed.
//!   `bcgp`: both cores run the JP ROM; `cross`: core 0 (the parent) runs US
//!   and core 1 JP; `crossr` the other way round — the crossplay proofs, in
//!   both drvD parent directions. Each core's save sits next to its ROM:
//!   roms/bcc.sav / roms/bcgp.sav.)

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use bcclink::link::Link;
use bcclink::{emu, hooks};

const UNIT_L_HP: u32 = 0x0200_b832;
const UNIT_R_HP: u32 = 0x0200_b8c2;
const COMM_PROGRAM: u32 = 0x0200_b794;
const COMM_SUBSTATE: u32 = 0x0200_b795;
const COMM_MODE: u32 = 0x0200_b796;
const COMM_RESULT: u32 = 0x0200_b797;

/// Comm program 5 = the comm-error backout (every `-2` phase/poll lands
/// there); connect-applet substates 50+ are its own failure screens
/// ("Connect failure!" on a mode mismatch is substate 50).
const PROGRAM_COMM_ERROR: u8 = 5;
const SUBSTATE_FAILURE: u8 = 50;
/// The connect applet's guest-completion path sets comm program 3 with this
/// result message: "Guest deck registered!".
const RESULT_GUEST_REGISTERED: u8 = 5;

const KEY_A: u32 = 1 << 0;
const KEY_START: u32 = 1 << 3;
const KEY_R: u32 = 1 << 8;
const KEY_L: u32 = 1 << 9;

fn shot(core: &mgba::core::Core, path: &str) {
    let Some(vbuf) = core.video_buffer() else { return };
    let mut rgba = vec![0u8; 240 * 160 * 4];
    for (dst, src) in rgba.chunks_exact_mut(4).zip(vbuf.chunks_exact(2)) {
        let v = u16::from_le_bytes([src[0], src[1]]);
        dst[0] = ((v & 0x1f) << 3) as u8;
        dst[1] = (((v >> 5) & 0x1f) << 3) as u8;
        dst[2] = (((v >> 10) & 0x1f) << 3) as u8;
        dst[3] = 0xff;
    }
    if let Err(e) = image::save_buffer(path, &rgba, 240, 160, image::ColorType::Rgba8) {
        println!("[selftest] screenshot failed: {e}");
    }
}

/// Harness-only: the keypad-read hook where the nav masher injects input —
/// a ROM address, so it shifts per version like the hooked comm library.
fn keypad_read(game: &emu::Game) -> u32 {
    match game.crc32 {
        0x9217fb18 => 0x08001ce2, // JP (A89J)
        _ => 0x08001cee,          // US (A89E)
    }
}

fn boot(
    rom_path: &str,
    name: &str,
    link: Arc<Link>,
    frame: Arc<AtomicU32>,
    mode: Arc<AtomicU8>,
) -> mgba::core::Core {
    let rom = std::fs::read(rom_path)
        .unwrap_or_else(|e| panic!("run from the repo root: {rom_path}: {e}"));
    let save_path = rom_path.replace(".gba", ".sav");
    let save = std::fs::read(&save_path).unwrap_or_else(|e| panic!("{save_path}: {e}"));
    let game = emu::identify(&rom)
        .unwrap_or_else(|| panic!("{rom_path} isn't a supported ROM"));
    println!("[selftest] {name}: {rom_path} ({})", game.title);

    let mut core = mgba::core::Core::new_gba(name, &mgba::core::Options::default()).unwrap();
    core.enable_video_buffer();
    core.as_mut().load_rom(mgba::vfile::VFile::from_vec(rom)).unwrap();
    core.as_mut()
        .load_save(mgba::vfile::VFile::from_vec(save))
        .unwrap();

    let mut traps = hooks::traps(game.offsets, link);

    // Headless menu nav (harness-only): mash A+START at the keypad read and
    // poke the hub/PET selections until the comm applet takes over, forcing
    // the comm-mode byte to the mode under test until the applet's drvA
    // loader (substate 4) has read it.
    traps.push((
        keypad_read(game),
        Box::new(move |mut core: mgba::core::CoreMutRef| {
            let program = core.raw_read_8(COMM_PROGRAM, -1);
            let substate = core.raw_read_8(COMM_SUBSTATE, -1);
            if program >= 2 || (program == 1 && substate >= 4) {
                return; // connecting / in battle — leave input alone
            }
            core.raw_write_8(COMM_MODE, -1, mode.load(Ordering::Relaxed));
            let scene = core.raw_read_8(0x0200_70f0, -1);
            if scene == 0x08 {
                core.raw_write_8(0x0200_b788, -1, 0); // hub: select PET
            }
            if scene == 0x0c {
                core.raw_write_8(0x0200_b789, -1, 7); // PET: select Transmit
            }
            let f = frame.load(Ordering::Relaxed);
            let pressed = (f / 3) % 2 == 0;
            let base = core.as_ref().gba().cpu().gpr(0) as u16 | 0x03ff;
            let r0 = if pressed {
                base & !((KEY_A | KEY_START) as u16)
            } else {
                base
            };
            core.gba_mut().cpu_mut().set_gpr(0, r0 as i32);
        }),
    ));
    core.set_traps(traps);
    core
}

fn ferry(from: &Link, to: &Link) {
    for msg in from.drain_outgoing() {
        to.deliver(&msg);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let slot = args.iter().any(|a| a == "slot");
    let mode: u8 = if args.iter().any(|a| a == "random") {
        1
    } else if args.iter().any(|a| a == "guest") {
        2
    } else {
        0
    };
    let mode_name = ["normal", "random", "guest"][mode as usize];
    let (rom0, rom1) = if args.iter().any(|a| a == "cross") {
        ("roms/bcc.gba", "roms/bcgp.gba")
    } else if args.iter().any(|a| a == "crossr") {
        ("roms/bcgp.gba", "roms/bcc.gba")
    } else if args.iter().any(|a| a == "bcgp") {
        ("roms/bcgp.gba", "roms/bcgp.gba")
    } else {
        ("roms/bcc.gba", "roms/bcc.gba")
    };
    println!("[selftest] mode: {mode_name}, slot: {slot}");

    // What the transport hello does on both ends.
    let link0 = Arc::new(Link::new());
    let link1 = Arc::new(Link::new());
    link0.set_connected(0);
    link1.set_connected(1);

    // The comm mode the nav trap forces; the guest test flips it to normal
    // after the exchange to chain a battle over the same link.
    let forced_mode = Arc::new(AtomicU8::new(mode));
    let frame = Arc::new(AtomicU32::new(0));
    let mut c0 = boot(rom0, "bcclink-st-0", link0.clone(), frame.clone(), forced_mode.clone());
    let mut c1 = boot(rom1, "bcclink-st-1", link1.clone(), frame.clone(), forced_mode.clone());
    c0.as_mut().reset();
    c1.as_mut().reset();

    let mut in_battle = false;
    let mut desynced = false;
    let mut guest_registered = false;
    for step in 0..10000u32 {
        // Mash through the battle AND, once the guest deck registered,
        // through its result dialog back to the menu (the nav trap is inert
        // there — it only drives while the comm applet isn't up).
        let mash = in_battle || guest_registered;
        let advance = if mash && (step / 3) % 2 == 0 {
            KEY_A | KEY_START
        } else {
            0
        };
        let pulse = in_battle && slot && (step / 2) % 2 == 0;
        c0.as_mut().set_keys(advance | if pulse { KEY_L } else { 0 });
        c1.as_mut().set_keys(advance | if pulse { KEY_R } else { 0 });
        c0.as_mut().run_frame();
        c1.as_mut().run_frame();
        frame.fetch_add(1, Ordering::Relaxed);
        // Twice both ways so a block exchange's round trip converges within
        // a frame, like a real link.
        ferry(&link0, &link1);
        ferry(&link1, &link0);
        ferry(&link0, &link1);

        // The game's own failure paths are never OK in any mode.
        for (i, core) in [&mut c0, &mut c1].into_iter().enumerate() {
            let program = core.as_mut().raw_read_8(COMM_PROGRAM, -1);
            let substate = core.as_mut().raw_read_8(COMM_SUBSTATE, -1);
            if program == PROGRAM_COMM_ERROR || (program == 1 && substate >= SUBSTATE_FAILURE) {
                println!("[selftest] FAILED at step {step}: c{i} hit the comm failure path (program {program}, substate {substate})");
                std::process::exit(1);
            }
        }

        if mode == 2 && !guest_registered {
            // Guest phase: both cores must land on the "Guest deck
            // registered!" result with the peer's record actually delivered
            // to 0x0200B69C (zero at boot).
            let done = [&mut c0, &mut c1].into_iter().all(|core| {
                core.as_mut().raw_read_8(COMM_PROGRAM, -1) == 3
                    && core.as_mut().raw_read_8(COMM_RESULT, -1) == RESULT_GUEST_REGISTERED
            });
            if done {
                let mut rec0 = [0u8; 8];
                let mut rec1 = [0u8; 8];
                c0.as_mut().raw_read_range(0x0200_b69c, -1, &mut rec0);
                c1.as_mut().raw_read_range(0x0200_b69c, -1, &mut rec1);
                if rec0 == [0u8; 8] || rec1 == [0u8; 8] {
                    println!("[selftest] FAILED: guest result without a delivered record");
                    std::process::exit(1);
                }
                println!("[selftest] guest deck registered on both at step {step}: records {rec0:02x?} / {rec1:02x?}");
                // Let the result dialog render, then keep a screenshot pair.
                for _ in 0..60 {
                    c0.as_mut().run_frame();
                    c1.as_mut().run_frame();
                    ferry(&link0, &link1);
                    ferry(&link1, &link0);
                }
                shot(&c0, "target/selftest_guest_c0.png");
                shot(&c1, "target/selftest_guest_c1.png");
                // Chain phase two: back out to the menu and re-enter Transmit
                // in Normal mode — a battle over the same link proves the
                // guest session closed cleanly and a fresh handshake
                // generation works after it.
                guest_registered = true;
                forced_mode.store(0, Ordering::Relaxed);
            }
            continue;
        }

        let l0 = c0.as_mut().raw_read_16(UNIT_L_HP, -1);
        let r0 = c0.as_mut().raw_read_16(UNIT_R_HP, -1);
        let l1 = c1.as_mut().raw_read_16(UNIT_L_HP, -1);
        let r1 = c1.as_mut().raw_read_16(UNIT_R_HP, -1);
        if !in_battle && l0 > 0 && r0 > 0 {
            in_battle = true;
            println!("[selftest] battle live at step {step}");
        }
        if in_battle {
            if (l0, r0) != (l1, r1) {
                println!("[selftest] DESYNC at step {step}: c0=(L{l0},R{r0}) c1=(L{l1},R{r1})");
                desynced = true;
                break;
            }
            if step % 500 == 0 {
                println!("[selftest] frame-exact at step {step}: L={l0} R={r0}");
            }
            if l0 == 0 || r0 == 0 {
                println!("[selftest] battle ended in sync at step {step}: L={l0} R={r0}");
                break;
            }
        }
    }
    if mode == 2 && !guest_registered {
        println!("[selftest] FAILED: guest exchange never completed (c0 program {}, c1 program {})",
            c0.as_mut().raw_read_8(COMM_PROGRAM, -1),
            c1.as_mut().raw_read_8(COMM_PROGRAM, -1));
        std::process::exit(1);
    } else if !in_battle {
        if mode == 2 {
            println!("[selftest] FAILED: guest deck registered but the chained battle never started");
        } else {
            println!("[selftest] FAILED: never reached a battle");
        }
        std::process::exit(1);
    } else if desynced {
        println!("[selftest] RESULT: DESYNCED");
        std::process::exit(1);
    } else if mode == 2 {
        println!("[selftest] RESULT: GUEST OK — deck registered, then a chained battle stayed IN SYNC (slot={slot})");
    } else {
        println!("[selftest] RESULT: {mode_name} stayed IN SYNC (slot={slot}) — bcclink hooks work");
    }
}
