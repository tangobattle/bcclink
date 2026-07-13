//! Emulator traps that replace BCC's SIO comm library with the network link.
//!
//! Addresses were reverse-engineered from the US ROM (`A89E`, rev 0, CRC32
//! `0x26be44fd`) and proven frame-exact in two-core harnesses (originally
//! `mgba/examples/bcc_pvp.rs` in the Tango repo). Prof9's ChipControl mod
//! establishes that BCC's EWRAM layout is identical across US/EU/JP — only
//! ROM `0x08xxxxxx` addresses shift — so a JP/EU port needs a new
//! [`RomOffsets`] but can keep [`Ewram`]. The JP original ([`A89J_00`])
//! bears this out: its whole comm library is the US one shifted down by
//! `0x2F8` — every hooked function matched instruction-for-instruction with
//! only ROM literals shifted, and every EWRAM/IO literal (including the comm
//! struct) byte-identical. An EU port still needs its own table.
//!
//! The player drives the whole flow themselves: boot the game, load a save,
//! and walk to **PET → Transmit** when they want to link. All three Transmit
//! modes work — Normal battle, Random battle (random terrain), and Guest
//! (the non-battle navi exchange) — because the traps only replace the comm
//! library's transport, phase by phase, with the [`Link`]:
//!
//! - the cable-presence pump and side latch are forced up, with the side
//!   fixed by network role (offerer = parent/left, answerer = child/right);
//! - the drvA *loader* (comm-applet state 4, once per session) anchors a new
//!   handshake generation and publishes the locally selected mode byte; the
//!   drvA *poll* then returns the **peer's** mode, so the game runs its own
//!   mode-agreement check and shows its native "Connect failure!" when the
//!   two players picked different things;
//! - drvB/drvC exchange the 8-byte navi record and 16-byte identity block
//!   both ways (in Guest mode the game ends the session here on its own);
//! - drvD relays the parent's staged word (`arena << 16 | rng16` — the
//!   arena byte the parent rolled for Random mode, and its live RNG state)
//!   to the child, parent-wins exactly like the cable; the child's own ROM
//!   code copies the rng16 into its RNG, so both sides simulate the same
//!   battle without any fabricated seed;
//! - every per-turn barrier SEND/POLL primitive relays the game's ordered
//!   byte stream through the link, without needing to know what any byte
//!   means.
//!
//! Every drv phase holds the game at its own "connecting" screen (`-1`)
//! until the peer's data shows up, and a dead transport makes every phase
//! and poll return `-2`, which the game treats as a comm error and backs
//! out of on its own.

use std::sync::Arc;

use crate::link::{BlockKind, Link, Poll};

pub type Trap = (u32, Box<dyn Fn(mgba::core::CoreMutRef)>);

pub struct Ewram {
    /// Comm/SIO exchange struct base. `+0x82` local side index, `+0x84` drvB
    /// staging (8-byte record), `+0x8C` drvC staging (16-byte identity),
    /// `+0x9C` drvD staging (the agreed word).
    pub comm_struct: u32,
}

pub struct RomOffsets {
    /// Link "cable present?" pump; neutered to report up.
    pub link_pump: u32,
    /// Samples the SIO multiplayer ID into `comm_struct + 0x82`; neutered to
    /// write the network-assigned side instead.
    pub side_latch: u32,
    /// The drvA setup loader (`r0` = the selected comm mode byte). Called by
    /// comm-applet state 4 exactly once per session — the anchor for a new
    /// handshake generation. Left to run (it only stages local state).
    pub drv_a_loader: u32,
    /// The four connect-handshake phase polls, each returning `-1`
    /// (exchanging) / `-2` (error) / `>= 0` (done; drvA's done value is the
    /// peer's mode byte).
    pub drv_a: u32,
    pub drv_b: u32,
    pub drv_c: u32,
    pub drv_d: u32,
    /// Every barrier SEND primitive: per-turn slot-in channels (combined L+R
    /// and per-slot L/R), battle attach, and end-of-turn sync — every byte
    /// the game would have put on the cable.
    pub barrier_sends: [u32; 4],
    /// Every barrier POLL primitive, matching the sends.
    pub barrier_polls: [u32; 4],
}

