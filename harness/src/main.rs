use bitcoin::{Network, hashes::{Hash, sha256}, secp256k1::Secp256k1};
use lightning::blinded_path::message::{BlindedMessagePath, MessageContext, OffersContext};
use lightning::blinded_path::EmptyNodeIdLookUp;
use lightning::blinded_path::payment::{AsyncBolt12OfferContext, BlindedPaymentPath, PaymentConstraints, PaymentContext, ReceiveTlvs};
use lightning::ln::{msgs::OnionMessage, peer_handler::IgnoringMessageHandler};
use lightning::onion_message::{messenger::{create_onion_message, peel_onion_message, Destination, OnionMessagePath, PeeledOnion}, offers::OffersMessage};
use lightning::onion_message::async_payments::{AsyncPaymentsMessage, OfferPathsRequest, OfferPaths, ServeStaticInvoice, StaticInvoicePersisted, HeldHtlcAvailable, ReleaseHeldHtlc};
use lightning::types::payment::PaymentSecret;
use lightning::offers::{invoice_request::InvoiceRequest, nonce::Nonce, offer::{Offer, OfferBuilder}, static_invoice::{StaticInvoice, StaticInvoiceBuilder}};
use lightning::sign::{EntropySource, KeysManager, NodeSigner, Recipient};
use lightning::util::{logger::{Logger, Record}, ser::{Readable, Writeable, LengthReadable, FixedLengthReader}};
use serde_json::{json, Value};
use std::io::{self, BufRead};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

