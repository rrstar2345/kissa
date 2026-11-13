mod accounts;
mod alerts;
mod algo;
mod backtest;
mod chain;
mod charts;
mod commands;
mod db;
mod emit_stats;
mod market;
mod popout;
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
        .on_window_event(popout::on_window_event)
        .setup(|app| {
            let paths = settings::setup_app_folders()?;
            let config = settings::load_or_create_config(&paths)?;
            info!("App root: {}", paths.root.display());

            let pool = tauri::async_runtime::block_on(db::init_pool(&paths.db_file))?;
            let (ltp, ltp_rx) = trading::ltp::LtpStore::new();
            let alerts = tauri::async_runtime::block_on(alerts::AlertService::new(pool.clone(), app.handle().clone()));
            let (trading_svc, broker_rx) = trading::service::TradingService::new(
                pool.clone(),
                config.clone(),
                ltp.clone(),
                app.handle().clone(),
                alerts.clone(),
            );
            // The saved data source decides which exchange status is polled and which streams run.
            let source = market::status::DataSource::from_setting(
                tauri::async_runtime::block_on(db::get_setting(&pool, "data.source")).ok().flatten().as_deref(),
            );
            let market_status = market::status::MarketStatusService::new(source);
            let state: SharedState = Arc::new(AppState {
                config: config.clone(),
                paths: paths.clone(),
                db: pool.clone(),
                stats: stats::new_shared_stats(),
                session: market_clock::new_shared_session_state(market_status.clone()),
                market_status,
                indices: new_index_store(),
                fno: RwLock::new(Vec::new()),
                chain: Mutex::new(std::collections::HashMap::new()),
                recorder: charts::recorder::Recorder::new(pool.clone()),
                ltp,
                trading: trading_svc.clone(),
                alerts,
                fyers_hub: market::fyers_socket::Hub::new(config.fyers.clone()),
            });
            app.manage(state.clone());
            // Pop-out windows left open last time.
            popout::restore(app.handle().clone());

            // Orders: paper matching, Dhan live updates, and the background
            // price feed for open positions / pending paper orders.
            trading_svc.start(ltp_rx, broker_rx);
            trading::feed::spawn(state.clone());
            // Algos: evaluate armed algos on every bar close of their underlying.
            algo::runner::spawn(state.clone(), app.handle().clone());

            // Exchange status (NSE, and Fyers when it is the data source): the
            // only code that calls the marketStatus APIs.
            market::status::spawn(state.clone());

            // Market-clock supervisor: keeps NSE time offset fresh and
            // re-evaluates the shared session (Active/Idle) as the clock moves.
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

            // Debug only: `KISSA_EMIT_STATS=1` logs UI emits per second.
            emit_stats::spawn();

            // Live prices: event-driven. The task sleeps until a price changes,
            // waits 50 ms so a burst of ticks becomes one `quotes` event, and
            // goes back to sleep. Nothing runs while the market is quiet.
            let (quote_app, quote_ltp) = (app.handle().clone(), state.ltp.clone());
            tauri::async_runtime::spawn(async move {
                let wake = quote_ltp.ui_wake();
                loop {
                    wake.notified().await;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let batch = quote_ltp.drain_changed();
                    if !batch.is_empty() {
                        emit_stats::count("quotes");
                        let _ = quote_app.emit("quotes", batch);
                    }
                }
            });

            // Market status: pushed only when something the UI shows changes
            // (exchange state, mode, stream state). The UI keeps its own clock.
            let (status_app, status_state) = (app.handle().clone(), state.clone());
            tauri::async_runtime::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut last = String::new();
                loop {
                    tick.tick().await;
                    let st = commands::build_market_status(&status_state).await;
                    let key = st.change_key();
                    if key != last {
                        last = key;
                        emit_stats::count("market-status");
                        let _ = status_app.emit("market-status", &st);
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
            // Index ticks are coalesced: the sink marks the index changed and
            // wakes the emitter, which sends the latest tick per index at most
            // every 50 ms and sleeps when no index is moving.
            let index_wake = Arc::new(tokio::sync::Notify::new());
            let index_dirty: Arc<std::sync::Mutex<std::collections::HashSet<String>>> = Default::default();
            {
                let (wake, dirty, store, handle) =
                    (index_wake.clone(), index_dirty.clone(), store.clone(), handle.clone());
                tauri::async_runtime::spawn(async move {
                    loop {
                        wake.notified().await;
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        let names: Vec<String> = match dirty.lock() {
                            Ok(mut d) => d.drain().collect(),
                            Err(_) => continue,
                        };
                        for name in names {
                            let tick = store.read().ok().and_then(|m| m.get(&name).cloned());
                            if let Some(tick) = tick {
                                emit_stats::count("index-tick");
                                let _ = handle.emit("index-tick", &tick);
                            }
                        }
                    }
                });
            }
            let sink: IndexSink = Arc::new(move |tick: IndexTick| {
                recorder.record(
                    &charts::instrument::index_key(&tick.index_name),
                    market_clock::get_ist_now().timestamp(),
                    tick.current_price,
                    None,
                    None,
                );
                let name = tick.index_name.clone();
                if let Ok(mut m) = store.write() {
                    m.insert(name.clone(), tick);
                }
                if let Ok(mut d) = index_dirty.lock() {
                    d.insert(name);
                }
                index_wake.notify_one();
            });
            // Fyers data socket and its index bridge (idle unless Fyers is the data source).
            state.fyers_hub.spawn(state.clone());
            market::fyers_socket::bridge::spawn(state.clone(), sink.clone());
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
            commands::fyers_login_url,
            commands::fyers_login,
            commands::fyers_login_local,
            commands::fyers_login_cancel,
            commands::get_chart,
            commands::list_chart_instruments,
            commands::list_rulesets,
            commands::save_ruleset,
            commands::delete_ruleset,
            commands::validate_ruleset,
            commands::run_backtest,
            commands::list_algos,
            commands::save_algo,
            commands::delete_algo,
            commands::set_algo_armed,
            commands::validate_algo,
            commands::list_algo_journal,
            commands::clear_algo_journal,
            commands::backfill_chart,
            commands::place_order,
            commands::cancel_order,
            commands::modify_order,
            commands::list_orders,
            commands::list_trades,
            commands::get_dashboard,
            commands::get_performance,
            commands::exit_position,
            commands::exit_all,
            commands::get_risk,
            commands::save_risk_limits,
            commands::set_kill_switch,
            commands::resume_account,
            commands::get_copier,
            commands::save_copier,
            commands::get_alert_settings,
            commands::save_alert_settings,
            commands::test_alert,
            commands::list_alert_history,
            commands::clear_alert_history,
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
            popout::open_window,
            popout::set_window_title,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| {
            error!("error while running tauri application: {e}");
            std::process::exit(1);
        });
}
