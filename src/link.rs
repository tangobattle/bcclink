//! The BCC lockstep link: shared state between the emulator traps and the
//! network threads.
//!
//! BCC's Transmit applet drives everything through one comm library: a
//! connect handshake of four block exchanges — the **mode** byte (drvA; the
//! game itself compares and shows "Connect failure!" on a mismatch), the
//! 8-byte navi **record** (drvB), the 16-byte **identity** (drvC), and the
//! parent's **word** `arena << 16 | rng16` (drvD, parent-wins; the child's
//! own ROM code copies the rng16 into its RNG) — followed, in the battle
//! modes, by an ordered byte stream through a per-turn SIO barrier. Guest
//! mode (2) ends after the identity exchange. The netplay contract is to
//! relay the blocks and the barrier stream faithfully, in order, over a
//! reliable transport — the game's own `-1` "still waiting" polling absorbs
//! any network latency.
//!
//! # Handshake generations
//!
//! The *player* walks in and out of PET → Transmit whenever they like: they
//! can cancel a connect, finish a battle or a guest exchange and come back
//! later, or survive a transport reconnect. Each entry runs a fresh
//! handshake, and stale bytes from an abandoned attempt must not leak into
//! the new one.
//!
//! So every wire message is tagged with a **generation**: a counter of
//! handshakes, anchored on the drvA *loader* (applet state 4 — exactly once
//! per session). A side opening a handshake moves to `max(gen + 1, highest
//! generation seen from the peer)` and drops its staged blocks; a side
//! waiting inside a handshake fast-forwards to the peer's newer generation
//! if one shows up, *keeping* its staged blocks (they belong to the
//! handshake its game is still inside) so they re-ship under the new tag.
//! Receivers drop anything tagged older than their current generation and
//! hold anything newer. Two sides that enter, cancel, and re-enter any
//! number of times therefore converge on a common generation, and the
//! barrier stream resets cleanly at each new handshake.

use std::collections::BTreeMap;
use std::sync::Mutex;

pub const TAG_BARRIER: u8 = 0;

/// The four connect-handshake exchange blocks. The wire tag is
/// `kind as u8 + 1`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// drvA: the selected comm mode byte (0 = Normal, 1 = Random, 2 = Guest).
    Mode = 0,
    /// drvB: the 8-byte navi record.
    Record = 1,
    /// drvC: the 16-byte identity block.
    Ident = 2,
    /// drvD: the parent's agreed word (`arena << 16 | rng16`). Only the
    /// parent publishes; the child injects it.
    Word = 3,
}

const BLOCK_KINDS: usize = 4;

/// What a barrier POLL trap should make the game see.
pub enum Poll {
    /// The peer's next barrier byte (sign-extended by the trap).
    Byte(u8),
    /// Nothing yet — return `-1`, the game re-polls.
    Waiting,
    /// The transport is down — return `-2`, the game aborts to the menu.
    Error,
}

pub struct Link {
    inner: Mutex<Inner>,
}

struct Inner {
    /// This console's battle side for the current connection: 0 = parent
    /// (the WebRTC offerer), 1 = child. Set per connection — the roles can
    /// swap across reconnects.
    side: u8,
    /// Transport up and hello exchanged.
    connected: bool,
    /// Transport down; traps report `-2` so the game backs out. Cleared on
    /// reconnect.
    error: bool,

    /// Current handshake generation. 0 = never entered Transmit.
    gen: u16,
    /// Highest generation seen in any peer message.
    peer_gen: u16,

    /// Our staged handshake blocks for the current generation, and the
    /// generation each was last shipped under (a fast-forwarded generation
    /// re-ships automatically because the shipped tag no longer matches).
    my_blocks: [Option<Vec<u8>>; BLOCK_KINDS],
    my_shipped: [Option<u16>; BLOCK_KINDS],
    /// The peer's blocks, tagged with the generation they were sent under.
    /// Injected only when the tag matches our current generation.
    peer_blocks: [Option<(u16, Vec<u8>)>; BLOCK_KINDS],

