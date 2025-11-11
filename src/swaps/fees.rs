use std::collections::HashMap;
use std::fmt;

use crate::error::Error;
use crate::network::Chain;
use crate::swaps::boltz::SwapType;

/// Transaction sizes in virtual bytes for different swap operations
#[derive(Debug, Clone, Copy)]
pub struct TxSizes {
    /// Size of a normal (submarine) swap claim transaction
    pub normal_claim: u64,
    /// Size of a reverse swap lockup transaction
    pub reverse_lockup: u64,
    /// Size of a reverse swap claim transaction
    pub reverse_claim: u64,
}

/// Transaction sizes for Bitcoin
pub const BTC_TX_SIZES: TxSizes = TxSizes {
    normal_claim: 151,
    reverse_lockup: 154,
    reverse_claim: 111,
};

/// Transaction sizes for Liquid
pub const LIQUID_TX_SIZES: TxSizes = TxSizes {
    normal_claim: 1337,
    reverse_lockup: 2503,
    reverse_claim: 1309,
};

/// Get transaction sizes for a given chain
pub fn get_tx_sizes(chain: Chain) -> TxSizes {
    match chain {
        Chain::Bitcoin(_) => BTC_TX_SIZES,
        Chain::Liquid(_) => LIQUID_TX_SIZES,
    }
}

/// Fee estimations for different chains (sat/vByte)
pub type FeeEstimations = HashMap<Chain, f64>;

/// Represents a swap pair for fee calculations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapPair {
    pub from: Chain,
    pub to: Chain,
}

impl SwapPair {
    /// Create a new swap pair
    pub fn new(from: Chain, to: Chain) -> Self {
        Self { from, to }
    }

    /// Create a Lightning to Bitcoin swap pair (reverse swap)
    pub fn ln_to_btc(chain: Chain) -> Self {
        Self {
            from: chain,
            to: chain,
        }
    }

    /// Create a Bitcoin to Lightning swap pair (submarine swap)
    pub fn btc_to_ln(chain: Chain) -> Self {
        Self {
            from: chain,
            to: chain,
        }
    }
}

/// Calculate network fees for a swap
///
/// # Arguments
/// * `swap_type` - The type of swap (Submarine, ReverseSubmarine, Chain)
/// * `pair` - The swap pair defining the chains involved
/// * `estimations` - Fee rate estimations for each chain (sat/vByte)
/// * `include_claim` - Whether to include claim transaction fees
///
/// # Returns
/// The calculated network fee in satoshis
pub fn calc_network_fee(
    swap_type: SwapType,
    pair: SwapPair,
    estimations: &FeeEstimations,
    include_claim: bool,
) -> Result<u64, Error> {
    let result = match swap_type {
        SwapType::Submarine => {
            let fee_rate = estimations.get(&pair.from).ok_or_else(|| {
                Error::Generic(format!("No fee estimation for chain {}", pair.from))
            })?;
            let sizes = get_tx_sizes(pair.from);
            sizes.normal_claim as f64 * fee_rate
        }
        SwapType::ReverseSubmarine => {
            let fee_rate = estimations.get(&pair.to).ok_or_else(|| {
                Error::Generic(format!("No fee estimation for chain {}", pair.to))
            })?;
            let sizes = get_tx_sizes(pair.to);
            let size = if include_claim {
                sizes.reverse_lockup + sizes.reverse_claim
            } else {
                sizes.reverse_lockup
            };
            size as f64 * fee_rate
        }
        SwapType::Chain => {
            // For chain swaps, calculate fees for both sides
            let from_fee = calc_network_fee(SwapType::Submarine, pair, estimations, include_claim)?;
            let to_fee =
                calc_network_fee(SwapType::ReverseSubmarine, pair, estimations, include_claim)?;
            return Ok(from_fee + to_fee);
        }
    };

    // Round up to ensure we don't underestimate
    Ok(result.ceil() as u64)
}

/// Relative fee tolerance percentage (25%)
pub const RELATIVE_FEE_TOLERANCE_PERCENT: f64 = 25.0;

/// Absolute fee tolerance in satoshis
pub const ABSOLUTE_FEE_TOLERANCE_SAT: u64 = 1500;

/// Calculate a percentage of a value
fn calculate_percentage(percentage: f64, value: u64) -> u64 {
    ((percentage / 100.0) * value as f64).ceil() as u64
}

/// Check if the actual fee is within tolerance of the expected fee
///
/// # Arguments
/// * `expected` - The expected fee in satoshis
/// * `actual` - The actual fee in satoshis
///
/// # Returns
/// Ok(()) if the fee is within tolerance, otherwise an error
fn check_tolerance(expected: u64, actual: u64) -> Result<(), Error> {
    let tolerance = std::cmp::max(
        ABSOLUTE_FEE_TOLERANCE_SAT,
        calculate_percentage(RELATIVE_FEE_TOLERANCE_PERCENT, expected),
    );

    if actual > expected + tolerance {
        return Err(Error::Protocol(format!(
            "Onchain fee way above expectation: {} > {} + {}",
            actual, expected, tolerance
        )));
    }

    Ok(())
}

