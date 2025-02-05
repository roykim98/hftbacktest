use algo::gridtrading;
use hftbacktest::{
    connector::binancefutures::{BinanceFutures, BinanceFuturesError, Endpoint},
    live::{BotError, LiveBot, LoggingRecorder},
    prelude::{Bot, ErrorKind, HashMapMarketDepth},
};
use tracing::error;

mod algo;

const ORDER_PREFIX: &str = "prefix";
const API_KEY: &str = "apikey";
const SECRET: &str = "secret";

fn prepare_live() -> LiveBot<HashMapMarketDepth> {
    let binance_futures = BinanceFutures::builder()
        .endpoint(Endpoint::Testnet)
        .api_key(API_KEY)
        .secret(SECRET)
        .order_prefix(ORDER_PREFIX)
        .build()
        .unwrap();

    let mut hbt = LiveBot::builder()
        .register("binancefutures", binance_futures)
        .add("binancefutures", "SOLUSDT", 0.001, 1.0)
        .error_handler(|error| {
            match error.kind {
                ErrorKind::ConnectionInterrupted => {
                    error!("ConnectionInterrupted");
                }
                ErrorKind::CriticalConnectionError => {
                    error!("CriticalConnectionError");
                }
                ErrorKind::OrderError => {
                    let error: &BinanceFuturesError = error.value_downcast_ref().unwrap();
                    match error {
                        BinanceFuturesError::AssetNotFound => {
                            error!("AssetNotFound");
                        }
                        BinanceFuturesError::InvalidRequest => {
                            error!("InvalidRequest");
                        }
                        BinanceFuturesError::ReqError(error) => {
                            error!(?error, "ReqError");
                        }
                        BinanceFuturesError::OrderError(code, msg) => {
                            error!(%code, %msg, "OrderError");
                        }
                    }
                }
                ErrorKind::Custom(errno) => {
                    if errno == 1000 {
                        // Aborts the connection.
                        return Err(BotError::Custom("UserStreamError".to_string()));
                    }
                }
            }
            Ok(())
        })
        .build()
        .unwrap();

    hbt.run().unwrap();
    hbt
}

fn main() {
    tracing_subscriber::fmt::init();

    let mut hbt = prepare_live();

    let relative_half_spread = 0.0005;
    let relative_grid_interval = 0.0005;
    let grid_num = 10;
    let min_grid_step = 0.001; // tick size
    let skew = relative_half_spread / grid_num as f64;
    let order_qty = 1.0;
    let max_position = grid_num as f64 * order_qty;

    let mut recorder = LoggingRecorder::new();
    gridtrading(
        &mut hbt,
        &mut recorder,
        relative_half_spread,
        relative_grid_interval,
        grid_num,
        min_grid_step,
        skew,
        order_qty,
        max_position,
    )
    .unwrap();
    hbt.close().unwrap();
}
