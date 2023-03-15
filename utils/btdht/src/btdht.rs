use btdht::MainlineDht;
use futures_util::StreamExt;
use ouisync_lib::{
    network::{self, dht_discovery::DHT_ROUTERS},
    ShareToken,
};
use std::{
    collections::HashSet,
    io,
    net::{Ipv4Addr, Ipv6Addr},
};
use structopt::StructOpt;
use tokio::{net::UdpSocket, task};

/// Command line options.
#[derive(StructOpt, Debug)]
struct Options {
    /// Accept a share token.
    #[structopt(long, value_name = "TOKEN")]
    pub token: Option<ShareToken>,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let options = Options::from_args();

    env_logger::init();

    const WITH_IPV4: bool = true;
    const WITH_IPV6: bool = true;

    let socket_v4 = if WITH_IPV4 {
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()
    } else {
        None
    };

    // Note: [BEP-32](https://www.bittorrent.org/beps/bep_0032.html) says we should bind the ipv6
    // socket to a concrete unicast address, not to an unspecified one. Not sure it's worth it
    // though as (1) I'm not sure how multi-homing causes problems and (2) devices often change IP
    // addresses (switch to a different wifi, or cellular,...) so we would need a mechanism to
    // restart the DHT with a different socket if that happens.
    let socket_v6 = if WITH_IPV6 {
        UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).await.ok()
    } else {
        None
    };

    let dht_v4 = socket_v4.map(|socket| {
        MainlineDht::builder()
            .add_routers(DHT_ROUTERS.iter().copied())
            .set_read_only(true)
            .start(socket)
            .unwrap()
    });

    let dht_v6 = socket_v6.map(|socket| {
        MainlineDht::builder()
            .add_routers(DHT_ROUTERS.iter().copied())
            .set_read_only(true)
            .start(socket)
            .unwrap()
    });

    if let Some(token) = options.token {
        let task = task::spawn(async move {
            if let Some(dht) = dht_v4 {
                println!();
                lookup("IPv4", &dht, &token).await;
            }

            if let Some(dht) = dht_v6 {
                println!();
                lookup("IPv6", &dht, &token).await;
            }
        });

        task.await?;
    } else {
        // This never ends, useful mainly for debugging.
        std::future::pending::<()>().await;
    }

    Ok(())
}

async fn lookup(prefix: &str, dht: &MainlineDht, token: &ShareToken) {
    println!("{} Bootstrapping...", prefix);
    if dht.bootstrapped(None).await {
        let mut seen_peers = HashSet::new();
        let info_hash = network::repository_info_hash(token.id());

        println!("{} Searching for peers...", prefix);
        let mut peers = dht.search(info_hash, false);

        while let Some(peer) = peers.next().await {
            if seen_peers.insert(peer) {
                println!("  {:?}", peer);
            }
        }
    } else {
        println!("{} Bootstrap failed", prefix)
    }
}