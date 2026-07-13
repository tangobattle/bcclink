//! Matchmaking transport for the lockstep link: tango's signaling server +
//! a tango-rtc peer connection.
//!
//! Both players enter the same link code; the code is namespaced
//! (`ring:<code>`) so it can never pair with a Tango client on the shared
//! server. The session runs over a single **reliable + ordered** WebRTC data
//! channel — exactly what the lockstep byte stream needs — and NAT traversal
//! (STUN/TURN) comes with the ICE config the server hands out.
//!
//! The WebRTC offerer is the parent (side 0); the answerer is the child.
//! After the channel opens, both sides exchange a hello
//! (`b"RING" | version u8 | game code [u8; 4] | side u8`) — the game
//! code is informational (US↔JP crossplay is allowed; it only drives the
//! cross-version indicator) — then every message is a [`Link`] wire message
//! verbatim — the data channel preserves
//! message boundaries, so there's no framing. App-level keepalives (1 s,
//! 10 s timeout) catch a peer that vanished without tearing the connection
//! down.
//!
//! One connection per task: a failure or disconnect sets the link's error
//! flag (the game backs out through its own comm-error path) and reports
//! [`Status::Lost`]; the user reconnects from the UI.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::link::Link;

pub const PROTOCOL_VERSION: u32 = 1;
const MAGIC: &[u8; 4] = b"RING";
// v4: the four bytes after the version byte changed meaning from the ROM's
// crc32 to its header game code.
// v5: the app was renamed from bcclink to Ring; the magic shrank from
// b"BCCLINK" to b"RING", so the hello is 10 bytes now (old peers fail the
// magic check before ever reading the version byte).
const HELLO_VERSION: u8 = 5;
const KEEPALIVE: u8 = 0xff;

const DRAIN_INTERVAL: Duration = Duration::from_millis(4);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub enum Status {
    Idle,
    /// Dialing the matchmaking server.
    Signaling,
    /// Registered under the link code; the peer hasn't shown up yet.
    WaitingForPeer,
    /// Channel open and hello exchanged. `side` is ours (0 = parent).
    /// `cross_version` flags a peer running a different (still supported)
    /// ROM version — US↔JP crossplay.
    Connected {
        side: u8,
        cross_version: bool,
    },
    /// The connection ended (peer left, transport died, or hello failed).
    Lost(String),
}

pub struct ConnectParams {
    pub endpoint: String,
    pub link_code: String,
    pub game_code: [u8; 4],
}

/// Spawn the connect task on `rt`. Runs until the connection dies or
/// `cancel` fires; reports progress through `status` and ferries the link.
pub fn spawn_connect(
    rt: &tokio::runtime::Handle,
    params: ConnectParams,
    link: Arc<Link>,
    status: Arc<Mutex<Status>>,
    cancel: CancellationToken,
) {
    rt.spawn(async move {
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                *status.lock().unwrap() = Status::Idle;
                return;
            }
            r = run_connection(&params, &link, &status, &cancel) => r,
        };
        link.set_error();
        *status.lock().unwrap() = match result {
            Ok(()) => Status::Lost("opponent disconnected".to_owned()),
            Err(e) if cancel.is_cancelled() => {
                log::info!("connection closed on cancel: {e}");
                Status::Idle
            }
            Err(e) => {
                log::warn!("connection ended: {e}");
                Status::Lost(e.to_string())
            }
        };
    });
}

