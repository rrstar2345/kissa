//! Alert hooks of the trading service: what to say about fills, rejections,
//! risk halts, the daily P&L summary and token expiry. Delivery, throttling
//! and history are in `crate::alerts`.

use chrono::{Datelike, Duration as ChronoDuration, Timelike, Utc};
use tracing::warn;

use super::TradingService;
use crate::accounts;
use crate::alerts::{token_phase, Alert, Kind, TokenPhase};
use crate::db;
use crate::market::market_clock;
use crate::trading::pricing::r2;
use crate::trading::{now_ts, today_ist, OrderRow, ROLE_SETTLE, ROLE_STOP, ROLE_TARGET};

const IST_OFFSET_SECS: i64 = 5 * 3600 + 30 * 60;
const SUMMARY_DONE_KEY: &str = "alerts.summary_last";

/// `NIFTY 24000 CE 2026-11-24`
fn contract(o: &OrderRow) -> String {
    format!("{} {} {} {}", o.symbol, o.strike, if o.is_call { "CE" } else { "PE" }, o.expiry)
}

/// `+1,234.50` / `-300.00` with Indian-style rupee sign.
fn rupees(v: f64) -> String {
    format!("{}₹{:.2}", if v < 0.0 { "-" } else if v > 0.0 { "+" } else { "" }, v.abs())
}

impl TradingService {
    async fn account_name(&self, account_id: &str) -> String {
        accounts::get(&self.pool, account_id).await.map(|a| a.name).unwrap_or_else(|_| account_id.to_string())
    }

    /// A fill was booked: `qty` units at `price`; `total_filled` of the order so far.
    pub(super) async fn alert_fill(&self, o: &OrderRow, qty: i64, price: f64, total_filled: i64) {
        let what = match o.role.as_str() {
            ROLE_STOP => "Stop-loss hit",
            ROLE_TARGET => "Target hit",
            ROLE_SETTLE => "Expiry settlement",
            _ => "Filled",
        };
        let name = self.account_name(&o.account_id).await;
        let lots = if o.lot_size > 0 { format!(" ({} lot)", qty / o.lot_size) } else { String::new() };
        let progress = if total_filled < o.qty {
            format!("\nOrder filled {total_filled} of {}", o.qty)
        } else {
            String::new()
        };
        self.alerts.notify(Alert::new(
            Kind::Fill,
            format!("{}:{}", o.id, total_filled),
            format!("{what}: {} {}", o.side, contract(o)),
            format!("{name} · {qty}{lots} @ {price:.2} · {}{progress}", o.product),
        ));
    }

    pub(super) async fn alert_rejected(&self, o: &OrderRow) {
        let name = self.account_name(&o.account_id).await;
        let reason = if o.message.trim().is_empty() { "no reason given" } else { o.message.trim() };
        self.alerts.notify(Alert::new(
            Kind::Rejection,
            o.id.clone(),
            format!("Order rejected: {} {}", o.side, contract(o)),
            format!("{name} · {} {} @ {:.2}\n{reason}", o.qty, o.order_type, o.price),
        ));
    }

    /// Risk events are rare and always worth hearing about: the key is unique
    /// so the de-dup window never hides one.
    pub(super) fn alert_risk(&self, title: String, body: String) {
        self.alerts.notify(Alert::new(Kind::Risk, now_ts(), title, body));
    }

    // ------------------------------------------------------------ scheduled ---

    /// Sweeper hook (about once a minute): the daily summary and token reminders.
    pub(super) async fn alerts_tick(&self) {
        if !self.alerts.has_channel() {
            return;
        }
        if let Err(e) = self.daily_summary_if_due().await {
            warn!("Daily summary alert failed: {e}");
        }
        if let Err(e) = self.token_reminders().await {
            warn!("Token expiry check failed: {e}");
        }
    }

