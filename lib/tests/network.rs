//! Networking tests

// These test mostly require QUIC / UDP which the simulator doesn't support yet.
#![cfg(not(feature = "simulation"))]

mod common;

use self::common::{actor, Env, NetworkExt, Proto, TEST_TIMEOUT};
use ouisync::network::{Network, PeerState};
use std::{net::Ipv4Addr, sync::Arc};
use tokio::{sync::Barrier, time};

// This test requires QUIC which is not yet supported in simulation
#[test]
fn peer_exchange() {
    let mut env = Env::new();
    let proto = Proto::Quic; // PEX works only with QUIC
    let barrier = Arc::new(Barrier::new(3));

    // Bob and Carol are initially connected only to Alice but eventually they connect to each
    // other via peer exchange.

    env.actor("alice", {
        let barrier = barrier.clone();

        async move {
            let network = actor::create_network(proto).await;
            let (_repo, reg) = actor::create_linked_repo(&network).await;
            reg.enable_pex();

            barrier.wait().await;
        }
    });

    env.actor("bob", {
        let barrier = barrier.clone();

        async move {
            let network = actor::create_network(proto).await;
            let (_repo, reg) = actor::create_linked_repo(&network).await;
            network.connect("alice");
            reg.enable_pex();

            expect_peer_known(&network, proto, "carol").await;
            barrier.wait().await;
        }
    });

    env.actor("carol", {
        async move {
            let network = actor::create_network(proto).await;
            let (_repo, reg) = actor::create_linked_repo(&network).await;
            network.connect("alice");
            reg.enable_pex();

            expect_peer_known(&network, proto, "bob").await;
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
        let (_repo, reg) = actor::create_linked_repo(&network).await;

        reg.enable_dht();
        reg.disable_dht();
        reg.enable_dht();
    });
}

#[test]
fn local_discovery() {
    let mut env = Env::new();
    let proto = Proto::Quic;
    let barrier = Arc::new(Barrier::new(2));

    // The peers are initially disconnected and don't know each other's socket addesses.
    // They eventually discover each other via local discovery.

    for (src_port, dst_port) in [(7001, 7002), (7002, 7001)] {
        let barrier = barrier.clone();

        env.actor(&format!("node-{src_port}"), async move {
            let network = actor::create_unbound_network();
            network
                .bind(&[proto.wrap((Ipv4Addr::LOCALHOST, src_port))])
                .await;

            network.set_local_discovery_enabled(true);

            // Note we compare only the ports because we bind to `LOCALHOST` (127.0.0.1) but local
            // discovery produces the actual LAN addresses which we don't know in advance (or
            // rather can't be bothered to find out). Comparing the ports should be enough to test
            // that local discovery works.
            expect_knows_port(&network, dst_port).await;

            barrier.wait().await;
        });
    }
}

// FIXME: this currently fails
#[ignore]
#[test]
fn add_peer_before_bind() {
    let mut env = Env::new();
    let proto = Proto::Quic;
    let barrier = Arc::new(Barrier::new(2));

    env.actor("alice", {
        let barrier = barrier.clone();

        async move {
            let network = actor::create_network(proto).await;
            expect_peer_active(&network, proto, "bob").await;

            barrier.wait().await;
        }
    });

    env.actor("bob", {
        async move {
            let network = actor::create_unbound_network();

            // NOTE: Can't use `NetworkExt::connect` here because it requires the network to be
            // bound.
            network.add_user_provided_peer(&proto.wrap(actor::lookup("alice")));
            expect_peer_known(&network, proto, "alice").await;

            network.bind(&[actor::default_bind_addr(proto)]).await;
            expect_peer_active(&network, proto, "alice").await;

            barrier.wait().await;
        }
    });
}

async fn expect_peer_known(network: &Network, peer_proto: Proto, peer_name: &str) {
    expect_peer_state(network, peer_proto, peer_name, |_| true).await
}

async fn expect_peer_active(network: &Network, peer_proto: Proto, peer_name: &str) {
    expect_peer_state(network, peer_proto, peer_name, |state| {
        matches!(state, PeerState::Active(_))
    })
    .await
}

async fn expect_peer_state<F>(
    network: &Network,
    peer_proto: Proto,
    peer_name: &str,
    expected_state_fn: F,
) where
    F: Fn(&PeerState) -> bool,
{
    time::timeout(TEST_TIMEOUT, async move {
        let mut rx = network.on_peer_set_change();
        let peer_addr = peer_proto.wrap(actor::lookup(peer_name));

        loop {
            if let Some(info) = network.get_peer_info(peer_addr) {
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
    time::timeout(TEST_TIMEOUT, async move {
        let mut rx = network.on_peer_set_change();

        loop {
            let mut peer_ports = network
                .collect_peer_info()
                .into_iter()
                .map(|info| info.port);

            if peer_ports.any(|port| port == peer_port) {
                break;
            }

            rx.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}
