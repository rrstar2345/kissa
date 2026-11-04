//! Black-76 / Black-Scholes (q = 0) pricing, implied volatility and Greeks.
//!
//! Options are priced off a forward `F = S * e^{rT}` (estimated by put-call
//! parity at the chain level), which avoids needing dividend assumptions.
//! Units: theta is per calendar day, vega is per 1 vol point (1%), IV is
//! returned in percent.

use std::f64::consts::PI;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Greeks {
    pub iv: f64,
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,
    pub vega: f64,
}

fn norm_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * PI).sqrt()
}

/// Standard normal CDF via the Abramowitz & Stegun 7.1.26 erf approximation
/// (abs. error < 1.5e-7, plenty for trading displays).
fn norm_cdf(x: f64) -> f64 {
    let z = x / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * z.abs());
    let poly = t
        * (0.254829592
            + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))));
    let erf = 1.0 - poly * (-z * z).exp();
    0.5 * (1.0 + if z >= 0.0 { erf } else { -erf })
}

/// Option price from forward `f`, strike `k`, vol `sigma` (decimal), time `t`
/// in years and rate `r`.
pub fn black_price(is_call: bool, f: f64, k: f64, sigma: f64, t: f64, r: f64) -> f64 {
    let df = (-r * t).exp();
    let sd = sigma * t.sqrt();
    if sd <= 0.0 {
        let intrinsic = if is_call { f - k } else { k - f };
        return df * intrinsic.max(0.0);
    }
    let d1 = ((f / k).ln() + 0.5 * sd * sd) / sd;
    let d2 = d1 - sd;
    if is_call {
        df * (f * norm_cdf(d1) - k * norm_cdf(d2))
    } else {
        df * (k * norm_cdf(-d2) - f * norm_cdf(-d1))
    }
}

/// Solve implied vol (decimal) by bisection. `None` when the price is below
/// intrinsic value or above the no-arbitrage bound.
pub fn implied_vol(is_call: bool, price: f64, f: f64, k: f64, t: f64, r: f64) -> Option<f64> {
    if !(price > 0.0 && f > 0.0 && k > 0.0 && t > 0.0) {
        return None;
    }
    let df = (-r * t).exp();
    let intrinsic = df * if is_call { (f - k).max(0.0) } else { (k - f).max(0.0) };
    if price < intrinsic - 1e-9 {
        return None;
    }
    let (mut lo, mut hi) = (1e-4_f64, 5.0_f64);
    if black_price(is_call, f, k, hi, t, r) < price {
        return None;
    }
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if black_price(is_call, f, k, mid, t, r) > price {
            hi = mid;
        } else {
            lo = mid;
        }
        if hi - lo < 1e-7 {
            break;
        }
    }
    Some(0.5 * (lo + hi))
}

/// Greeks for an option given its market price. `None` if IV can't be solved.
pub fn greeks(is_call: bool, price: f64, f: f64, k: f64, t: f64, r: f64) -> Option<Greeks> {
    let sigma = implied_vol(is_call, price, f, k, t, r)?;
    let sqrt_t = t.sqrt();
    let sd = sigma * sqrt_t;
    let d1 = ((f / k).ln() + 0.5 * sd * sd) / sd;
    let d2 = d1 - sd;
    let df = (-r * t).exp();
    let s = f * df; // spot-equivalent
    let pdf = norm_pdf(d1);

    let delta = if is_call { norm_cdf(d1) } else { norm_cdf(d1) - 1.0 };
    let gamma = pdf / (s * sd);
    let vega = s * pdf * sqrt_t / 100.0;
    let decay = -s * pdf * sigma / (2.0 * sqrt_t);
    let theta_year = if is_call {
        decay - r * k * df * norm_cdf(d2)
    } else {
        decay + r * k * df * norm_cdf(-d2)
    };

    Some(Greeks { iv: sigma * 100.0, delta, gamma, theta: theta_year / 365.0, vega })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdf_sanity() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-6);
        assert!((norm_cdf(1.96) - 0.975).abs() < 1e-3);
        assert!((norm_cdf(-1.0) + norm_cdf(1.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn iv_round_trip() {
        let (f, k, t, r, sigma) = (24000.0, 24200.0, 14.0 / 365.0, 0.07, 0.16);
        for is_call in [true, false] {
            let p = black_price(is_call, f, k, sigma, t, r);
            let iv = implied_vol(is_call, p, f, k, t, r).unwrap();
            assert!((iv - sigma).abs() < 1e-5, "iv {iv}");
        }
    }

    #[test]
    fn greeks_signs_and_parity() {
        let (f, k, t, r) = (24000.0, 24000.0, 7.0 / 365.0, 0.07);
        let c = black_price(true, f, k, 0.15, t, r);
        let p = black_price(false, f, k, 0.15, t, r);
        let gc = greeks(true, c, f, k, t, r).unwrap();
        let gp = greeks(false, p, f, k, t, r).unwrap();
        assert!(gc.delta > 0.0 && gp.delta < 0.0);
        assert!((gc.delta - gp.delta - 1.0).abs() < 1e-6);
        assert!((gc.gamma - gp.gamma).abs() < 1e-9);
        assert!(gc.theta < 0.0 && gc.vega > 0.0);
        assert!((gc.iv - 15.0).abs() < 0.01);
    }

    #[test]
    fn below_intrinsic_is_none() {
        assert!(implied_vol(true, 1.0, 24000.0, 23000.0, 0.02, 0.07).is_none());
    }
}
