//! Disposable regtest host for the unmodified pinned LDK. Only public APIs;
//! funding, blocks and process control are supplied by the independent driver.
use super::{bytes, hex, FixtureLogger};
use bitcoin::{Network, Transaction, Block, consensus::{deserialize, serialize}, secp256k1::PublicKey};
use lightning::chain::{self, BlockLocator, Listen, Watch, chaininterface::{BroadcasterInterface, FeeEstimator, ConfirmationTarget, TransactionType}, chainmonitor::ChainMonitor};
use lightning::events::{Event, EventsProvider, PaymentPurpose};
use lightning::ln::{channelmanager::{SimpleArcChannelManager, ChannelManager, ChainParameters, ChannelManagerReadArgs, PaymentId, OptionalOfferPaymentParams}, peer_handler::{MessageHandler, PeerManager, IgnoringMessageHandler}, types::ChannelId};
use lightning::onion_message::messenger::{OnionMessenger, SimpleArcOnionMessenger, DefaultMessageRouter};
use lightning::routing::{gossip::{NetworkGraph, P2PGossipSync}, router::DefaultRouter, scoring::{ProbabilisticScorer, ProbabilisticScoringDecayParameters, ProbabilisticScoringFeeParameters}};
use lightning::sign::{KeysManager, InMemorySigner, EntropySource, NodeSigner};
use lightning::util::{config::UserConfig, persist::{KVStoreSync, read_channel_monitors}, ser::{Writeable, Readable, ReadableArgs, LengthReadable, FixedLengthReader}};
use lightning::blinded_path::message::{BlindedMessagePath, NextMessageHop};
use lightning::offers::{offer::Offer, static_invoice::StaticInvoice};
use lightning_persister::fs_store::v1::FilesystemStore;
use serde_json::{Value, json};
use std::{sync::{Arc, Mutex, RwLock}, path::PathBuf, time::{Duration, SystemTime, UNIX_EPOCH}, str::FromStr};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