/// Get the required chains for fee estimation based on swap type and pair
///
/// # Arguments
/// * `swap_type` - The type of swap
/// * `pair` - The swap pair
///
/// # Returns
/// A vector of chains that need fee estimations
pub fn required_estimations(swap_type: SwapType, pair: SwapPair) -> Vec<Chain> {
    match swap_type {
        SwapType::Submarine => vec![pair.from],
        SwapType::ReverseSubmarine => vec![pair.to],
        SwapType::Chain => {
            if pair.from == pair.to {
                vec![pair.from]
            } else {
                vec![pair.from, pair.to]
            }
        }
    }
}

/// Validate swap amounts against expected fees
///
/// This function checks that the difference between send and receive amounts
/// is within acceptable tolerance of the expected fees.
///
/// # Arguments
/// * `swap_type` - The type of swap
/// * `pair` - The swap pair
/// * `send_amount` - The amount being sent (in satoshis)
/// * `receive_amount` - The amount being received (in satoshis)
/// * `service_fee_percent` - The service fee percentage
/// * `estimations` - Fee rate estimations for each chain
/// * `include_claim` - Whether to include claim transaction fees
///
/// # Returns
/// Ok(()) if amounts are valid, otherwise an error
pub fn check_amounts(
    swap_type: SwapType,
    pair: SwapPair,
    send_amount: u64,
    receive_amount: u64,
    service_fee_percent: f64,
    estimations: &FeeEstimations,
    include_claim: bool,
) -> Result<(), Error> {
    // Validate we have all required fee estimations
    let required_chains = required_estimations(swap_type, pair);
    for chain in &required_chains {
        if !estimations.contains_key(chain) {
            return Err(Error::Generic(format!("No estimation for chain {}", chain)));
        }
    }

    // Calculate total fees
    let total_fees = send_amount
        .checked_sub(receive_amount)
        .ok_or_else(|| Error::Generic("Receive amount exceeds send amount".to_string()))?;

    // Calculate network fees (what's left after subtracting service fees)
    let network_fees = match swap_type {
        SwapType::Submarine => {
            // For submarine swaps, service fee is calculated on receive amount
            let service_fee = calculate_percentage(service_fee_percent, receive_amount);
            total_fees
                .checked_sub(service_fee)
                .ok_or_else(|| Error::Generic("Service fee exceeds total fees".to_string()))?
        }
        SwapType::ReverseSubmarine | SwapType::Chain => {
            // For reverse and chain swaps, service fee is calculated on send amount
            let service_fee = calculate_percentage(service_fee_percent, send_amount);
            total_fees
                .checked_sub(service_fee)
                .ok_or_else(|| Error::Generic("Service fee exceeds total fees".to_string()))?
        }
    };

    // Calculate expected network fees and check tolerance
    let expected_network_fee = calc_network_fee(swap_type, pair, estimations, include_claim)?;
    check_tolerance(expected_network_fee, network_fees)?;

    Ok(())
}