async fn run_connection(
    params: &ConnectParams,
    link: &Arc<Link>,
    status: &Arc<Mutex<Status>>,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    *status.lock().unwrap() = Status::Signaling;

    // Namespaced so a Ring code can never collide with a Tango lobby code
    // on the shared server.
    let session_id = format!("ring:{}", params.link_code.trim().to_lowercase());
    let connecting = tango_signaling::connect(
        &params.endpoint,
        &session_id,
        None, // let ICE pick: direct when possible, TURN when it isn't
        PROTOCOL_VERSION,
        vec![tango_rtc::ChannelConfig {
            label: "ring".to_owned(),
            ordered: true,
            reliable: true,
        }],
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("matchmaking: {e}"))?;

    *status.lock().unwrap() = Status::WaitingForPeer;
    let connected = connecting
        .await
        .map_err(|e| anyhow::anyhow!("webrtc: {e}"))?;

    // The peer connection must stay alive for the channel's lifetime; hold
    // it here until this function returns.
    let peer_conn = connected.peer_conn;
    let mut channels = connected.channels;
    let mut dc = channels
        .pop()
        .ok_or_else(|| anyhow::anyhow!("no data channel"))?;

    // Offerer = parent. The SDP roles are asymmetric by construction; the
    // hello's side byte double-checks that both ends resolved them that way.
    let side = if peer_conn
        .local_description()
        .map(|d| matches!(d.sdp_type, tango_rtc::SdpType::Offer))
        .unwrap_or(false)
    {
        0u8
    } else {
        1u8
    };

    let mut hello = Vec::new();
    hello.extend_from_slice(MAGIC);
    hello.push(HELLO_VERSION);
    hello.extend_from_slice(&params.game_code);
    hello.push(side);
    dc.send(&hello).await?;

    let peer_hello = tokio::time::timeout(RECEIVE_TIMEOUT, dc.receive())
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for hello"))?
        .ok_or_else(|| anyhow::anyhow!("channel closed during hello"))?;
    if peer_hello.len() != 10 || &peer_hello[..4] != MAGIC {
        anyhow::bail!("peer is not a compatible Ring instance");
    }
    if peer_hello[4] != HELLO_VERSION {
        anyhow::bail!(
            "peer Ring version differs (their hello v{}, ours v{HELLO_VERSION})",
            peer_hello[4]
        );
    }
    // A differing game code is allowed: Ring refuses to start an
    // unsupported ROM, so a mismatched peer is necessarily the other
    // region's build — US↔JP crossplay, which the cable never gated either.
    // Cross-version battles proved frame-exact in the cross selftest;
    // surface the fact anyway so a surprise has a visible cause.
    let peer_code: [u8; 4] = peer_hello[5..9].try_into().unwrap();
    let cross_version = peer_code != params.game_code;
    if cross_version {
        log::info!(
            "cross-version link: their game code {}, ours {}",
            String::from_utf8_lossy(&peer_code),
            String::from_utf8_lossy(&params.game_code)
        );
    }
    if peer_hello[9] == side {
        anyhow::bail!("role conflict (both sides resolved side {side}); reconnect");
    }

    log::info!("link up as side {side}");
    link.set_connected(side);
    *status.lock().unwrap() = Status::Connected {
        side,
        cross_version,
    };

    // Ferry until something dies. Sender: drain the link at a short interval
    // (the stream is a few bytes per turn; the game's own -1 "still waiting"
    // polling absorbs the latency) plus keepalives. Receiver: deliver, with a
    // timeout so a vanished peer is detected.
    let (mut dc_tx, mut dc_rx) = dc.split();
    let sender = {
        let link = link.clone();
        let cancel = cancel.clone();
        async move {
            let mut since_keepalive = Duration::ZERO;
            loop {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                let mut msgs = link.drain_outgoing();
                since_keepalive += DRAIN_INTERVAL;
                if since_keepalive >= KEEPALIVE_INTERVAL {
                    since_keepalive = Duration::ZERO;
                    msgs.push(vec![KEEPALIVE]);
                }
                for msg in msgs {
                    dc_tx.send(&msg).await?;
                }
                tokio::time::sleep(DRAIN_INTERVAL).await;
            }
        }
    };
    let receiver = {
        let link = link.clone();
        async move {
            loop {
                let msg = tokio::time::timeout(RECEIVE_TIMEOUT, dc_rx.receive())
                    .await
                    .map_err(|_| anyhow::anyhow!("peer stopped responding"))?
                    .ok_or_else(|| anyhow::anyhow!("channel closed"))?;
                if msg.first() != Some(&KEEPALIVE) {
                    link.deliver(&msg);
                }
            }
        }
    };

    let result = tokio::select! {
        r = sender => r.map_err(|e: std::io::Error| anyhow::anyhow!("send: {e}")),
        r = receiver => r,
    };
    peer_conn.abandon();
    match result {
        // "channel closed" without a transport error = the peer left.
        Err(e) if e.to_string() == "channel closed" => Ok(()),
        other => other,
    }
}
