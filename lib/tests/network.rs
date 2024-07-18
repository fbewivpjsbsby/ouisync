//! Networking tests

// These test mostly require QUIC / UDP which the simulator doesn't support yet.
#![cfg(not(feature = "simulation"))]

#[macro_use]
mod common;

use self::common::{actor, Env, Proto, DEFAULT_REPO, TEST_TIMEOUT};
use ouisync::network::{Network, PeerState};
use std::sync::Arc;
use tokio::{sync::Barrier, time};

// This test requires QUIC which is not yet supported in simulation
#[test]
fn peer_exchange_basics() {
    let mut env = Env::new();
    let proto = Proto::Quic; // PEX works only with QUIC
    let barrier = Arc::new(Barrier::new(3));

    // Bob and Carol are initially connected only to Alice but eventually they connect to each
    // other via peer exchange.

    env.actor("alice", {
        let barrier = barrier.clone();

        async move {
            let network = actor::create_network(proto).await;
            let (_repo, reg) = actor::create_linked_repo(DEFAULT_REPO, &network).await;
            reg.set_pex_enabled(true).await;

            barrier.wait().await;
        }
    });

    env.actor("bob", {
        let barrier = barrier.clone();

        async move {
            let network = actor::create_network(proto).await;
            let (_repo, reg) = actor::create_linked_repo(DEFAULT_REPO, &network).await;

            let peer_addr = actor::lookup_addr("alice").await;
            network.add_user_provided_peer(&peer_addr);

            reg.set_pex_enabled(true).await;

            expect_peer_known(&network, "carol").await;
            barrier.wait().await;
        }
    });

    env.actor("carol", {
        async move {
            let network = actor::create_network(proto).await;
            let (_repo, reg) = actor::create_linked_repo(DEFAULT_REPO, &network).await;

            let peer_addr = actor::lookup_addr("alice").await;
            network.add_user_provided_peer(&peer_addr);

            reg.set_pex_enabled(true).await;

            expect_peer_known(&network, "bob").await;
            barrier.wait().await;
        }
    });
}

#[test]
fn peer_exchange_discovers_only_peers_sharing_same_repository() {
    let mut env = Env::new();
    let proto = Proto::Quic; // PEX works only with QUIC
    let barrier = Arc::new(Barrier::new(4));

    // Alice has two repos - one shared with Bob and Carol and the other shared with Dave.
    // Bob, Carol and Dave initially know only Alice. Through peer exchange, Bob and Carol learn
    // about each other because they share the same repo, but they don't learn about Carol because
    // they share none with her.

    env.actor("alice", {
        let barrier = barrier.clone();

        async move {
            let network = actor::create_network(proto).await;

            let (_repo_1, reg_1) = actor::create_linked_repo("repo-1", &network).await;
            reg_1.set_pex_enabled(true).await;

            let (_repo_2, reg_2) = actor::create_linked_repo("repo-2", &network).await;
            reg_2.set_pex_enabled(true).await;

            barrier.wait().await;
            barrier.wait().await;
        }
    });

    env.actor("bob", {
        let barrier = barrier.clone();

        async move {
            let network = actor::create_network(proto).await;
            let (_repo, reg) = actor::create_linked_repo("repo-1", &network).await;

            let peer_addr = actor::lookup_addr("alice").await;
            network.add_user_provided_peer(&peer_addr);

            reg.set_pex_enabled(true).await;

            expect_peer_known(&network, "carol").await;

            barrier.wait().await;

            expect_peer_not_known(&network, "dave").await;

            barrier.wait().await;
        }
    });

    env.actor("carol", {
        let barrier = barrier.clone();

        async move {
            let network = actor::create_network(proto).await;
            let (_repo, reg) = actor::create_linked_repo("repo-1", &network).await;

            let peer_addr = actor::lookup_addr("alice").await;
            network.add_user_provided_peer(&peer_addr);

            reg.set_pex_enabled(true).await;

            expect_peer_known(&network, "bob").await;

            barrier.wait().await;

            expect_peer_not_known(&network, "dave").await;

            barrier.wait().await;
        }
    });

    env.actor("dave", {
        async move {
            let network = actor::create_network(proto).await;
            let (_repo, reg) = actor::create_linked_repo("repo-2", &network).await;

            let peer_addr = actor::lookup_addr("alice").await;
            network.add_user_provided_peer(&peer_addr);

            reg.set_pex_enabled(true).await;

            barrier.wait().await;

            expect_peer_not_known(&network, "bob").await;
            expect_peer_not_known(&network, "carol").await;

            barrier.wait().await;
        }
    });
}