pub fn estimate_claim_fee(chain: Chain, fee_rate: f64) -> u64 {
    let sizes = get_tx_sizes(chain);
    (sizes.reverse_claim as f64 * fee_rate).ceil() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::BitcoinChain;

    #[test]
    fn test_get_tx_sizes() {
        let btc_sizes = get_tx_sizes(Chain::Bitcoin(BitcoinChain::Bitcoin));
        assert_eq!(btc_sizes.normal_claim, 151);
        assert_eq!(btc_sizes.reverse_lockup, 154);
        assert_eq!(btc_sizes.reverse_claim, 111);

        let liquid_sizes = get_tx_sizes(Chain::Liquid(crate::network::LiquidChain::Liquid));
        assert_eq!(liquid_sizes.normal_claim, 1337);
        assert_eq!(liquid_sizes.reverse_lockup, 2503);
        assert_eq!(liquid_sizes.reverse_claim, 1309);
    }

    #[test]
    fn test_calculate_percentage() {
        assert_eq!(calculate_percentage(10.0, 1000), 100);
        assert_eq!(calculate_percentage(25.0, 100), 25);
        assert_eq!(calculate_percentage(0.5, 10000), 50);
        // Test rounding up
        assert_eq!(calculate_percentage(10.0, 15), 2); // 1.5 rounds up to 2
    }

    #[test]
    fn test_calc_network_fee_submarine() {
        let pair = SwapPair::btc_to_ln(Chain::Bitcoin(BitcoinChain::Bitcoin));
        let mut estimations = FeeEstimations::new();
        estimations.insert(Chain::Bitcoin(BitcoinChain::Bitcoin), 10.0);

        let fee = calc_network_fee(SwapType::Submarine, pair, &estimations, false).unwrap();
        // 151 * 10 = 1510
        assert_eq!(fee, 1510);
    }

    #[test]
    fn test_calc_network_fee_reverse() {
        let pair = SwapPair::ln_to_btc(Chain::Bitcoin(BitcoinChain::Bitcoin));
        let mut estimations = FeeEstimations::new();
        estimations.insert(Chain::Bitcoin(BitcoinChain::Bitcoin), 10.0);

        // Without claim
        let fee = calc_network_fee(SwapType::ReverseSubmarine, pair, &estimations, false).unwrap();
        // 154 * 10 = 1540
        assert_eq!(fee, 1540);

        // With claim
        let fee_with_claim =
            calc_network_fee(SwapType::ReverseSubmarine, pair, &estimations, true).unwrap();
        // (154 + 111) * 10 = 2650
        assert_eq!(fee_with_claim, 2650);
    }

    #[test]
    fn test_check_tolerance_within() {
        // Actual fee equal to expected
        assert!(check_tolerance(1000, 1000).is_ok());

        // Actual fee below expected (always ok)
        assert!(check_tolerance(1000, 500).is_ok());

        // Actual fee within absolute tolerance
        assert!(check_tolerance(1000, 2499).is_ok()); // 1000 + 1500 - 1

        // Actual fee within relative tolerance (25% of 10000 = 2500)
        assert!(check_tolerance(10000, 12500).is_ok());
    }

    #[test]
    fn test_check_tolerance_exceeds() {
        // Exceeds both tolerances
        let result = check_tolerance(1000, 3000);
        assert!(result.is_err());

        // Exceeds relative tolerance
        let result = check_tolerance(10000, 15000);
        assert!(result.is_err());
    }

    #[test]
    fn test_required_estimations() {
        let btc_chain = Chain::Bitcoin(BitcoinChain::Bitcoin);
        let liquid_chain = Chain::Liquid(crate::network::LiquidChain::Liquid);

        // Submarine swap
        let pair = SwapPair::new(btc_chain, btc_chain);
        let required = required_estimations(SwapType::Submarine, pair);
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], btc_chain);

        // Reverse swap
        let required = required_estimations(SwapType::ReverseSubmarine, pair);
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], btc_chain);

        // Chain swap same chain
        let required = required_estimations(SwapType::Chain, pair);
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], btc_chain);

        // Chain swap different chains
        let pair = SwapPair::new(btc_chain, liquid_chain);
        let required = required_estimations(SwapType::Chain, pair);
        assert_eq!(required.len(), 2);
        assert!(required.contains(&btc_chain));
        assert!(required.contains(&liquid_chain));
    }

    #[test]
    fn test_check_amounts_reverse_swap() {
        let pair = SwapPair::ln_to_btc(Chain::Bitcoin(BitcoinChain::Bitcoin));
        let mut estimations = FeeEstimations::new();
        estimations.insert(Chain::Bitcoin(BitcoinChain::Bitcoin), 10.0);

        // Reverse swap: send 100000, service fee 1% = 1000, network fee ~2650 (154+111)*10
        // Total fees = 3650, so receive amount = 100000 - 3650 = 96350
        let send_amount = 100_000;
        let service_fee_percent = 1.0;
        let expected_service_fee = 1000;
        let expected_network_fee = 2650; // (154 + 111) * 10
        let receive_amount = send_amount - expected_service_fee - expected_network_fee;

        let result = check_amounts(
            SwapType::ReverseSubmarine,
            pair,
            send_amount,
            receive_amount,
            service_fee_percent,
            &estimations,
            true,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_amounts_submarine_swap() {
        let pair = SwapPair::btc_to_ln(Chain::Bitcoin(BitcoinChain::Bitcoin));
        let mut estimations = FeeEstimations::new();
        estimations.insert(Chain::Bitcoin(BitcoinChain::Bitcoin), 10.0);

        // Submarine swap: receive 100000, service fee 1% of receive = 1000, network fee ~1510
        // Total fees = 2510, so send amount = 100000 + 2510 = 102510
        let receive_amount = 100_000;
        let service_fee_percent = 1.0;
        let expected_service_fee = 1000;
        let expected_network_fee = 1510; // 151 * 10
        let send_amount = receive_amount + expected_service_fee + expected_network_fee;

        let result = check_amounts(
            SwapType::Submarine,
            pair,
            send_amount,
            receive_amount,
            service_fee_percent,
            &estimations,
            false,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_amounts_fails_excessive_fees() {
        let pair = SwapPair::ln_to_btc(Chain::Bitcoin(BitcoinChain::Bitcoin));
        let mut estimations = FeeEstimations::new();
        estimations.insert(Chain::Bitcoin(BitcoinChain::Bitcoin), 10.0);

        // Set receive amount too low (excessive fees)
        let send_amount = 100_000;
        let receive_amount = 80_000; // Way too low

        let result = check_amounts(
            SwapType::ReverseSubmarine,
            pair,
            send_amount,
            receive_amount,
            1.0,
            &estimations,
            true,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_check_amounts_missing_estimation() {
        let pair = SwapPair::ln_to_btc(Chain::Bitcoin(BitcoinChain::Bitcoin));
        let estimations = FeeEstimations::new(); // Empty estimations

        let result = check_amounts(
            SwapType::ReverseSubmarine,
            pair,
            100_000,
            95_000,
            1.0,
            &estimations,
            true,
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No estimation for chain"));
    }
}
