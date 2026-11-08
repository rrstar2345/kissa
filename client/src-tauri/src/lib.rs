mod accounts;
mod chain;
mod charts;
mod commands;
mod db;
mod market;
mod secrets;
mod settings;
mod state;
mod stats;
mod trading;

use std::sync::Arc;
use tauri::{Emitter, Manager};
use tokio::sync::{Mutex, RwLock};
use tracing::{error, info};

use market::events::{new_index_store, IndexSink, IndexTick};
use market::{market_clock, streamers};
use state::{AppState, SharedState};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kissa_lib=info".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let paths = settings::setup_app_folders()?;
            let config = settings::load_or_create_config(&paths)?;
            info!("App root: {}", paths.root.display());

            let pool = tauri::async_runtime::block_on(db::init_pool(&paths.db_file))?;
            let (ltp, ltp_rx) = trading::ltp::LtpStore::new();
            let (trading_svc, broker_rx) =
                trading::service::TradingService::new(pool.clone(), config.clone(), ltp.clone(), app.handle().clone());
            let state: SharedState = Arc::new(AppState {
                config: config.clone(),
                paths: paths.clone(),
                db: pool.clone(),
                stats: stats::new_shared_stats(),
                session: market_clock::new_shared_session_state(),
                indices: new_index_store(),
                fno: RwLock::new(Vec::new()),
                chain: Mutex::new(None),
                recorder: charts::recorder::Recorder::new(pool.clone()),
                ltp,
                trading: trading_svc.clone(),
            });
            app.manage(state.clone());

            // Orders: paper matching, Dhan live updates, and the background
            // price feed for open positions / pending paper orders.
            trading_svc.start(ltp_rx, broker_rx);
            trading::feed::spawn(state.clone());

            // Market-clock supervisor: keeps NSE time offset fresh and flips
            // the shared session between Active and Idle.
            let (cfg, session) = (config.clone(), state.session.clone());
            tauri::async_runtime::spawn(async move {
                loop {
                    // Decide the mode first (local clock until NSE time is known) so
                    // the streamers don't wait on a slow NSE time request to start.
                    session.tick(cfg.market_runtime.inactive_switch_after_secs).await;
                    market_clock::refresh_if_stale(&cfg).await;
                    session.tick(cfg.market_runtime.inactive_switch_after_secs).await;
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                }
            });

            // Live prices: push every changed price to the UI in one batched
            // event every 100 ms, so positions, P&L and the order ticket update
            // as the market moves instead of on a polling timer.
            let (quote_app, quote_ltp) = (app.handle().clone(), state.ltp.clone());
            tauri::async_runtime::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    let batch = quote_ltp.drain_changed();
                    if !batch.is_empty() {
                        let _ = quote_app.emit("quotes", batch);
                    }
                }
            });

            // Chart data: live bars are folded in memory and flushed to SQLite.
            state.recorder.clone().spawn(app.handle().clone(), config.charts.flush_secs);
            charts::store::spawn_maintenance(state.db.clone(), config.charts.clone());

            // Index streamer: always on; feeds the header ticker and gives
            // the chain engine its underlying price.
            let handle = app.handle().clone();
            let store = state.indices.clone();
            let recorder = state.recorder.clone();
            let sink: IndexSink = Arc::new(move |tick: IndexTick| {
                recorder.record(
                    &charts::instrument::index_key(&tick.index_name),
                    market_clock::get_ist_now().timestamp(),
                    tick.current_price,
                    None,
                    None,
                );
                if let Ok(mut m) = store.write() {
                    m.insert(tick.index_name.clone(), tick.clone());
                }
                let _ = handle.emit("index-tick", &tick);
            });
            tauri::async_runtime::spawn(streamers::indices::run(
                config,
                state.stats.clone(),
                state.session.clone(),
                sink,
            ));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::market_status,
            commands::indices_snapshot,
            commands::list_fno_symbols,
            commands::list_expiries,
            commands::start_chain,
            commands::stop_chain,
            commands::get_setting,
            commands::set_setting,
            commands::list_accounts,
            commands::save_account,
            commands::delete_account,
            commands::test_account,
            commands::get_chart,
            commands::list_chart_instruments,
            commands::backfill_chart,
            commands::place_order,
            commands::cancel_order,
            commands::modify_order,
            commands::list_orders,
            commands::list_trades,
            commands::get_dashboard,
            commands::get_diary,
            commands::exit_position,
            commands::exit_all,
            commands::get_quote,
            commands::contract_info,
            commands::refresh_instruments,
            commands::connect_broker,
            commands::sync_orders,
            commands::get_settings_json,
            commands::save_settings_json,
            commands::get_server_settings,
            commands::save_server_settings,
            commands::test_server,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| {
            error!("error while running tauri application: {e}");
            std::process::exit(1);
        });
}