pub struct Offsets {
    pub rom: RomOffsets,
    pub ewram: Ewram,
}

/// US, `A89E`, rev 0.
pub static A89E_00: Offsets = Offsets {
    ewram: Ewram {
        comm_struct: 0x0200bd20,
    },
    rom: RomOffsets {
        link_pump: 0x0804ad4c,
        side_latch: 0x0804a3c4,
        drv_a_loader: 0x0804a494,
        drv_a: 0x0804a4b0,
        drv_b: 0x0804a694,
        drv_c: 0x0804a8ac,
        drv_d: 0x0804aa94,
        barrier_sends: [0x0804ac88, 0x0804acdc, 0x0804ac48, 0x0804ad24],
        barrier_polls: [0x0804acac, 0x0804ad00, 0x0804ac64, 0x0804ad3c],
    },
};

/// JP (Rockman EXE Battle Chip GP), `A89J`, rev 0. Every ROM address is the
/// US one minus `0x2F8`; the EWRAM layout is unchanged.
pub static A89J_00: Offsets = Offsets {
    ewram: Ewram {
        comm_struct: 0x0200bd20,
    },
    rom: RomOffsets {
        link_pump: 0x0804aa54,
        side_latch: 0x0804a0cc,
        drv_a_loader: 0x0804a19c,
        drv_a: 0x0804a1b8,
        drv_b: 0x0804a39c,
        drv_c: 0x0804a5b4,
        drv_d: 0x0804a79c,
        barrier_sends: [0x0804a990, 0x0804a9e4, 0x0804a950, 0x0804aa2c],
        barrier_polls: [0x0804a9b4, 0x0804aa08, 0x0804a96c, 0x0804aa44],
    },
};

const STILL_EXCHANGING: u32 = 0xffff_ffff; // -1
const COMM_ERROR: u32 = 0xffff_fffe; // -2

/// Force a THUMB function entered via `bl` to return `r0` immediately: set
/// the return value and jump to the return address in `lr` without executing
/// the body. This is how every comm-library poll is neutered.
fn force_return(mut core: mgba::core::CoreMutRef, r0: u32) {
    let lr = core.as_ref().gba().cpu().gpr(14) as u32;
    core.gba_mut().cpu_mut().set_gpr(0, r0 as i32);
    core.gba_mut().cpu_mut().set_thumb_pc(lr);
}

