//! What the Algos page needs: validation against the database, saving,
//! deleting, arming and the list with run state. The evaluation loop is in
//! `runner`.

use anyhow::{anyhow, Result};
use sqlx::SqlitePool;

use super::{store, Algo, AlgoState, AlgoView};
use crate::accounts;
use crate::charts::rules;
use crate::state::SharedState;

/// Structure checks plus: the signal exists and is valid, every account
/// exists and is a paper or Dhan account.
pub async fn validate_full(pool: &SqlitePool, algo: &Algo) -> Result<()> {
    let errs = algo.validate();
    if !errs.is_empty() {
        return Err(anyhow!(errs.join("; ")));
    }
    let signals = rules::store::list(pool).await?;
    let signal = signals
        .iter()
        .find(|r| r.id == algo.signal_id)
        .ok_or_else(|| anyhow!("The selected signal no longer exists"))?;
    let signal_errs = signal.validate();
    if !signal_errs.is_empty() {
        return Err(anyhow!("The signal “{}” is not valid: {}", signal.name, signal_errs.join("; ")));
    }
    for id in &algo.accounts {
        let a = accounts::get(pool, id).await.map_err(|_| anyhow!("An account of this algo no longer exists"))?;
        if a.kind != "paper" && a.kind != "dhan" {
            return Err(anyhow!("{} is not a paper or Dhan account, so an algo cannot trade on it", a.name));
        }
    }
    Ok(())
}

/// Every algo with its run state and signal name.
pub async fn list_views(pool: &SqlitePool) -> Result<Vec<AlgoView>> {
    let algos = store::list_algos(pool).await?;
    let states = store::list_states(pool).await?;
    let signals = rules::store::list(pool).await?;
    Ok(algos
        .into_iter()
        .map(|a| {
            let st = states.get(&a.id).cloned().unwrap_or_else(|| AlgoState::fresh(&a.id));
            AlgoView {
                signal_name: signals.iter().find(|r| r.id == a.signal_id).map(|r| r.name.clone()),
                armed: st.armed,
                phase: st.phase,
                legs: st.legs,
                last_bar_ts: st.last_bar_ts,
                updated_at: st.updated_at,
                algo: a,
            }
        })
        .collect())
}

pub async fn view(pool: &SqlitePool, id: &str) -> Result<AlgoView> {
    list_views(pool)
        .await?
        .into_iter()
        .find(|v| v.algo.id == id)
        .ok_or_else(|| anyhow!("Algo not found"))
}

/// Validate and save. Changes apply from the next bar: an open position keeps
/// the contracts it was entered on; the signal is read fresh on every bar.
pub async fn save(pool: &SqlitePool, mut algo: Algo) -> Result<Algo> {
    algo.normalize();
    validate_full(pool, &algo).await?;
    store::save_algo(pool, algo).await
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<()> {
    let st = store::get_state(pool, id).await?;
    if st.armed {
        return Err(anyhow!("Disarm the algo before deleting it"));
    }
    if st.in_position() {
        return Err(anyhow!("The algo has an open position. Close it first (the algo manages the exit), then delete it"));
    }
    store::delete_algo(pool, id).await
}

/// Arm (after the safety checks) or disarm. Disarming stops new entries; an
/// open position is still managed until it is closed.
pub async fn set_armed(state: &SharedState, id: &str, armed: bool) -> Result<()> {
    let pool = &state.db;
    let algo = store::get_algo(pool, id).await?.ok_or_else(|| anyhow!("Algo not found"))?;
    if armed {
        validate_full(pool, &algo).await?;
        if algo.accounts.is_empty() {
            return Err(anyhow!("Select at least one account before arming"));
        }
        for acct_id in &algo.accounts {
            let a = accounts::get(pool, acct_id).await?;
            if a.kind == "dhan" && !algo.allow_live {
                return Err(anyhow!(
                    "{} is a live Dhan account. Turn on “Allow live accounts” for this algo first",
                    a.name
                ));
            }
            if let Some(reason) = state.trading.halt_reason(acct_id).await {
                return Err(anyhow!("{} is halted: {reason}", a.name));
            }
        }
    }
    store::set_armed(pool, id, armed).await?;
    let (level, kind, msg) = if armed {
        ("info", "armed", "Armed. Signals are acted on from the next bar close.")
    } else {
        ("info", "disarmed", "Disarmed. No new entries; an open position is still managed until it is closed.")
    };
    store::journal_add(pool, &algo.id, &algo.name, level, kind, msg).await;
    Ok(())
}