struct FixtureEntropy(AtomicU64);
struct FixtureLogger;
impl Logger for FixtureLogger {
    fn log(&self, record: Record) {
        if std::env::var_os("WINNOW_REFERENCE_TRACE").is_some() {
            eprintln!("{}", record.args);
        } else if matches!(record.level, lightning::util::logger::Level::Warn | lightning::util::logger::Level::Error) {
            // CI artifacts contain locations, never raw onion payloads,
            // channel secrets or preimages from the verbose reference logger.
            eprintln!("{:?} {}:{}", record.level, record.module_path, record.line);
        }
    }
}
fn keys() -> KeysManager<FixtureLogger> { KeysManager::new(&[42;32], 1, 0, false, FixtureLogger) }
impl EntropySource for FixtureEntropy {
    fn get_secure_random_bytes(&self) -> [u8; 32] {
        sha256::Hash::hash(&self.0.fetch_add(1, Ordering::Relaxed).to_be_bytes()).to_byte_array()
    }
}
fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }
fn bytes(value: &Value) -> Result<Vec<u8>, String> {
    let text = value.as_str().ok_or("hex string required")?;
    if text.len() % 2 != 0 || !text.is_ascii() { return Err("invalid hex".into()); }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i+2],16).map_err(|_| "invalid hex".into())).collect()
}
fn generated_vectors() -> Result<Value, String> {
    let secp = Secp256k1::new();
    let keys = keys();
    let node = keys.get_node_id(Recipient::Node).unwrap();
    let entropy = FixtureEntropy(AtomicU64::new(1));
    let receive_key = keys.get_receive_auth_key();
    let expanded = keys.get_expanded_key();
    let nonce = Nonce::from_entropy_source(&entropy);
    let path = BlindedMessagePath::one_hop(node, receive_key,
        MessageContext::Offers(OffersContext::InvoiceRequest { nonce, payment_metadata: None }), true, &entropy, &secp);
    let offer = OfferBuilder::deriving_signing_pubkey(node, &expanded, nonce, &secp)
        .chain(Network::Regtest).path(path.clone()).build().map_err(|e| format!("{e:?}"))?;
    let payment = BlindedPaymentPath::one_hop(node, receive_key, ReceiveTlvs {
        payment_secret: PaymentSecret([45;32]),
        payment_constraints: PaymentConstraints { max_cltv_expiry: 1000, htlc_minimum_msat: 1 },
        payment_context: PaymentContext::AsyncBolt12Offer(AsyncBolt12OfferContext { offer_nonce: nonce, payment_metadata: None }),
    }, 18, &entropy, &secp).map_err(|_| "payment path")?;
    let invoice = StaticInvoiceBuilder::for_offer_using_derived_keys(&offer, vec![payment], vec![path.clone()],
        Duration::from_secs(1_800_000_000), &expanded, nonce, &secp).map_err(|e| format!("{e:?}"))?
        .relative_expiry(3600).build_and_sign(&secp).map_err(|e| format!("{e:?}"))?;
    Ok(json!({"offer": offer.to_string(), "offer_hex": hex(&offer.encode()), "static_invoice": hex(&invoice.encode()),
        "message_path": hex(&path.encode()), "signing_key": invoice.signing_pubkey().to_string(),
        "node_key": node.to_string(), "node_secret": hex(&keys.get_node_secret_key().secret_bytes()), "receive_key": hex(&receive_key.0),
        "offer_paths_request": hex(&OfferPathsRequest { invoice_slot: 7 }.encode()),
        "offer_paths": hex(&OfferPaths { paths: vec![path.clone()], paths_absolute_expiry: Some(1_800_003_600) }.encode()),
        "serve_static_invoice": hex(&ServeStaticInvoice { invoice: invoice.clone(), forward_invoice_request_path: path }.encode()),
        "persisted": hex(&StaticInvoicePersisted {}.encode()), "held": hex(&HeldHtlcAvailable {}.encode()), "release": hex(&ReleaseHeldHtlc {}.encode())}))
}
fn execute(input: Value) -> Result<Value, String> {
    match input["command"].as_str() {
        Some("vectors") => generated_vectors(),
        Some("held_message") => {
            // Read-only evidence decoder. This does not instantiate a payment
            // manager, handle the message, or wake the stopped recipient.
            let seed = u8::try_from(input["seed"].as_u64().ok_or("seed")?).map_err(|_| "seed")?;
            let keys = KeysManager::new(&[seed;32], 1, 0, false, FixtureLogger);
            let raw = bytes(&input["hex"])?;
            let message = OnionMessage::read_from_fixed_length_buffer(&mut FixedLengthReader::new(&mut raw.as_slice(), raw.len() as u64)).map_err(|e| format!("{e:?}"))?;
            let held = matches!(peel_onion_message(&message, &Secp256k1::new(), &keys, &FixtureLogger, &IgnoringMessageHandler {}),
                Ok(PeeledOnion::AsyncPayments(AsyncPaymentsMessage::HeldHtlcAvailable(_), _, Some(_))));
            Ok(json!({"held":held}))
        },
        Some("offer") => {
            let offer: Offer = input["string"].as_str().ok_or("offer string")?.parse().map_err(|e| format!("{e:?}"))?;
            Ok(json!({"roundtrip": offer.to_string(), "hex": hex(&offer.encode())}))
        },
        Some("invoice_request") => {
            let request = InvoiceRequest::try_from(bytes(&input["hex"])?).map_err(|e| format!("{e:?}"))?;
            Ok(json!({"hex": hex(&request.encode()), "payer_key": request.payer_signing_pubkey().to_string(), "amount_msat": request.amount_msats()}))
        },
        Some("static_invoice") => {
            let invoice = StaticInvoice::try_from(bytes(&input["hex"])?).map_err(|e| format!("{e:?}"))?;
            Ok(json!({"hex": hex(&invoice.encode()), "signing_key": invoice.signing_pubkey().to_string(),
                "payment_path_count": invoice.payment_paths().len(), "notification_path_count": invoice.held_htlc_available_paths().len()}))
        },
        Some("peel_request") => {
            let raw = bytes(&input["hex"])?;
            let mut cursor = raw.as_slice();
            let message = OnionMessage::read_from_fixed_length_buffer(&mut FixedLengthReader::new(&mut cursor, raw.len() as u64)).map_err(|e| format!("{e:?}"))?;
            match peel_onion_message(&message, &Secp256k1::new(), &keys(), &FixtureLogger, &IgnoringMessageHandler {}) {
                Ok(PeeledOnion::Offers(OffersMessage::InvoiceRequest(request), context, reply)) =>
                    Ok(json!({"hex": hex(&request.encode()), "context": context.is_some(), "reply": reply.is_some()})),
                _ => Err("LDK did not authenticate an invoice request".into()),
            }
        },
        Some("reply_invoice") => {
            let raw_path = bytes(&input["path"])?;
            let path = BlindedMessagePath::read(&mut raw_path.as_slice()).map_err(|e| format!("{e:?}"))?;
            let invoice = StaticInvoice::try_from(bytes(&input["invoice"])?).map_err(|e| format!("{e:?}"))?;
            let entropy = FixtureEntropy(AtomicU64::new(200));
            let route = OnionMessagePath { intermediate_nodes: vec![], destination: Destination::BlindedPath(path), first_node_addresses: vec![] };
            let (_, message, _) = create_onion_message(&entropy, &keys(), &EmptyNodeIdLookUp {}, &Secp256k1::new(), route,
                OffersMessage::StaticInvoice(invoice), None).map_err(|e| format!("{e:?}"))?;
            Ok(json!({"hex": hex(&message.encode())}))
        },
        _ => Err("unknown command".into()),
    }
}
mod node;
fn main() {
    if std::env::args().nth(1).as_deref() == Some("node") {
        tokio::runtime::Runtime::new().unwrap().block_on(node::run()).unwrap();
        return;
    }
    for line in io::stdin().lock().lines() {
        let response = line.map_err(|e| e.to_string()).and_then(|line| serde_json::from_str(&line).map_err(|e| e.to_string())).and_then(execute);
        println!("{}", match response { Ok(value) => json!({"ok": true, "result": value}), Err(error) => json!({"ok":false,"error":error}) });
    }
}