pub fn traps(offsets: &'static Offsets, link: Arc<Link>) -> Vec<Trap> {
    let o = &offsets.rom;
    let comm_struct = offsets.ewram.comm_struct;
    let mut v: Vec<Trap> = Vec::new();

    v.push((o.link_pump, Box::new(|core| force_return(core, 0))));

    v.push((o.side_latch, {
        let link = link.clone();
        Box::new(move |mut core: mgba::core::CoreMutRef| {
            core.raw_write_8(comm_struct + 0x82, -1, link.side());
            force_return(core, 0);
        })
    }));

    // drvA loader: the game is opening a connect handshake with the selected
    // mode in r0. Observe only — the loader body just stages local state.
    v.push((o.drv_a_loader, {
        let link = link.clone();
        Box::new(move |core: mgba::core::CoreMutRef| {
            link.open_handshake(core.as_ref().gba().cpu().gpr(0) as u8);
        })
    }));

    // drvA poll: done = the PEER's mode byte; the game compares it with the
    // local selection itself (mismatch → its native "Connect failure!").
    // Holding at -1 until the peer shows up doubles as the "waiting at the
    // connecting screen" state. The side byte is rewritten here too: the
    // game latched it before drvA, but the connection (whose offerer /
    // answerer roles decide the side) may only have come up while this
    // screen was already polling.
    v.push((o.drv_a, {
        let link = link.clone();
        Box::new(move |mut core: mgba::core::CoreMutRef| {
            if link.has_error() {
                force_return(core, COMM_ERROR);
            } else if let Some(mode) = link.peer_block(BlockKind::Mode) {
                core.raw_write_8(comm_struct + 0x82, -1, link.side());
                force_return(core, mode.first().copied().unwrap_or(0) as u32);
            } else {
                force_return(core, STILL_EXCHANGING);
            }
        })
    }));

    // drvB / drvC: publish our staged block, inject the peer's into the
    // destination the game passed in r0 once it arrives for this generation.
    for (prim, kind, staging, len) in [
        (o.drv_b, BlockKind::Record, comm_struct + 0x84, 8usize),
        (o.drv_c, BlockKind::Ident, comm_struct + 0x8c, 16usize),
    ] {
        let link = link.clone();
        v.push((
            prim,
            Box::new(move |mut core: mgba::core::CoreMutRef| {
                if link.has_error() {
                    force_return(core, COMM_ERROR);
                    return;
                }
                let mut mine = vec![0u8; len];
                core.raw_read_range(staging, -1, &mut mine);
                link.stage_block(kind, mine);
                if let Some(peer) = link.peer_block(kind) {
                    let dst = core.as_ref().gba().cpu().gpr(0) as u32;
                    core.raw_write_range(dst, -1, &peer);
                    force_return(core, 0);
                } else {
                    force_return(core, STILL_EXCHANGING);
                }
            }),
        ));
    }

    // drvD: parent-word-wins, exactly like the cable. Each side's game
    // staged its TX word at comm_struct + 0x9C (state 10: the parent builds
    // `arena << 16 | rng16` — the arena it rolled for Random mode and its
    // live RNG state — the child stages zero, don't-care). Both publish and
    // both WAIT for the other's word (the real drvD is a rendezvous: the
    // parent checks the child's frame before completing); then the parent's
    // word is the agreed one — the child takes it, and the child's own ROM
    // code (US 0x08048B02..06, JP 0x08048806..0A) copies the rng16 into its
    // RNG. Guest mode never reaches this phase.
    v.push((o.drv_d, {
        let link = link.clone();
        Box::new(move |mut core: mgba::core::CoreMutRef| {
            if link.has_error() {
                force_return(core, COMM_ERROR);
                return;
            }
            let mut mine = vec![0u8; 4];
            core.raw_read_range(comm_struct + 0x9c, -1, &mut mine);
            link.stage_block(BlockKind::Word, mine.clone());
            let Some(peer) = link.peer_block(BlockKind::Word) else {
                force_return(core, STILL_EXCHANGING);
                return;
            };
            let agreed = if link.side() == 0 { &mine } else { &peer };
            let dst = core.as_ref().gba().cpu().gpr(0) as u32;
            core.raw_write_range(dst, -1, agreed);
            core.raw_write_range(comm_struct + 0x9c, -1, agreed);
            force_return(core, 0);
        })
    }));

    // The generic per-turn barrier relay. Every SEND's payload byte (r0) is
    // captured into the outgoing stream — the prim itself still runs, since it
    // only stages the byte and bumps the local sequence counter; the SIO
    // transfer it would trigger is what the POLL neuter replaces. Every POLL
    // returns the peer's next byte in order, sign-extended as the s8 the game
    // expects, or -1 while the network hasn't delivered it yet.
    for prim in o.barrier_sends {
        let link = link.clone();
        v.push((
            prim,
            Box::new(move |core: mgba::core::CoreMutRef| {
                link.push_barrier(core.as_ref().gba().cpu().gpr(0) as u8);
            }),
        ));
    }
    for prim in o.barrier_polls {
        let link = link.clone();
        v.push((
            prim,
            Box::new(move |core: mgba::core::CoreMutRef| match link.poll_barrier() {
                Poll::Byte(byte) => force_return(core, byte as i8 as i32 as u32),
                Poll::Waiting => force_return(core, STILL_EXCHANGING),
                Poll::Error => force_return(core, COMM_ERROR),
            }),
        ));
    }

    v
}