struct Fees;
impl FeeEstimator for Fees { fn get_est_sat_per_1000_weight(&self, _: ConfirmationTarget) -> u32 { 1000 } }
struct Broadcast(Mutex<Vec<Transaction>>);
impl BroadcasterInterface for Broadcast { fn broadcast_transactions(&self, txs: &[(&Transaction, TransactionType)]) { self.0.lock().unwrap().extend(txs.iter().map(|(tx, _)| (*tx).clone())); } }
struct FullBlocks;
impl chain::Filter for FullBlocks {
    fn register_tx(&self, _: &bitcoin::Txid, _: &bitcoin::Script) {}
    fn register_output(&self, _: chain::WatchedOutput) {}
}
type Keys = KeysManager<Arc<FixtureLogger>>;
type Monitor = ChainMonitor<InMemorySigner<Arc<FixtureLogger>>, Arc<FullBlocks>, Arc<Broadcast>, Arc<Fees>, Arc<FixtureLogger>, Arc<FilesystemStore>, Arc<Keys>>;
type Manager = SimpleArcChannelManager<Monitor, Broadcast, Fees, FixtureLogger>;
type Messenger = SimpleArcOnionMessenger<Monitor, Broadcast, Fees, FixtureLogger>;
type Gossip = P2PGossipSync<Arc<NetworkGraph<Arc<FixtureLogger>>>, Arc<CoreOutputs>, Arc<FixtureLogger>>;
type Peers = PeerManager<lightning_net_tokio::SocketDescriptor, Arc<Manager>, Arc<Gossip>, Arc<Messenger>, Arc<FixtureLogger>, IgnoringMessageHandler, Arc<Keys>, Arc<Monitor>>;
struct CoreOutputs(Arc<FilesystemStore>);
impl lightning::routing::utxo::UtxoLookup for CoreOutputs {
    fn get_utxo(&self, chain: &bitcoin::constants::ChainHash, scid: u64, _: Arc<lightning::util::wakers::Notifier>) -> lightning::routing::utxo::UtxoResult {
        use lightning::routing::utxo::{UtxoResult, UtxoLookupError};
        if *chain != bitcoin::constants::ChainHash::using_genesis_block(Network::Regtest) { return UtxoResult::Sync(Err(UtxoLookupError::UnknownChain)); }
        let output = self.0.read("core_outputs", "", &scid.to_string()).ok().and_then(|raw| deserialize(&raw).ok()).ok_or(UtxoLookupError::UnknownTx);
        UtxoResult::Sync(output)
    }
}
struct Node {
    manager: Arc<Manager>, monitor: Arc<Monitor>, messenger: Arc<Messenger>, peers: Arc<Peers>,
    store: Arc<FilesystemStore>, broadcast: Arc<Broadcast>, events: Mutex<Vec<Value>>,
}
impl Node {
    fn new(root: PathBuf, seed: u8, client: bool) -> Result<Self, String> {
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let logger = Arc::new(FixtureLogger);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let keys = Arc::new(KeysManager::new(&[seed;32], now.as_secs(), now.subsec_nanos(), false, logger.clone()));
        let store = Arc::new(FilesystemStore::new(root));
        let fees = Arc::new(Fees); let broadcast = Arc::new(Broadcast(Mutex::new(Vec::new())));
        let monitor = Arc::new(Monitor::new(None, broadcast.clone(), logger.clone(), fees.clone(), store.clone(), keys.clone(), keys.get_peer_storage_key(), false));
        let graph = Arc::new(NetworkGraph::new(Network::Regtest, logger.clone()));
        let scorer = Arc::new(RwLock::new(ProbabilisticScorer::new(ProbabilisticScoringDecayParameters::default(), graph.clone(), logger.clone())));
        let router = Arc::new(DefaultRouter::new(graph.clone(), logger.clone(), keys.clone(), scorer, ProbabilisticScoringFeeParameters::default()));
        let message_router = Arc::new(DefaultMessageRouter::new(graph.clone(), keys.clone()));
        let mut config = UserConfig::default();
        config.channel_handshake_config.announce_for_forwarding = !client;
        config.channel_handshake_config.negotiate_anchors_zero_fee_htlc_tx = false;
        config.channel_handshake_limits.force_announced_channel_preference = client;
        config.accept_forwards_to_priv_channels = true;
        config.enable_htlc_hold = !client;
        config.hold_outbound_htlcs_at_next_hop = client;
        config.channel_config.forwarding_fee_base_msat = 1000;
        config.channel_config.forwarding_fee_proportional_millionths = 0;
        config.channel_config.cltv_expiry_delta = 48;
        let monitors = read_channel_monitors(store.clone(), keys.clone(), keys.clone()).map_err(|e| e.to_string())?;
        let manager = match store.read("", "", "manager") {
            Ok(raw) => {
                let args = ChannelManagerReadArgs::new(keys.clone(), keys.clone(), keys.clone(), fees.clone(), monitor.clone(), broadcast.clone(), router, message_router.clone(), logger.clone(), config, monitors.iter().map(|(_, m)| m).collect());
                let (_, manager): (BlockLocator, Manager) = ReadableArgs::read(&mut raw.as_slice(), args).map_err(|e| format!("restore manager: {e:?}"))?;
                manager
            },
            Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => {
                if !monitors.is_empty() { return Err("missing manager with existing monitors".into()); }
                ChannelManager::new(fees, monitor.clone(), broadcast.clone(), router, message_router.clone(), logger.clone(), keys.clone(), keys.clone(), keys.clone(), config,
                    ChainParameters { network: Network::Regtest, best_block: BlockLocator::from_network(Network::Regtest) }, now.as_secs() as u32)
            },
            Err(e) => return Err(e.to_string()),
        };
        for (_, mon) in monitors { monitor.watch_channel(mon.channel_id(), mon).map_err(|e| format!("restore monitor {e:?}"))?; }
        let manager = Arc::new(manager);
        let messenger = Arc::new(OnionMessenger::new_with_offline_peer_interception(keys.clone(), keys.clone(), logger.clone(), manager.clone(), message_router,
            manager.clone(), manager.clone(), IgnoringMessageHandler {}, IgnoringMessageHandler {}, false));
        // Funding outputs are populated exclusively from Bitcoin Core's mined
        // blocks; LDK verifies gossip signatures and the real funding scripts.
        let gossip = Arc::new(P2PGossipSync::new(graph, Some(Arc::new(CoreOutputs(store.clone()))), logger.clone()));
        let handler = MessageHandler { chan_handler: manager.clone(), route_handler: gossip, onion_message_handler: messenger.clone(), custom_message_handler: IgnoringMessageHandler {}, send_only_message_handler: monitor.clone() };
        let peers = Arc::new(PeerManager::new(handler, now.as_secs() as u32, &keys.get_secure_random_bytes(), logger, keys));
        Ok(Self { manager, monitor, messenger, peers, store, broadcast, events: Mutex::new(Vec::new()) })
    }
    fn persist(&self) { self.store.write("", "", "manager", self.manager.encode()).expect("manager persistence"); }
    fn handle(&self, event: Event) {
        let description = format!("{event:?}");
        match event {
            Event::FundingGenerationReady { temporary_channel_id, counterparty_node_id, channel_value_satoshis, output_script, .. } => self.events.lock().unwrap().push(json!({"event":"funding", "temporary_id":hex(&temporary_channel_id.0),"peer":counterparty_node_id.to_string(),"amount":channel_value_satoshis,"script":hex(output_script.as_bytes())})),
            Event::OpenChannelRequest { temporary_channel_id, counterparty_node_id, .. } => {
                self.manager.accept_inbound_channel(&temporary_channel_id, &counterparty_node_id, 1, None).expect("accept inbound");
            },
            Event::PaymentClaimable { payment_hash, amount_msat, purpose, onion_fields, .. } => {
                let preimage = match purpose { PaymentPurpose::SpontaneousPayment(p) => Some(p), PaymentPurpose::Bolt12OfferPayment { payment_preimage, .. } => payment_preimage, _ => None };
                let wire_amount: u64 = self.manager.list_channels().iter().flat_map(|c| &c.pending_inbound_htlcs)
                    .filter(|h| h.payment_hash == payment_hash).map(|h| h.amount_msat).sum();
                self.events.lock().unwrap().push(json!({"event":"claimable", "hash":hex(&payment_hash.0), "amount":amount_msat,
                    "wire_amount_msat":wire_amount,"intended_msat":onion_fields.as_ref().map(|f| f.total_mpp_amount_msat)}));
                if let Some(preimage) = preimage { self.manager.claim_funds(preimage); }
            },
            Event::PaymentClaimed { payment_hash, amount_msat, .. } => self.events.lock().unwrap().push(json!({"event":"received", "hash":hex(&payment_hash.0), "amount":amount_msat})),
            Event::PaymentForwarded { total_fee_earned_msat, outbound_amount_forwarded_msat, claim_from_onchain_tx, .. } => {
                self.events.lock().unwrap().push(json!({"event":"forwarded", "fee":total_fee_earned_msat,"outbound_msat":outbound_amount_forwarded_msat,"onchain":claim_from_onchain_tx}));
            },
            Event::PaymentSent { payment_hash, payment_preimage, fee_paid_msat, .. } => self.events.lock().unwrap().push(json!({"event":"sent", "hash":hex(&payment_hash.0), "preimage":hex(&payment_preimage.0), "fee":fee_paid_msat})),
            Event::PersistStaticInvoice { invoice, invoice_request_path, invoice_slot, recipient_id, invoice_persisted_path } => {
                let name = format!("{}-{invoice_slot}", hex(&recipient_id));
                let record = json!({"invoice":hex(&invoice.encode()),"path":hex(&invoice_request_path.encode())});
                self.store.write("invoices", "", &name, serde_json::to_vec(&record).unwrap()).expect("invoice persistence");
                self.manager.static_invoice_persisted(invoice_persisted_path);
                self.events.lock().unwrap().push(json!({"event":"invoice_persisted", "slot":invoice_slot,"recipient":hex(&recipient_id)}));
            },
            Event::StaticInvoiceRequested { recipient_id, invoice_slot, reply_path, invoice_request } => {
                let name = format!("{}-{invoice_slot}", hex(&recipient_id));
                let raw = self.store.read("invoices", "", &name).expect("requested invoice");
                let saved: Value = serde_json::from_slice(&raw).unwrap();
                let invoice = StaticInvoice::try_from(bytes(&saved["invoice"]).unwrap()).unwrap();
                let path = BlindedMessagePath::read(&mut bytes(&saved["path"]).unwrap().as_slice()).unwrap();
                self.manager.respond_to_static_invoice_request(invoice, reply_path, invoice_request, path).expect("invoice response");
            },
            Event::OnionMessageIntercepted { next_hop, message, .. } => {
                let name = match next_hop { NextMessageHop::NodeId(node) => node.to_string(), _ => panic!("unexpected scid interception") };
                let mut pending: Vec<String> = self.store.read("offline", "", &name).ok().map(|b| serde_json::from_slice(&b).unwrap()).unwrap_or_default();
                let raw = hex(&message.encode()); if !pending.contains(&raw) { pending.push(raw); }
                assert!(pending.len() <= 1024);
                self.store.write("offline", "", &name, serde_json::to_vec(&pending).unwrap()).expect("offline persistence");
                self.events.lock().unwrap().push(json!({"event":"offline_message", "peer":name,"count":pending.len()}));
            },
            Event::OnionMessagePeerConnected { peer_node_id } => {
                let pending: Vec<String> = self.store.read("offline", "", &peer_node_id.to_string()).ok().map(|b| serde_json::from_slice(&b).unwrap()).unwrap_or_default();
                for raw in pending {
                    let raw = bytes(&json!(raw)).unwrap();
                    let message = lightning::ln::msgs::OnionMessage::read_from_fixed_length_buffer(&mut FixedLengthReader::new(&mut raw.as_slice(), raw.len() as u64)).unwrap();
                    if self.messenger.forward_onion_message(message, &peer_node_id).is_err() { break; }
                }
            },
            _ => self.events.lock().unwrap().push(json!({"event":"ldk", "detail":description})),
        }
    }
    async fn command(&self, input: Value) -> Result<Value, String> {
        match input["command"].as_str() {
            Some("offline_queue") => {
                let peer = PublicKey::from_str(input["peer"].as_str().ok_or("peer")?).map_err(|e| e.to_string())?;
                let pending: Vec<String> = match self.store.read("offline", "", &peer.to_string()) {
                    Ok(raw) => serde_json::from_slice(&raw).map_err(|e| e.to_string())?,
                    Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => vec![],
                    Err(e) => return Err(e.to_string()),
                };
                Ok(json!({"messages":pending}))
            },
            Some("status") => Ok(json!({"node":self.manager.get_our_node_id().to_string(), "height":self.manager.current_best_block().height,
                "peers":self.peers.list_peers().iter().map(|p| p.counterparty_node_id.to_string()).collect::<Vec<_>>(),
                "channels": self.manager.list_channels().iter().map(|c| json!({"id":hex(&c.channel_id.0),"peer":c.counterparty.node_id.to_string(),"ready":c.is_channel_ready,"usable":c.is_usable,"public":c.is_announced,"scid":c.short_channel_id,"inbound_alias":c.inbound_scid_alias,"outbound_alias":c.outbound_scid_alias,"outbound_msat":c.outbound_capacity_msat,"inbound_msat":c.inbound_capacity_msat,
                    "pending_inbound": c.pending_inbound_htlcs.iter().map(|h| json!({"hash":hex(&h.payment_hash.0),"amount":h.amount_msat,"expiry":h.cltv_expiry,"state":format!("{:?}", h.state)})).collect::<Vec<_>>(),
                    "pending_outbound": c.pending_outbound_htlcs.iter().map(|h| json!({"hash":hex(&h.payment_hash.0),"amount":h.amount_msat,"expiry":h.cltv_expiry,"state":format!("{:?}", h.state)})).collect::<Vec<_>>()
                })).collect::<Vec<_>>() })),
            Some("connect") => {
                let peer = PublicKey::from_str(input["peer"].as_str().ok_or("peer")?).map_err(|e| e.to_string())?;
                let address = input["address"].as_str().ok_or("address")?.parse().map_err(|_| "address")?;
                let closed = lightning_net_tokio::connect_outbound(self.peers.clone(), peer, address).await.ok_or("connection failed")?;
                tokio::spawn(closed);
                Ok(json!({"connected":true}))
            },
            Some("open") => {
                let peer = PublicKey::from_str(input["peer"].as_str().ok_or("peer")?).map_err(|e| e.to_string())?;
                let mut config = UserConfig::default();
                config.channel_handshake_config.announce_for_forwarding = input["public"].as_bool().unwrap_or(false);
                config.channel_handshake_config.negotiate_anchors_zero_fee_htlc_tx = false;
                config.channel_handshake_limits.force_announced_channel_preference = false;
                config.channel_config.forwarding_fee_base_msat = 1000;
                config.channel_config.forwarding_fee_proportional_millionths = 0;
                config.channel_config.cltv_expiry_delta = 48;
                let id = self.manager.create_channel(peer, input["amount"].as_u64().unwrap_or(1_000_000), 0, 1, None, Some(config)).map_err(|e| format!("{e:?}"))?;
                Ok(json!({"temporary_id":hex(&id.0)}))
            },
            Some("fund") => {
                let id = ChannelId::from_bytes(bytes(&input["id"])?.try_into().map_err(|_| "channel id")?);
                let peer = PublicKey::from_str(input["peer"].as_str().ok_or("peer")?).map_err(|e| e.to_string())?;
                let tx = deserialize(&bytes(&input["transaction"])?).map_err(|e| e.to_string())?;
                self.manager.funding_transaction_generated(id, peer, tx).map_err(|e| format!("{e:?}"))?;
                Ok(json!({"funding":true}))
            },
            Some("block") => {
                let block: Block = deserialize(&bytes(&input["hex"])?).map_err(|e| e.to_string())?;
                let height = input["height"].as_u64().ok_or("height")? as u32;
                for (index, tx) in block.txdata.iter().enumerate() {
                    for (vout, output) in tx.output.iter().enumerate() {
                        let scid = ((height as u64) << 40) | ((index as u64) << 16) | vout as u64;
                        self.store.write("core_outputs", "", &scid.to_string(), serialize(output)).expect("Core output persistence");
                    }
                }
                self.monitor.block_connected(&block, height); self.manager.block_connected(&block, height);
                Ok(json!({"height":height}))
            },
            Some("announce") => {
                self.peers.broadcast_node_announcement([1,2,3], [0;32], vec![]);
                Ok(json!({"announced":true}))
            },
            Some("server_paths") => {
                let paths = self.manager.blinded_paths_for_async_recipient(bytes(&input["recipient"])? , None).map_err(|_| "paths")?;
                Ok(json!({"paths":paths.iter().map(|p| hex(&p.encode())).collect::<Vec<_>>()}))
            },
            Some("receive_offer") => {
                let paths = input["paths"].as_array().ok_or("paths")?.iter().map(|v| BlindedMessagePath::read(&mut bytes(v)?.as_slice()).map_err(|e| format!("{e:?}"))).collect::<Result<Vec<_>,String>>()?;
                self.manager.set_paths_to_static_invoice_server(paths).map_err(|_| "set paths")?;
                Ok(json!({"registered":true}))
            },
            Some("offer") => Ok(json!({"offer":self.manager.get_async_receive_offer().ok().map(|o| o.to_string())})),
            Some("pay_offer") => {
                let offer: Offer = input["offer"].as_str().ok_or("offer")?.parse().map_err(|e| format!("{e:?}"))?;
                let id = PaymentId(bytes(&input["id"])?.try_into().map_err(|_| "payment id")?);
                self.manager.pay_for_offer(&offer, input["amount"].as_u64(), id, OptionalOfferPaymentParams::default()).map_err(|e| format!("{e:?}"))?;
                Ok(json!({"paying":true}))
            },
            Some("events") => Ok(json!({"events":std::mem::take(&mut *self.events.lock().unwrap()), "broadcasts":std::mem::take(&mut *self.broadcast.0.lock().unwrap()).iter().map(|tx| hex(&serialize(tx))).collect::<Vec<_>>()})),
            _ => Err("unknown node command".into()),
        }
    }
}
pub async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 { return Err("node DIRECTORY SEED client|provider".into()); }
    let node = Arc::new(Node::new(PathBuf::from(&args[2]), args[3].parse().map_err(|_| "seed")?, args[4] == "client")?);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
    println!("{}", json!({"ok":true,"node":node.manager.get_our_node_id().to_string(),"port":listener.local_addr().unwrap().port()}));
    let listen_node = node.clone();
    tokio::spawn(async move { loop { let (stream, _) = listener.accept().await.unwrap(); tokio::spawn(lightning_net_tokio::setup_inbound(listen_node.peers.clone(), stream.into_std().unwrap())); } });
    let event_node = node.clone();
    tokio::spawn(async move { loop {
        let n = event_node.clone(); n.manager.process_pending_events(&|e| { n.handle(e); Ok(()) });
        n.messenger.process_pending_events(&|e| { n.handle(e); Ok(()) });
        n.monitor.process_pending_events(&|e| { n.handle(e); Ok(()) });
        n.manager.process_pending_htlc_forwards();
        n.persist(); n.peers.process_events();
        tokio::time::sleep(Duration::from_millis(25)).await;
    } });
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        let input = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        let result = node.command(input).await; node.persist(); node.peers.process_events();
        let response = match result { Ok(v) => json!({"ok":true,"result":v}), Err(e) => json!({"ok":false,"error":e}) };
        out.write_all(format!("{response}\n").as_bytes()).await.map_err(|e| e.to_string())?; out.flush().await.map_err(|e| e.to_string())?;
    }
    Ok(())
}