    /// Once per weekday, after the configured time, when anything happened today.
    async fn daily_summary_if_due(&self) -> anyhow::Result<()> {
        let cfg = self.alerts.config();
        if !cfg.event_on(Kind::DailySummary) {
            return Ok(());
        }
        let ist = market_clock::get_ist_now() + ChronoDuration::seconds(IST_OFFSET_SECS);
        if ist.weekday().number_from_monday() > 5 || ist.hour() * 60 + ist.minute() < cfg.summary_minutes() {
            return Ok(());
        }
        let today = today_ist();
        if db::get_setting(&self.pool, SUMMARY_DONE_KEY).await?.as_deref() == Some(today.as_str()) {
            return Ok(());
        }
        let dash = self.dashboard(None).await?;
        db::set_setting(&self.pool, SUMMARY_DONE_KEY, &today).await?;
        let active: Vec<_> =
            dash.accounts.iter().filter(|a| a.orders_today > 0 || a.trades_today > 0 || a.open_positions > 0).collect();
        if active.is_empty() {
            return Ok(());
        }
        let total: f64 = active.iter().map(|a| a.net_today).sum();
        let mut lines = Vec::new();
        for a in &active {
            lines.push(format!(
                "{} ({}): net {} · realised {} · unrealised {} · charges ₹{:.2} · {} trades · {} open",
                a.name,
                a.kind,
                rupees(a.net_today),
                rupees(a.realised_today),
                rupees(a.unrealised),
                a.charges_today,
                a.trades_today,
                a.open_positions
            ));
        }
        self.alerts.notify(Alert::new(
            Kind::DailySummary,
            today.clone(),
            format!("Daily P&L {today}: {}", rupees(r2(total))),
            lines.join("\n"),
        ));
        Ok(())
    }

    /// Remind about Fyers / Dhan tokens that are about to expire or probably
    /// have. The expiry is estimated: saved time + the configured lifetime.
    /// Each stage (soon, expired) is sent once per saved token.
    async fn token_reminders(&self) -> anyhow::Result<()> {
        let cfg = self.alerts.config();
        if !cfg.event_on(Kind::TokenExpiry) {
            return Ok(());
        }
        let warn_lead = ChronoDuration::hours(cfg.token_warn_hours as i64);
        let now = Utc::now();
        for a in accounts::list(&self.pool).await? {
            let (broker, hours) = match a.kind.as_str() {
                "fyers" => ("Fyers", cfg.fyers_token_hours),
                "dhan" => ("Dhan", cfg.dhan_token_hours),
                _ => continue,
            };
            if hours == 0 || !a.secrets_set.iter().any(|s| s == "accessToken") {
                continue;
            }
            let Some(saved) = accounts::token_saved_at(&self.pool, &a.id).await else { continue };
            let expires = saved + ChronoDuration::hours(hours as i64);
            let phase = token_phase(now, expires, warn_lead);
            if phase == TokenPhase::Fresh {
                continue;
            }
            let marker = format!("{}|{}", saved.to_rfc3339(), phase.as_str());
            let key = format!("{}{}", accounts::TOKEN_NOTIFIED_PREFIX, a.id);
            if db::get_setting(&self.pool, &key).await?.as_deref() == Some(marker.as_str()) {
                continue;
            }
            db::set_setting(&self.pool, &key, &marker).await?;
            let (title, body) = if phase == TokenPhase::Soon {
                let mins = (expires - now).num_minutes().max(1);
                (
                    format!("{broker} token expires soon: {}", a.name),
                    format!(
                        "Expected in about {}h {}m ({hours} h after it was saved). Paste a new token on the Accounts page.",
                        mins / 60,
                        mins % 60
                    ),
                )
            } else {
                (
                    format!("{broker} token has probably expired: {}", a.name),
                    format!("It was saved {hours}+ hours ago. Paste a new token on the Accounts page."),
                )
            };
            self.alerts.notify(Alert::new(Kind::TokenExpiry, marker, title, body));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rupee_formatting() {
        assert_eq!(rupees(1234.5), "+₹1234.50");
        assert_eq!(rupees(-300.0), "-₹300.00");
        assert_eq!(rupees(0.0), "₹0.00");
    }
}
