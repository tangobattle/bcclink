//! End-to-end exercise of the matchmaking transport: two clients in one
//! process, a random link code, the real signaling server, a real WebRTC
//! channel. Verifies pairing, side assignment, the hello, and barrier bytes
//! flowing both ways.
//!
//! Needs network access (and the matchmaking server up).
//!
//! Run: cargo run --release --example mmtest [-- wss://server]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ring_bcc::link::{BlockKind, Link, Poll};
use ring_bcc::net;
use tokio_util::sync::CancellationToken;

fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let endpoint = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "wss://matchmaking.tango.n1gp.net".to_owned());
    let code = format!("mmtest-{:08x}", rand::random::<u32>());
    println!("[mmtest] endpoint {endpoint}, code {code}");

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let cancel = CancellationToken::new();
    let links = [Arc::new(Link::new()), Arc::new(Link::new())];
    let statuses = [
        Arc::new(Mutex::new(net::Status::Idle)),
        Arc::new(Mutex::new(net::Status::Idle)),
    ];
    for i in 0..2 {
        net::spawn_connect(
            rt.handle(),
            net::ConnectParams {
                endpoint: endpoint.clone(),
                link_code: code.clone(),
                game_code: *b"A89E",
            },
            links[i].clone(),
            statuses[i].clone(),
            cancel.clone(),
        );
    }

    let sides: Vec<u8> = (0..2)
        .map(|i| {
            wait_for(&format!("client {i} connected"), Duration::from_secs(60), || {
                match &*statuses[i].lock().unwrap() {
                    net::Status::Connected { side, .. } => Some(*side),
                    net::Status::Lost(e) => panic!("client {i} lost: {e}"),
                    _ => None,
                }
            })
        })
        .collect();
    assert_ne!(sides[0], sides[1], "sides must be complementary");
    println!("[mmtest] both connected: sides {:?}", sides);

    // A mode exchange through the real channel, like the drvA phase does.
    links[0].open_handshake(0);
    links[1].open_handshake(1);
    for i in 0..2 {
        let peer_mode = wait_for("mode exchange", Duration::from_secs(5), || {
            links[i].peer_block(BlockKind::Mode)
        });
        assert_eq!(peer_mode, vec![1 - i as u8]);
    }
    println!("[mmtest] mode exchange ok");

    // Barrier bytes through the real channel, both directions, in order.
    for byte in [10u8, 20, 30] {
        links[0].push_barrier(byte);
        links[1].push_barrier(byte ^ 0xff);
    }
    for want in [10u8, 20, 30] {
        let got = wait_for("barrier 0→1", Duration::from_secs(10), || match links[1].poll_barrier() {
            Poll::Byte(b) => Some(b),
            _ => None,
        });
        assert_eq!(got, want);
        let got = wait_for("barrier 1→0", Duration::from_secs(10), || match links[0].poll_barrier() {
            Poll::Byte(b) => Some(b),
            _ => None,
        });
        assert_eq!(got, want ^ 0xff);
    }
    println!("[mmtest] barrier relay ok over the real channel");

    cancel.cancel();
    std::thread::sleep(Duration::from_millis(200));
    println!("[mmtest] RESULT: OK");
}
