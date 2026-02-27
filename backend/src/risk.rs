//! Pre-trade risk checks.
//!
//! Pure validation that runs before any order reaches IBKR. Zero values for
//! `max_order_size` and `max_pos_size` mean "zero tolerance" (reject everything),
//! not "disabled". Set large values to effectively disable a limit.
//! `min_pos_size=0` means "no minimum" (disabled).

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
    // 1. Autotrade gate
    if !config.autotrade {
        return Err("Autotrade is disabled for this contract".to_string());
    }

    // 2. Max order size
    if quantity > config.max_order_size as f64 {
        return Err(format!(
            "Order size {quantity} exceeds max_order_size {}",
            config.max_order_size
        ));
    }

    // 3. Compute resultant position
    let resultant = match action.to_uppercase().as_str() {
        "BUY" => current_position + quantity,
        "SELL" => current_position - quantity,
        other => return Err(format!("Unknown action: {other}")),
    };

    // 4. Max position size
    if resultant.abs() > config.max_pos_size as f64 {
        return Err(format!(
            "Resultant position {resultant} exceeds max_pos_size {}",
            config.max_pos_size
        ));
    }

    // 5. Min position size — closing to zero is always allowed
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
            autotrade: true,
            max_pos_size: max_pos,
            min_pos_size: min_pos,
            max_order_size: max_order,
            multiplier: 1.0,
            lot_size: 1,
        }
    }

    #[test]
    fn autotrade_disabled_rejects() {
        let mut cfg = config(100, 100, 0);
        cfg.autotrade = false;
        let err = check_risk("BUY", 1.0, &cfg, 0.0).unwrap_err();
        assert!(err.contains("Autotrade is disabled"), "got: {err}");
    }

    #[test]
    fn autotrade_enabled_passes() {
        let cfg = config(100, 100, 0);
        assert!(check_risk("BUY", 50.0, &cfg, 0.0).is_ok());
    }

    #[test]
    fn order_size_within_limit() {
        let cfg = config(100, 1_000_000, 0);
        assert!(check_risk("BUY", 50.0, &cfg, 0.0).is_ok());
    }

    #[test]
    fn order_size_exceeds_limit() {
        let cfg = config(10, 1_000_000, 0);
        let err = check_risk("BUY", 50.0, &cfg, 0.0).unwrap_err();
        assert!(err.contains("max_order_size"), "got: {err}");
    }

    #[test]
    fn max_order_size_zero_rejects() {
        let cfg = config(0, 1_000_000, 0);
        let err = check_risk("BUY", 1.0, &cfg, 0.0).unwrap_err();
        assert!(err.contains("max_order_size"), "got: {err}");
    }

    #[test]
    fn resultant_position_within_limit() {
        let cfg = config(1_000_000, 100, 0);
        assert!(check_risk("BUY", 50.0, &cfg, 30.0).is_ok());
    }

    #[test]
    fn resultant_position_exceeds_max_long() {
        let cfg = config(1_000_000, 100, 0);
        let err = check_risk("BUY", 80.0, &cfg, 50.0).unwrap_err();
        assert!(err.contains("max_pos_size"), "got: {err}");
    }

    #[test]
    fn resultant_position_exceeds_max_short() {
        let cfg = config(1_000_000, 100, 0);
        let err = check_risk("SELL", 80.0, &cfg, -50.0).unwrap_err();
        assert!(err.contains("max_pos_size"), "got: {err}");
    }

    #[test]
    fn max_pos_size_zero_rejects() {
        let cfg = config(1_000_000, 0, 0);
        let err = check_risk("BUY", 1.0, &cfg, 0.0).unwrap_err();
        assert!(err.contains("max_pos_size"), "got: {err}");
    }

    #[test]
    fn resultant_position_below_min() {
        let cfg = config(1_000_000, 1_000_000, 10);
        let err = check_risk("BUY", 5.0, &cfg, 0.0).unwrap_err();
        assert!(err.contains("min_pos_size"), "got: {err}");
    }

    #[test]
    fn closing_to_zero_allowed() {
        let cfg = config(1_000_000, 1_000_000, 10);
        // Selling full position to reach zero should pass even with min_pos_size set.
        assert!(check_risk("SELL", 50.0, &cfg, 50.0).is_ok());
    }
}
