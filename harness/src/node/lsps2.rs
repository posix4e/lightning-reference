//! LSPS2 (bLIP-52) just-in-time channel provider, using only stock LDK's
//! `lightning-liquidity` service. The independent driver still supplies every
//! funding transaction and block; this module answers the client's requests
//! and hands LDK's events to the service handler.
use lightning::util::config::{HTLCInterceptionFlags, UserConfig};
use lightning_liquidity::lsps0::ser::LSPSDateTime;
use lightning_liquidity::lsps2::msgs::LSPS2RawOpeningFeeParams;
use lightning_liquidity::lsps2::service::LSPS2ServiceConfig;
use lightning_liquidity::LiquidityServiceConfig;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Fixed regtest terms: 1,000 sats minimum or 1%, one hour of validity.
pub(super) const MIN_FEE_MSAT: u64 = 1_000_000;
pub(super) const PROPORTIONAL_PPM: u32 = 10_000;
pub(super) const CLTV_EXPIRY_DELTA: u32 = 144;

/// Variants the driver selects per run, so one binary covers the matrix.
#[derive(Clone, Copy, Debug)]
pub(super) struct Options {
    pub client_trusts_lsp: bool,
    pub scid_privacy: bool,
    pub zero_reserve: bool,
    pub anchors: bool,
    pub overprovision_ppm: u64,
}
impl Options {
    pub(super) fn from_env() -> Self {
        let flag = |name: &str| std::env::var(name).map(|v| v == "1").unwrap_or(false);
        Self {
            client_trusts_lsp: flag("REFERENCE_LSPS2_CLIENT_TRUSTS_LSP"),
            scid_privacy: flag("REFERENCE_LSPS2_SCID_PRIVACY"),
            zero_reserve: flag("REFERENCE_LSPS2_ZERO_RESERVE"),
            // LDK allows a zero client reserve only on anchor channels.
            anchors: flag("REFERENCE_LSPS2_ANCHORS") || flag("REFERENCE_LSPS2_ZERO_RESERVE"),
            overprovision_ppm: std::env::var("REFERENCE_LSPS2_OVERPROVISION_PPM").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000),
        }
    }
    /// The channel LDK opens for `amt_to_forward_msat`, over-provisioned so
    /// the provider keeps its reserve and commitment fee after forwarding.
    pub(super) fn channel_sats(&self, amt_to_forward_msat: u64) -> u64 {
        let extra = amt_to_forward_msat / 1_000_000 * self.overprovision_ppm;
        (amt_to_forward_msat + extra) / 1000 + 10_000
    }
    pub(super) fn channel_config(&self) -> UserConfig {
        let mut config = UserConfig::default();
        config.channel_handshake_config.announce_for_forwarding = false;
        config.channel_handshake_config.negotiate_anchors_zero_fee_htlc_tx = self.anchors;
        config.channel_handshake_config.negotiate_scid_privacy = self.scid_privacy;
        config.channel_handshake_limits.force_announced_channel_preference = false;
        config
    }
}

/// Node-wide settings: interception, and anchors offered in `init` so a
/// client may accept the anchor channels this provider opens.
pub(super) fn node_config(config: &mut UserConfig, options: &Options) {
    config.htlc_interception_flags = HTLCInterceptionFlags::ToInterceptSCIDs as u8;
    config.accept_forwards_to_priv_channels = true;
    config.channel_handshake_config.negotiate_anchors_zero_fee_htlc_tx = options.anchors;
}

pub(super) fn service_config() -> LiquidityServiceConfig {
    LiquidityServiceConfig {
        lsps1_service_config: None,
        lsps2_service_config: Some(LSPS2ServiceConfig { promise_secret: [7; 32] }),
        lsps5_service_config: None,
        advertise_service: true,
    }
}

pub(super) fn fee_params() -> LSPS2RawOpeningFeeParams {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    LSPS2RawOpeningFeeParams {
        min_fee_msat: MIN_FEE_MSAT,
        proportional: PROPORTIONAL_PPM,
        valid_until: LSPSDateTime::new_from_duration_since_epoch(now + Duration::from_secs(3600)),
        min_lifetime: 1008,
        max_client_to_self_delay: 2016,
        min_payment_size_msat: 1_000_000,
        max_payment_size_msat: 1_000_000_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_covers_the_forward_and_the_provider_reserve() {
        let options = Options { client_trusts_lsp: false, scid_privacy: false, zero_reserve: false, anchors: false, overprovision_ppm: 100_000 };
        assert_eq!(options.channel_sats(49_000_000), 49_000 + 4_900 + 10_000);
        let exact = Options { overprovision_ppm: 0, ..options };
        assert_eq!(exact.channel_sats(49_000_000), 59_000);
    }

    #[test]
    fn anchors_follow_the_option() {
        let options = Options { client_trusts_lsp: false, scid_privacy: true, zero_reserve: false, anchors: true, overprovision_ppm: 0 };
        let config = options.channel_config();
        assert!(config.channel_handshake_config.negotiate_anchors_zero_fee_htlc_tx);
        assert!(config.channel_handshake_config.negotiate_scid_privacy);
        assert!(!config.channel_handshake_config.announce_for_forwarding);
    }

    #[test]
    fn terms_are_valid_for_an_hour() {
        let params = fee_params();
        assert!(!params.valid_until.is_past());
        assert_eq!((params.min_fee_msat, params.proportional), (MIN_FEE_MSAT, PROPORTIONAL_PPM));
    }
}