// Test for an edge case that used to panic.
#[test]
fn dht_toggle() {
    let mut env = Env::new();
    let proto = Proto::Quic;

    env.actor("eric", async move {
        let network = actor::create_network(proto).await;
        let (_repo, reg) = actor::create_linked_repo(DEFAULT_REPO, &network).await;

        reg.set_dht_enabled(true).await;
        reg.set_dht_enabled(false).await;
        reg.set_dht_enabled(true).await;
    });
}

#[test]
fn local_discovery() {
    let mut env = Env::new();
    let proto = Proto::Quic;
    let barrier = Arc::new(Barrier::new(2));

    // The peers are initially disconnected and don't know each other's socket addesses.
    // They eventually discover each other via local discovery.

    for (src, dst) in [("alice", "bob"), ("bob", "alice")] {
        let barrier = barrier.clone();

        env.actor(src, async move {
            let network = actor::create_network(proto).await;
            network.set_local_discovery_enabled(true);

            let dst_port = actor::lookup_addr(dst).await.port();

            // Note we compare only the ports because we bind to `LOCALHOST` (127.0.0.1) but local
            // discovery produces the actual LAN addresses which we don't know in advance (or
            // rather can't be bothered to find out). Comparing the ports should be enough to test
            // that local discovery works.
            expect_knows_port(&network, dst_port).await;

            barrier.wait().await;
        });
    }
}

#[test]
fn add_peer_before_bind() {
    let mut env = Env::new();
    let proto = Proto::Quic;
    let barrier = Arc::new(Barrier::new(2));

    env.actor("alice", {
        let barrier = barrier.clone();

        async move {
            let network = actor::create_network(proto).await;
            expect_peer_active(&network, "bob").await;

            barrier.wait().await;
        }
    });

    env.actor("bob", {
        async move {
            let network = actor::create_unbound_network();

            let peer_addr = actor::lookup_addr("alice").await;
            network.add_user_provided_peer(&peer_addr);
            expect_peer_known(&network, "alice").await;

            actor::bind(&network, proto).await;
            expect_peer_active(&network, "alice").await;

            barrier.wait().await;
        }
    });
}

async fn expect_peer_known(network: &Network, peer_name: &str) {
    expect_peer_state(network, peer_name, |_| true).await
}

async fn expect_peer_active(network: &Network, peer_name: &str) {
    expect_peer_state(network, peer_name, |state| {
        matches!(state, PeerState::Active { .. })
    })
    .await
}

async fn expect_peer_state<F>(network: &Network, peer_name: &str, expected_state_fn: F)
where
    F: Fn(&PeerState) -> bool,
{
    time::timeout(*TEST_TIMEOUT, async move {
        let mut rx = network.on_peer_set_change();
        let peer_addr = actor::lookup_addr(peer_name).await;

        loop {
            if let Some(info) = network.peer_info(peer_addr) {
                if expected_state_fn(&info.state) {
                    break;
                }
            }

            rx.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}

async fn expect_knows_port(network: &Network, peer_port: u16) {
    let collector = network.peer_info_collector();

    time::timeout(*TEST_TIMEOUT, async move {
        let mut rx = network.on_peer_set_change();

        loop {
            let mut peer_ports = collector.collect().into_iter().map(|info| info.addr.port());

            if peer_ports.any(|port| port == peer_port) {
                break;
            }

            rx.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}

async fn expect_peer_not_known(network: &Network, peer_name: &str) {
    let peer_addr = actor::lookup_addr(peer_name).await;

    if let Some(info) = network.peer_info(peer_addr) {
        error!("unexpected known peer {peer_name}: {info:?}");
        panic!("unexpected known peer {peer_name}: {info:?}");
    }
}
