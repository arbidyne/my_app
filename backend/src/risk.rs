//! Pre-trade risk checks.
//!
//! Pure validation that runs before any order reaches IBKR. Each check can be
//! individually disabled by setting the corresponding limit to zero (or zero
//! for `min_pos_size`).

use crate::ContractConfig;

/// Validates an order against risk limits before submission to IBKR.
/// Returns `Ok(())` if the order passes all checks, or `Err(reason)`
/// describing which limit was breached.
pub fn check_risk(
    action: &str,
    quantity: f64,
    config: &ContractConfig,
    current_position: f64,
) -> Result<(), String> {
    // 1. Max order size
    if config.max_order_size > 0 && quantity > config.max_order_size as f64 {
        return Err(format!(
            "Order size {quantity} exceeds max_order_size {}",
            config.max_order_size
        ));
    }

    // 2. Compute resultant position
    let resultant = match action.to_uppercase().as_str() {
        "BUY" => current_position + quantity,
        "SELL" => current_position - quantity,
        other => return Err(format!("Unknown action: {other}")),
    };

    // 3. Max position size
    if config.max_pos_size > 0 && resultant.abs() > config.max_pos_size as f64 {
        return Err(format!(
            "Resultant position {resultant} exceeds max_pos_size {}",
            config.max_pos_size
        ));
    }

    // 4. Min position size — closing to zero is always allowed
    if config.min_pos_size != 0
        && resultant.abs() != 0.0
        && resultant.abs() < (config.min_pos_size as f64).abs()
    {
        return Err(format!(
            "Resultant position {resultant} below min_pos_size {}",
            config.min_pos_size
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(max_order: u32, max_pos: u32, min_pos: i32) -> ContractConfig {
        ContractConfig {
            symbol: "TEST".to_string(),
            autotrade: false,
            max_pos_size: max_pos,
            min_pos_size: min_pos,
            max_order_size: max_order,
            multiplier: 1.0,
            lot_size: 1,
        }
    }

    #[test]
    fn order_size_within_limit() {
        let cfg = config(100, 0, 0);
        assert!(check_risk("BUY", 50.0, &cfg, 0.0).is_ok());
    }

    #[test]
    fn order_size_exceeds_limit() {
        let cfg = config(10, 0, 0);
        let err = check_risk("BUY", 50.0, &cfg, 0.0).unwrap_err();
        assert!(err.contains("max_order_size"), "got: {err}");
    }

    #[test]
    fn resultant_position_within_limit() {
        let cfg = config(0, 100, 0);
        assert!(check_risk("BUY", 50.0, &cfg, 30.0).is_ok());
    }

    #[test]
    fn resultant_position_exceeds_max_long() {
        let cfg = config(0, 100, 0);
        let err = check_risk("BUY", 80.0, &cfg, 50.0).unwrap_err();
        assert!(err.contains("max_pos_size"), "got: {err}");
    }

    #[test]
    fn resultant_position_exceeds_max_short() {
        let cfg = config(0, 100, 0);
        let err = check_risk("SELL", 80.0, &cfg, -50.0).unwrap_err();
        assert!(err.contains("max_pos_size"), "got: {err}");
    }

    #[test]
    fn resultant_position_below_min() {
        let cfg = config(0, 0, 10);
        let err = check_risk("BUY", 5.0, &cfg, 0.0).unwrap_err();
        assert!(err.contains("min_pos_size"), "got: {err}");
    }

    #[test]
    fn closing_to_zero_allowed() {
        let cfg = config(0, 0, 10);
        // Selling full position to reach zero should pass even with min_pos_size set.
        assert!(check_risk("SELL", 50.0, &cfg, 50.0).is_ok());
    }

    #[test]
    fn zero_limits_are_disabled() {
        let cfg = config(0, 0, 0);
        // All limits disabled — any order should pass.
        assert!(check_risk("BUY", 999_999.0, &cfg, 999_999.0).is_ok());
    }
}
