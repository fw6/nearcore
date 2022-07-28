use crate::network_protocol::testonly as data;
use crate::network_protocol::{Encoding, PeerAddr, SyncAccountsData};
use crate::peer;
use crate::peer::peer_actor;
use crate::peer_manager;
use crate::peer_manager::peer_manager_actor::Event as PME;
use crate::peer_manager::testonly::Event;
use crate::testonly::{make_rng, AsSet as _, Rng};
use crate::types::{PeerMessage, RoutingTableUpdate};
use itertools::Itertools;
use near_logger_utils::init_test_logger;
use near_network_primitives::config;
use near_network_primitives::time;
use near_network_primitives::types::{Ping, RoutedMessageBody};
use near_primitives::network::PeerId;
use pretty_assertions::assert_eq;
use rand::Rng as _;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::net::TcpStream;

// After the initial exchange, all subsequent SyncRoutingTable messages are
// expected to contain only the diff of the known data.
#[tokio::test]
async fn repeated_data_in_sync_routing_table() {
    init_test_logger();
    let mut rng = make_rng(921853233);
    let rng = &mut rng;
    let mut clock = time::FakeClock::default();
    let chain = Arc::new(data::Chain::make(&mut clock, rng, 10));
    let pm = peer_manager::testonly::start(rng, chain.clone()).await;
    let cfg = peer::testonly::PeerConfig {
        signer: data::make_signer(rng),
        chain,
        peers: vec![],
        start_handshake_with: Some(PeerId::new(pm.cfg.node_key.public_key())),
        force_encoding: Some(Encoding::Proto),
    };
    let stream = TcpStream::connect(pm.cfg.node_addr.unwrap()).await.unwrap();
    let mut peer =
        peer::testonly::PeerHandle::start_endpoint(clock.clock(), rng, cfg, stream).await;
    let edge = peer.complete_handshake().await;

    let mut edges_got = HashSet::new();
    let mut edges_want = HashSet::new();
    let mut accounts_got = HashSet::new();
    let mut accounts_want = HashSet::new();
    edges_want.insert(edge);

    // Gradually increment the amount of data in the system and then broadcast it.
    for _ in 0..10 {
        // Wait for the new data to be broadcasted.
        // Note that in the first iteration we expect just 1 edge, without sending anything before.
        // It is important because the first SyncRoutingTable contains snapshot of all data known to
        // the node (not just the diff), so we expect incremental behavior only after the first
        // SyncRoutingTable.
        // TODO(gprusak): the first SyncRoutingTable will be delayed, until we replace actix
        // internal clock with a fake clock.
        while edges_got != edges_want || accounts_got != accounts_want {
            match peer.events.recv().await {
                peer::testonly::Event::RoutingTable(got) => {
                    for a in got.accounts {
                        assert!(!accounts_got.contains(&a), "repeated broadcast: {a:?}");
                        assert!(accounts_want.contains(&a), "unexpected broadcast: {a:?}");
                        accounts_got.insert(a);
                    }
                    for e in got.edges {
                        assert!(!edges_got.contains(&e), "repeated broadcast: {e:?}");
                        assert!(edges_want.contains(&e), "unexpected broadcast: {e:?}");
                        edges_got.insert(e);
                    }
                }
                // Ignore other messages.
                _ => {}
            }
        }
        // Add more data.
        let signer = data::make_signer(rng);
        edges_want.insert(data::make_edge(&peer.cfg.signer, &signer));
        accounts_want.insert(data::make_announce_account(rng));
        // Send all the data created so far. PeerManager is expected to discard the duplicates.
        peer.send(PeerMessage::SyncRoutingTable(RoutingTableUpdate {
            edges: edges_want.iter().cloned().collect(),
            accounts: accounts_want.iter().cloned().collect(),
        }))
        .await;
    }
}