    /// Outgoing barrier bytes in push order, each tagged with the generation
    /// it was produced under. Append-only; `tx_shipped` marks the drain
    /// point. Stale-generation bytes still ship and are dropped by the peer.
    tx: Vec<(u16, u8)>,
    tx_shipped: usize,

    /// The peer's barrier bytes for our current generation, consumed by
    /// index, plus any that arrived tagged with a future generation.
    peer_rx: Vec<u8>,
    peer_rx_pending: BTreeMap<u16, Vec<u8>>,
    poll_idx: usize,
}

impl Link {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                side: 0,
                connected: false,
                error: false,
                gen: 0,
                peer_gen: 0,
                my_blocks: Default::default(),
                my_shipped: Default::default(),
                peer_blocks: Default::default(),
                tx: Vec::new(),
                tx_shipped: 0,
                peer_rx: Vec::new(),
                peer_rx_pending: BTreeMap::new(),
                poll_idx: 0,
            }),
        }
    }

    // --- transport-facing API ---

    pub fn set_connected(&self, side: u8) {
        let mut inner = self.inner.lock().unwrap();
        inner.side = side;
        inner.connected = true;
        inner.error = false;
    }

    pub fn set_error(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.connected = false;
        inner.error = true;
    }

    /// Wire messages produced since the last drain, in order:
    /// `[tag, gen_lo, gen_hi, payload...]`. The transport ships each verbatim,
    /// reliably and in order; the peer hands each to [`deliver`](Self::deliver).
    pub fn drain_outgoing(&self) -> Vec<Vec<u8>> {
        let mut inner = self.inner.lock().unwrap();
        let mut out = Vec::new();

        for kind in 0..BLOCK_KINDS {
            if inner.my_shipped[kind] != Some(inner.gen) {
                if let Some(block) = inner.my_blocks[kind].clone() {
                    out.push(frame(kind as u8 + 1, inner.gen, &block));
                    inner.my_shipped[kind] = Some(inner.gen);
                }
            }
        }

        // Group new barrier bytes into runs of one generation so each message
        // carries a single tag.
        let mut i = inner.tx_shipped;
        while i < inner.tx.len() {
            let run_gen = inner.tx[i].0;
            let mut run = Vec::new();
            while i < inner.tx.len() && inner.tx[i].0 == run_gen {
                run.push(inner.tx[i].1);
                i += 1;
            }
            out.push(frame(TAG_BARRIER, run_gen, &run));
        }
        inner.tx_shipped = inner.tx.len();

        out
    }

    /// Consume one framed wire message the peer produced via
    /// [`drain_outgoing`](Self::drain_outgoing).
    pub fn deliver(&self, msg: &[u8]) {
        if msg.len() < 3 {
            return;
        }
        let (tag, gen, payload) = (msg[0], u16::from_le_bytes([msg[1], msg[2]]), &msg[3..]);
        let mut inner = self.inner.lock().unwrap();
        inner.peer_gen = inner.peer_gen.max(gen);
        match tag {
            TAG_BARRIER => {
                if gen == inner.gen {
                    inner.peer_rx.extend_from_slice(payload);
                } else if gen > inner.gen {
                    inner.peer_rx_pending.entry(gen).or_default().extend_from_slice(payload);
                }
                // Older generation: a dead handshake's leftovers — drop.
            }
            tag if (tag as usize) <= BLOCK_KINDS => {
                let kind = (tag - 1) as usize;
                if inner.peer_blocks[kind].as_ref().is_none_or(|(g, _)| gen >= *g) {
                    inner.peer_blocks[kind] = Some((gen, payload.to_vec()));
                }
            }
            _ => {}
        }
    }

    // --- trap-facing API ---

    /// This console's battle side for the current connection (0 until one is
    /// up).
    pub fn side(&self) -> u8 {
        self.inner.lock().unwrap().side
    }

    pub fn is_connected(&self) -> bool {
        self.inner.lock().unwrap().connected
    }

    pub fn has_error(&self) -> bool {
        self.inner.lock().unwrap().error
    }

    /// The drvA loader fired: the game is opening a fresh connect handshake
    /// with the given comm mode. Enter the next generation (or jump to the
    /// peer's if it's already ahead), drop blocks staged by any previous
    /// handshake, and stage the mode byte for exchange.
    pub fn open_handshake(&self, mode: u8) {
        let mut inner = self.inner.lock().unwrap();
        let next = (inner.gen + 1).max(inner.peer_gen);
        inner.enter_gen(next, true);
        inner.my_blocks[BlockKind::Mode as usize] = Some(vec![mode]);
    }

    /// Stage our block of `kind` for the current handshake. Fast-forwards to
    /// the peer's newer generation first (keeping other staged blocks — they
    /// belong to the game-level handshake we're still inside, and the
    /// shipped-generation tracking re-ships them under the new tag; clearing
    /// them would starve the peer forever).
    pub fn stage_block(&self, kind: BlockKind, block: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        inner.fast_forward();
        inner.my_blocks[kind as usize] = Some(block);
    }

    /// The peer's block of `kind`, if it has arrived for the current
    /// handshake. Fast-forwards like [`stage_block`](Self::stage_block).
    pub fn peer_block(&self, kind: BlockKind) -> Option<Vec<u8>> {
        let mut inner = self.inner.lock().unwrap();
        inner.fast_forward();
        match &inner.peer_blocks[kind as usize] {
            Some((gen, block)) if *gen == inner.gen => Some(block.clone()),
            _ => None,
        }
    }

    /// A barrier SEND trap captured one outgoing byte.
    pub fn push_barrier(&self, byte: u8) {
        let mut inner = self.inner.lock().unwrap();
        let gen = inner.gen;
        inner.tx.push((gen, byte));
    }

    /// A barrier POLL trap wants the peer's next byte.
    pub fn poll_barrier(&self) -> Poll {
        let mut inner = self.inner.lock().unwrap();
        if inner.error {
            return Poll::Error;
        }
        if inner.poll_idx < inner.peer_rx.len() {
            let byte = inner.peer_rx[inner.poll_idx];
            inner.poll_idx += 1;
            Poll::Byte(byte)
        } else {
            Poll::Waiting
        }
    }
}