// test that TTL is handled property.
#[tokio::test]
async fn ttl() {
    init_test_logger();
    let mut rng = make_rng(921853233);
    let rng = &mut rng;
    let mut clock = time::FakeClock::default();
    let chain = Arc::new(data::Chain::make(&mut clock, rng, 10));
    let mut pm = peer_manager::testonly::start(rng, chain.clone()).await;
    let cfg = peer::testonly::PeerConfig {
        signer: data::make_signer(rng),
        chain,
        peers: vec![],
        start_handshake_with: Some(PeerId::new(pm.cfg.node_key.public_key())),
        force_encoding: Some(Encoding::Proto),
    };
    let stream = TcpStream::connect(pm.cfg.node_addr.unwrap()).await.unwrap();
    let mut peer =
        peer::testonly::PeerHandle::start_endpoint(clock.clock(), rng, cfg, stream).await;
    peer.complete_handshake().await;
    // await for peer manager to compute the routing table.
    // TODO(gprusak): probably extract it to a separate function when migrating other tests from
    // integration-tests to near_network.
    pm.events
        .recv_until(|ev| match ev {
            Event::PeerManager(PME::RoutingTableUpdate(rt)) => {
                if rt.get(&peer.cfg.id()).map_or(false, |v| v.len() > 0) {
                    Some(())
                } else {
                    None
                }
            }
            _ => None,
        })
        .await;

    for ttl in 0..5 {
        let msg = RoutedMessageBody::Ping(Ping { nonce: rng.gen(), source: peer.cfg.id() });
        let msg = peer.routed_message(msg, peer.cfg.id(), ttl, Some(clock.now_utc()));
        peer.send(PeerMessage::Routed(msg.clone())).await;
        // If TTL is <2, then the message will be dropped (at least 2 hops are required).
        if ttl < 2 {
            pm.events
                .recv_until(|ev| match ev {
                    Event::PeerManager(PME::RoutedMessageDropped) => Some(()),
                    _ => None,
                })
                .await;
        } else {
            let got = peer
                .events
                .recv_until(|ev| match ev {
                    peer::testonly::Event::Routed(msg) => Some(msg),
                    _ => None,
                })
                .await;
            assert_eq!(msg.body, got.body);
            assert_eq!(msg.ttl - 1, got.ttl);
        }
    }
}

async fn add_peer(
    clock: &time::Clock,
    rng: &mut Rng,
    chain: Arc<data::Chain>,
    cfg: &config::NetworkConfig,
) -> (peer::testonly::PeerHandle, SyncAccountsData) {
    let peer_cfg = peer::testonly::PeerConfig {
        signer: data::make_signer(rng),
        chain,
        peers: vec![],
        start_handshake_with: Some(PeerId::new(cfg.node_key.public_key())),
        force_encoding: Some(Encoding::Proto),
    };
    let stream = TcpStream::connect(cfg.node_addr.unwrap()).await.unwrap();
    let mut peer =
        peer::testonly::PeerHandle::start_endpoint(clock.clone(), rng, peer_cfg, stream).await;
    peer.complete_handshake().await;
    // TODO(gprusak): this should be part of complete_handshake, once Borsh support is removed.
    let msg = match peer.events.recv().await {
        peer::testonly::Event::Peer(peer_actor::Event::MessageProcessed(
            PeerMessage::SyncAccountsData(msg),
        )) => msg,
        ev => panic!("expected SyncAccountsData, got {ev:?}"),
    };
    (peer, msg)
}