impl Inner {
    fn fast_forward(&mut self) {
        // The peer raced past an attempt we thought was current (it cancelled
        // and re-entered before its newer messages reached us). Jump to its
        // generation, keeping staged blocks.
        if self.peer_gen > self.gen {
            let next = self.peer_gen;
            self.enter_gen(next, false);
        }
    }

    fn enter_gen(&mut self, gen: u16, clear_staged: bool) {
        self.gen = gen;
        self.peer_rx = self.peer_rx_pending.remove(&gen).unwrap_or_default();
        self.peer_rx_pending = self.peer_rx_pending.split_off(&gen);
        self.poll_idx = 0;
        if clear_staged {
            self.my_blocks = Default::default();
        }
    }
}

fn frame(tag: u8, gen: u16, payload: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(payload.len() + 3);
    msg.push(tag);
    msg.extend_from_slice(&gen.to_le_bytes());
    msg.extend_from_slice(payload);
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ferry(from: &Link, to: &Link) {
        for msg in from.drain_outgoing() {
            to.deliver(&msg);
        }
    }

    fn connected_pair() -> (Link, Link) {
        let a = Link::new();
        let b = Link::new();
        a.set_connected(0);
        b.set_connected(1);
        (a, b)
    }

    #[test]
    fn clean_handshake_and_barrier() {
        let (a, b) = connected_pair();
        a.open_handshake(0);
        b.open_handshake(0);
        ferry(&a, &b);
        ferry(&b, &a);
        assert_eq!(a.peer_block(BlockKind::Mode).unwrap(), vec![0]);
        assert_eq!(b.peer_block(BlockKind::Mode).unwrap(), vec![0]);

        a.stage_block(BlockKind::Record, b"AAAAAAAA".to_vec());
        b.stage_block(BlockKind::Record, b"BBBBBBBB".to_vec());
        ferry(&a, &b);
        ferry(&b, &a);
        assert_eq!(a.peer_block(BlockKind::Record).unwrap(), b"BBBBBBBB");
        assert_eq!(b.peer_block(BlockKind::Record).unwrap(), b"AAAAAAAA");

        // drvD rendezvous: both publish (the child's word is a zero
        // don't-care), both wait for the other's.
        a.stage_block(BlockKind::Word, vec![0x34, 0x12, 0x03, 0x00]);
        b.stage_block(BlockKind::Word, vec![0, 0, 0, 0]);
        assert!(a.peer_block(BlockKind::Word).is_none());
        ferry(&a, &b);
        ferry(&b, &a);
        assert_eq!(b.peer_block(BlockKind::Word).unwrap(), vec![0x34, 0x12, 0x03, 0x00]);
        assert_eq!(a.peer_block(BlockKind::Word).unwrap(), vec![0, 0, 0, 0]);

        for byte in [1u8, 2, 3] {
            a.push_barrier(byte);
        }
        ferry(&a, &b);
        for want in [1u8, 2, 3] {
            match b.poll_barrier() {
                Poll::Byte(got) => assert_eq!(got, want),
                _ => panic!("expected byte"),
            }
        }
        assert!(matches!(b.poll_barrier(), Poll::Waiting));
    }

    #[test]
    fn mode_mismatch_is_visible_to_both() {
        // The game compares the peer's mode itself; the link just relays.
        let (a, b) = connected_pair();
        a.open_handshake(1);
        b.open_handshake(2);
        ferry(&a, &b);
        ferry(&b, &a);
        assert_eq!(a.peer_block(BlockKind::Mode).unwrap(), vec![2]);
        assert_eq!(b.peer_block(BlockKind::Mode).unwrap(), vec![1]);
    }

    #[test]
    fn cancel_reenter_race_converges() {
        let (a, b) = connected_pair();
        // Session 1 completes a mode + record exchange.
        a.open_handshake(0);
        b.open_handshake(0);
        a.stage_block(BlockKind::Record, b"ADECK1".to_vec());
        b.stage_block(BlockKind::Record, b"BDECK1".to_vec());
        ferry(&a, &b);
        ferry(&b, &a);
        assert!(a.peer_block(BlockKind::Record).is_some());
        assert!(b.peer_block(BlockKind::Record).is_some());

        // A enters again, ships its blocks, cancels, re-enters — while B
        // starts its handshake against the stale generation.
        a.open_handshake(0);
        a.stage_block(BlockKind::Record, b"ADECK2".to_vec());
        ferry(&a, &b); // the doomed blocks reach B
        a.open_handshake(0); // A is now a generation ahead
        b.open_handshake(0); // B still on the stale one
        // B's drvB completes instantly off the abandoned record (accepted
        // stale-content edge).
        assert_eq!(b.peer_block(BlockKind::Record).unwrap(), b"ADECK2");
        // A stages its new blocks; B learns A's generation from them, and B's
        // next phase poll fast-forwards — re-shipping B's staged blocks under
        // the new tag so A unblocks.
        b.stage_block(BlockKind::Record, b"BDECK2".to_vec());
        ferry(&b, &a);
        a.stage_block(BlockKind::Record, b"ADECK3".to_vec());
        assert!(a.peer_block(BlockKind::Record).is_none());
        ferry(&a, &b);
        b.stage_block(BlockKind::Ident, b"BIDENT2xxxxxxxxx".to_vec());
        ferry(&b, &a);
        assert_eq!(a.peer_block(BlockKind::Record).unwrap(), b"BDECK2");
        a.stage_block(BlockKind::Ident, b"AIDENT3xxxxxxxxx".to_vec());
        ferry(&a, &b);
        assert_eq!(b.peer_block(BlockKind::Ident).unwrap(), b"AIDENT3xxxxxxxxx");
        ferry(&b, &a);
        assert_eq!(a.peer_block(BlockKind::Ident).unwrap(), b"BIDENT2xxxxxxxxx");
    }

    #[test]
    fn stale_generation_barrier_bytes_are_dropped() {
        let (a, b) = connected_pair();
        a.open_handshake(0);
        b.open_handshake(0);
        a.push_barrier(0x55); // in-flight from a session about to die
        a.open_handshake(0); // new session
        b.open_handshake(0);
        ferry(&a, &b); // stale byte arrives tagged with the dead generation
        assert!(matches!(b.poll_barrier(), Poll::Waiting));
    }
}