#[tokio::test]
async fn accounts_data_broadcast() {
    init_test_logger();
    let mut rng = make_rng(921853233);
    let rng = &mut rng;
    let mut clock = time::FakeClock::default();
    let chain = Arc::new(data::Chain::make(&mut clock, rng, 10));
    let clock = clock.clock();
    let clock = &clock;

    let pm = peer_manager::testonly::start(rng, chain.clone()).await;

    let take_sync = |ev| match ev {
        peer::testonly::Event::Peer(peer_actor::Event::MessageProcessed(
            PeerMessage::SyncAccountsData(msg),
        )) => Some(msg),
        _ => None,
    };

    let data = chain.make_tier1_data(rng, clock);

    // Connect peer, expect initial sync to be empty.
    let (mut peer1, got1) = add_peer(clock, rng, chain.clone(), &pm.cfg).await;
    assert_eq!(got1.accounts_data, vec![]);

    // Send some data and wait for it to be broadcasted back.
    let msg = SyncAccountsData {
        accounts_data: vec![data[0].clone(), data[1].clone()],
        incremental: true,
        requesting_full_sync: false,
    };
    let want = msg.accounts_data.clone();
    peer1.send(PeerMessage::SyncAccountsData(msg)).await;
    let got1 = peer1.events.recv_until(take_sync).await;
    assert_eq!(got1.accounts_data.as_set(), want.as_set());

    // Connect another peer and perform initial full sync.
    let (mut peer2, got2) = add_peer(clock, rng, chain.clone(), &pm.cfg).await;
    assert_eq!(got2.accounts_data.as_set(), want.as_set());

    // Send a mix of new and old data. Only new data should be broadcasted.
    let msg = SyncAccountsData {
        accounts_data: vec![data[1].clone(), data[2].clone()],
        incremental: true,
        requesting_full_sync: false,
    };
    let want = vec![data[2].clone()];
    peer1.send(PeerMessage::SyncAccountsData(msg)).await;
    let got1 = peer1.events.recv_until(take_sync).await;
    let got2 = peer2.events.recv_until(take_sync).await;
    assert_eq!(got1.accounts_data.as_set(), want.as_set());
    assert_eq!(got2.accounts_data.as_set(), want.as_set());

    // Send a request for a full sync.
    let want = vec![data[0].clone(), data[1].clone(), data[2].clone()];
    peer1
        .send(PeerMessage::SyncAccountsData(SyncAccountsData {
            accounts_data: vec![],
            incremental: true,
            requesting_full_sync: true,
        }))
        .await;
    let got1 = peer1.events.recv_until(take_sync).await;
    assert_eq!(got1.accounts_data.as_set(), want.as_set());
}

fn peer_addrs(vc: &config::ValidatorConfig) -> Vec<PeerAddr> {
    match &vc.endpoints {
        config::ValidatorEndpoints::PublicAddrs(addrs) => {
            addrs.iter().cloned().map(|addr| PeerAddr { addr, peer_id: None }).collect()
        }
        config::ValidatorEndpoints::TrustedStunServers(_) => {
            panic!("tests only support PublicAddrs in validator config")
        }
    }
}

// Test with 3 peer managers connected sequentially: 1-2-3
// All of them are validators.
// No matter what the order of shifting into the epoch,
// all of them should receive all the AccountDatas eventually.
#[tokio::test]
async fn accounts_data_gradual_epoch_change() {
    init_test_logger();
    let mut rng = make_rng(921853233);
    let rng = &mut rng;
    let mut clock = time::FakeClock::default();
    let chain = Arc::new(data::Chain::make(&mut clock, rng, 10));

    // It is an array [_;3] which allows compiler to borrow
    // each position separately. If it was a Vec, the rest
    // of the test would be way harder to express.
    let mut pms = [
        peer_manager::testonly::start(rng, chain.clone()).await,
        peer_manager::testonly::start(rng, chain.clone()).await,
        peer_manager::testonly::start(rng, chain.clone()).await,
    ];

    // 0 <-> 1 <-> 2
    pms[0].connect_to(&pms[1].peer_info()).await;
    pms[1].connect_to(&pms[2].peer_info()).await;

    // Validator configs.
    let vs: Vec<_> = pms.iter().map(|pm| pm.cfg.validator.clone().unwrap()).collect();

    // For every order of nodes.
    for ids in (0..pms.len()).permutations(3) {
        // Construct ChainInfo for a new epoch,
        // with tier1_accounts containing all validators.
        let e = data::make_epoch_id(rng);
        let mut chain_info = chain.get_chain_info();
        chain_info.tier1_accounts = Arc::new(
            vs.iter()
                .map(|v| ((e.clone(), v.signer.validator_id().clone()), v.signer.public_key()))
                .collect(),
        );

        // Advance epoch in the given order.
        for id in ids {
            pms[id].set_chain_info(chain_info.clone()).await;
        }

        // Wait for data to arrive.
        let want = vs
            .iter()
            .map(|v| ((e.clone(), v.signer.validator_id().clone()), peer_addrs(&v)))
            .collect();
        for pm in &mut pms {
            pm.wait_for_accounts_data(&want).await;
        }
    }
}
