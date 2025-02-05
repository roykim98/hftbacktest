use std::{collections::HashMap, marker::PhantomData};

use crate::{
    backtest::{
        evs::{EventIntentKind, EventSet},
        proc::{LocalProcessor, Processor},
        Asset,
        BacktestError,
    },
    depth::{HashMapMarketDepth, MarketDepth},
    prelude::{OrderId, OrderRequest},
    types::{
        Bot,
        BuildError,
        Event,
        OrdType,
        Order,
        Side,
        StateValues,
        TimeInForce,
        WaitOrderResponse,
        UNTIL_END_OF_DATA,
    },
};

/// [`Backtest`] builder.
pub struct BacktestBuilder<MD> {
    local: Vec<Box<dyn LocalProcessor<MD, Event>>>,
    exch: Vec<Box<dyn Processor>>,
}

impl<MD> BacktestBuilder<MD> {
    /// Adds [`Asset`], which will undergo simulation within the backtester.
    pub fn add(self, asset: Asset<dyn LocalProcessor<MD, Event>, dyn Processor>) -> Self {
        let mut self_ = Self { ..self };
        self_.local.push(asset.local);
        self_.exch.push(asset.exch);
        self_
    }

    /// Builds [`Backtest`].
    pub fn build(self) -> Result<Backtest<MD>, BuildError> {
        let num_assets = self.local.len();
        if self.local.len() != num_assets || self.exch.len() != num_assets {
            panic!();
        }
        Ok(Backtest {
            cur_ts: i64::MAX,
            evs: EventSet::new(num_assets),
            local: self.local,
            exch: self.exch,
        })
    }
}

/// This backtester provides multi-asset and multi-exchange model backtesting, allowing you to
/// configure different setups such as queue models or asset types for each asset. However, this may
/// result in slightly slower performance compared to [`Backtest`].
pub struct Backtest<MD> {
    cur_ts: i64,
    evs: EventSet,
    local: Vec<Box<dyn LocalProcessor<MD, Event>>>,
    exch: Vec<Box<dyn Processor>>,
}

impl<MD> Backtest<MD>
where
    MD: MarketDepth,
{
    pub fn builder() -> BacktestBuilder<MD> {
        BacktestBuilder {
            local: vec![],
            exch: vec![],
        }
    }

    pub fn new(
        local: Vec<Box<dyn LocalProcessor<MD, Event>>>,
        exch: Vec<Box<dyn Processor>>,
    ) -> Self {
        let num_assets = local.len();
        if local.len() != num_assets || exch.len() != num_assets {
            panic!();
        }
        Self {
            cur_ts: i64::MAX,
            evs: EventSet::new(num_assets),
            local,
            exch,
        }
    }

    fn initialize_evs(&mut self) -> Result<(), BacktestError> {
        for (asset_no, local) in self.local.iter_mut().enumerate() {
            match local.initialize_data() {
                Ok(ts) => self.evs.update_local_data(asset_no, ts),
                Err(BacktestError::EndOfData) => {
                    self.evs.invalidate_local_data(asset_no);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
        for (asset_no, exch) in self.exch.iter_mut().enumerate() {
            match exch.initialize_data() {
                Ok(ts) => self.evs.update_exch_data(asset_no, ts),
                Err(BacktestError::EndOfData) => {
                    self.evs.invalidate_exch_data(asset_no);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    pub fn goto_end(&mut self) -> Result<bool, BacktestError> {
        if self.cur_ts == i64::MAX {
            self.initialize_evs()?;
            match self.evs.next() {
                Some(ev) => {
                    self.cur_ts = ev.timestamp;
                }
                None => {
                    return Ok(false);
                }
            }
        }
        self.goto::<false>(UNTIL_END_OF_DATA, WaitOrderResponse::None)
    }

    fn goto<const WAIT_NEXT_FEED: bool>(
        &mut self,
        timestamp: i64,
        wait_order_response: WaitOrderResponse,
    ) -> Result<bool, BacktestError> {
        let mut timestamp = timestamp;
        for (asset_no, local) in self.local.iter().enumerate() {
            self.evs
                .update_exch_order(asset_no, local.earliest_send_order_timestamp());
            self.evs
                .update_local_order(asset_no, local.earliest_recv_order_timestamp());
        }
        loop {
            match self.evs.next() {
                Some(ev) => {
                    if ev.timestamp > timestamp {
                        self.cur_ts = timestamp;
                        return Ok(true);
                    }
                    match ev.kind {
                        EventIntentKind::LocalData => {
                            let local = unsafe { self.local.get_unchecked_mut(ev.asset_no) };
                            match local.process_data() {
                                Ok((next_ts, _)) => {
                                    self.evs.update_local_data(ev.asset_no, next_ts);
                                }
                                Err(BacktestError::EndOfData) => {
                                    self.evs.invalidate_local_data(ev.asset_no);
                                }
                                Err(e) => {
                                    return Err(e);
                                }
                            }
                            if WAIT_NEXT_FEED {
                                timestamp = ev.timestamp;
                            }
                        }
                        EventIntentKind::LocalOrder => {
                            let local = unsafe { self.local.get_unchecked_mut(ev.asset_no) };
                            let wait_order_resp_id = match wait_order_response {
                                WaitOrderResponse::Specified(
                                    wait_order_asset_no,
                                    wait_order_id,
                                ) if ev.asset_no == wait_order_asset_no => Some(wait_order_id),
                                _ => None,
                            };
                            if local.process_recv_order(ev.timestamp, wait_order_resp_id)?
                                || wait_order_response == WaitOrderResponse::Any
                            {
                                timestamp = ev.timestamp;
                            }
                            self.evs.update_local_order(
                                ev.asset_no,
                                local.earliest_recv_order_timestamp(),
                            );
                        }
                        EventIntentKind::ExchData => {
                            let exch = unsafe { self.exch.get_unchecked_mut(ev.asset_no) };
                            match exch.process_data() {
                                Ok((next_ts, _)) => {
                                    self.evs.update_exch_data(ev.asset_no, next_ts);
                                }
                                Err(BacktestError::EndOfData) => {
                                    self.evs.invalidate_exch_data(ev.asset_no);
                                }
                                Err(e) => {
                                    return Err(e);
                                }
                            }
                            self.evs.update_local_order(
                                ev.asset_no,
                                exch.earliest_send_order_timestamp(),
                            );
                        }
                        EventIntentKind::ExchOrder => {
                            let exch = unsafe { self.exch.get_unchecked_mut(ev.asset_no) };
                            let _ = exch.process_recv_order(ev.timestamp, None)?;
                            self.evs.update_exch_order(
                                ev.asset_no,
                                exch.earliest_recv_order_timestamp(),
                            );
                        }
                    }
                }
                None => {
                    return Ok(false);
                }
            }
        }
    }
}

impl<MD> Bot<MD> for Backtest<MD>
where
    MD: MarketDepth,
{
    type Error = BacktestError;

    #[inline]
    fn current_timestamp(&self) -> i64 {
        self.cur_ts
    }

    #[inline]
    fn num_assets(&self) -> usize {
        self.local.len()
    }

    #[inline]
    fn position(&self, asset_no: usize) -> f64 {
        self.local.get(asset_no).unwrap().position()
    }

    #[inline]
    fn state_values(&self, asset_no: usize) -> &StateValues {
        self.local.get(asset_no).unwrap().state_values()
    }

    fn depth(&self, asset_no: usize) -> &MD {
        self.local.get(asset_no).unwrap().depth()
    }

    fn trade(&self, asset_no: usize) -> &[Event] {
        self.local.get(asset_no).unwrap().trade()
    }

    #[inline]
    fn clear_last_trades(&mut self, asset_no: Option<usize>) {
        match asset_no {
            Some(an) => {
                let local = self.local.get_mut(an).unwrap();
                local.clear_last_trades();
            }
            None => {
                for local in self.local.iter_mut() {
                    local.clear_last_trades();
                }
            }
        }
    }

    #[inline]
    fn orders(&self, asset_no: usize) -> &HashMap<u64, Order> {
        &self.local.get(asset_no).unwrap().orders()
    }

    #[inline]
    fn submit_buy_order(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: f64,
        qty: f64,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.submit_order(
            order_id,
            Side::Buy,
            price,
            qty,
            order_type,
            time_in_force,
            self.cur_ts,
        )?;

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified(asset_no, order_id),
            );
        }
        Ok(true)
    }

    #[inline]
    fn submit_sell_order(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: f64,
        qty: f64,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.submit_order(
            order_id,
            Side::Sell,
            price,
            qty,
            order_type,
            time_in_force,
            self.cur_ts,
        )?;

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified(asset_no, order_id),
            );
        }
        Ok(true)
    }

    fn submit_order(
        &mut self,
        asset_no: usize,
        order: OrderRequest,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.submit_order(
            order.order_id,
            Side::Sell,
            order.price,
            order.qty,
            order.order_type,
            order.time_in_force,
            self.cur_ts,
        )?;

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified(asset_no, order.order_id),
            );
        }
        Ok(true)
    }

    #[inline]
    fn cancel(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.cancel(order_id, self.cur_ts)?;

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified(asset_no, order_id),
            );
        }
        Ok(true)
    }

    #[inline]
    fn clear_inactive_orders(&mut self, asset_no: Option<usize>) {
        match asset_no {
            Some(asset_no) => {
                self.local
                    .get_mut(asset_no)
                    .unwrap()
                    .clear_inactive_orders();
            }
            None => {
                for local in self.local.iter_mut() {
                    local.clear_inactive_orders();
                }
            }
        }
    }

    #[inline]
    fn wait_order_response(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        timeout: i64,
    ) -> Result<bool, BacktestError> {
        self.goto::<false>(
            self.cur_ts + timeout,
            WaitOrderResponse::Specified(asset_no, order_id),
        )
    }

    #[inline]
    fn wait_next_feed(
        &mut self,
        include_order_resp: bool,
        timeout: i64,
    ) -> Result<bool, Self::Error> {
        if self.cur_ts == i64::MAX {
            self.initialize_evs()?;
            match self.evs.next() {
                Some(ev) => {
                    self.cur_ts = ev.timestamp;
                }
                None => {
                    return Ok(false);
                }
            }
        }
        if include_order_resp {
            self.goto::<true>(self.cur_ts + timeout, WaitOrderResponse::Any)
        } else {
            self.goto::<true>(self.cur_ts + timeout, WaitOrderResponse::None)
        }
    }

    #[inline]
    fn elapse(&mut self, duration: i64) -> Result<bool, Self::Error> {
        if self.cur_ts == i64::MAX {
            self.initialize_evs()?;
            match self.evs.next() {
                Some(ev) => {
                    self.cur_ts = ev.timestamp;
                }
                None => {
                    return Ok(false);
                }
            }
        }
        self.goto::<false>(self.cur_ts + duration, WaitOrderResponse::None)
    }

    #[inline]
    fn elapse_bt(&mut self, duration: i64) -> Result<bool, Self::Error> {
        self.elapse(duration)
    }

    #[inline]
    fn close(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    fn feed_latency(&self, asset_no: usize) -> Option<(i64, i64)> {
        self.local.get(asset_no).unwrap().feed_latency()
    }

    #[inline]
    fn order_latency(&self, asset_no: usize) -> Option<(i64, i64, i64)> {
        self.local.get(asset_no).unwrap().order_latency()
    }
}

/// `MultiAssetSingleExchangeBacktest` builder.
pub struct MultiAssetSingleExchangeBacktestBuilder<Local, Exchange> {
    local: Vec<Local>,
    exch: Vec<Exchange>,
}

impl<Local, Exchange> MultiAssetSingleExchangeBacktestBuilder<Local, Exchange>
where
    Local: LocalProcessor<HashMapMarketDepth, Event> + 'static,
    Exchange: Processor + 'static,
{
    /// Adds [`Asset`], which will undergo simulation within the backtester.
    pub fn add(self, asset: Asset<Local, Exchange>) -> Self {
        let mut self_ = Self { ..self };
        self_.local.push(*asset.local);
        self_.exch.push(*asset.exch);
        self_
    }

    /// Builds [`MultiAssetSingleExchangeBacktest`].
    pub fn build(
        self,
    ) -> Result<MultiAssetSingleExchangeBacktest<HashMapMarketDepth, Local, Exchange>, BuildError>
    {
        let num_assets = self.local.len();
        if self.local.len() != num_assets || self.exch.len() != num_assets {
            panic!();
        }
        Ok(MultiAssetSingleExchangeBacktest {
            cur_ts: i64::MAX,
            evs: EventSet::new(num_assets),
            local: self.local,
            exch: self.exch,
            _md_marker: Default::default(),
        })
    }
}

/// This backtester provides multi-asset and single-exchange model backtesting, meaning all assets
/// have the same setups for models such as asset type or queue model. However, this can be slightly
/// faster than [`Backtest`]. If you need to configure different models for each asset, use
/// [`Backtest`].
pub struct MultiAssetSingleExchangeBacktest<MD, Local, Exchange> {
    cur_ts: i64,
    evs: EventSet,
    local: Vec<Local>,
    exch: Vec<Exchange>,
    _md_marker: PhantomData<MD>,
}

impl<MD, Local, Exchange> MultiAssetSingleExchangeBacktest<MD, Local, Exchange>
where
    MD: MarketDepth,
    Local: LocalProcessor<MD, Event>,
    Exchange: Processor,
{
    pub fn builder() -> MultiAssetSingleExchangeBacktestBuilder<Local, Exchange> {
        MultiAssetSingleExchangeBacktestBuilder {
            local: vec![],
            exch: vec![],
        }
    }

    pub fn new(local: Vec<Local>, exch: Vec<Exchange>) -> Self {
        let num_assets = local.len();
        if local.len() != num_assets || exch.len() != num_assets {
            panic!();
        }
        Self {
            cur_ts: i64::MAX,
            evs: EventSet::new(num_assets),
            local,
            exch,
            _md_marker: Default::default(),
        }
    }

    fn initialize_evs(&mut self) -> Result<(), BacktestError> {
        for (asset_no, local) in self.local.iter_mut().enumerate() {
            match local.initialize_data() {
                Ok(ts) => self.evs.update_local_data(asset_no, ts),
                Err(BacktestError::EndOfData) => {
                    self.evs.invalidate_local_data(asset_no);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
        for (asset_no, exch) in self.exch.iter_mut().enumerate() {
            match exch.initialize_data() {
                Ok(ts) => self.evs.update_exch_data(asset_no, ts),
                Err(BacktestError::EndOfData) => {
                    self.evs.invalidate_exch_data(asset_no);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    pub fn goto<const WAIT_NEXT_FEED: bool>(
        &mut self,
        timestamp: i64,
        wait_order_response: WaitOrderResponse,
    ) -> Result<bool, BacktestError> {
        let mut timestamp = timestamp;
        for (asset_no, local) in self.local.iter().enumerate() {
            self.evs
                .update_exch_order(asset_no, local.earliest_send_order_timestamp());
            self.evs
                .update_local_order(asset_no, local.earliest_recv_order_timestamp());
        }
        loop {
            match self.evs.next() {
                Some(ev) => {
                    if ev.timestamp > timestamp {
                        self.cur_ts = timestamp;
                        return Ok(true);
                    }
                    match ev.kind {
                        EventIntentKind::LocalData => {
                            let local = unsafe { self.local.get_unchecked_mut(ev.asset_no) };
                            match local.process_data() {
                                Ok((next_ts, _)) => {
                                    self.evs.update_local_data(ev.asset_no, next_ts);
                                }
                                Err(BacktestError::EndOfData) => {
                                    self.evs.invalidate_local_data(ev.asset_no);
                                }
                                Err(e) => {
                                    return Err(e);
                                }
                            }
                            if WAIT_NEXT_FEED {
                                timestamp = ev.timestamp;
                            }
                        }
                        EventIntentKind::LocalOrder => {
                            let local = unsafe { self.local.get_unchecked_mut(ev.asset_no) };
                            let wait_order_resp_id = match wait_order_response {
                                WaitOrderResponse::Specified(
                                    wait_order_asset_no,
                                    wait_order_id,
                                ) if ev.asset_no == wait_order_asset_no => Some(wait_order_id),
                                _ => None,
                            };
                            if local.process_recv_order(ev.timestamp, wait_order_resp_id)?
                                || wait_order_response == WaitOrderResponse::Any
                            {
                                timestamp = ev.timestamp;
                            }
                            self.evs.update_local_order(
                                ev.asset_no,
                                local.earliest_recv_order_timestamp(),
                            );
                        }
                        EventIntentKind::ExchData => {
                            let exch = unsafe { self.exch.get_unchecked_mut(ev.asset_no) };
                            match exch.process_data() {
                                Ok((next_ts, _)) => {
                                    self.evs.update_exch_data(ev.asset_no, next_ts);
                                }
                                Err(BacktestError::EndOfData) => {
                                    self.evs.invalidate_exch_data(ev.asset_no);
                                }
                                Err(e) => {
                                    return Err(e);
                                }
                            }
                            self.evs.update_local_order(
                                ev.asset_no,
                                exch.earliest_send_order_timestamp(),
                            );
                        }
                        EventIntentKind::ExchOrder => {
                            let exch = unsafe { self.exch.get_unchecked_mut(ev.asset_no) };
                            let _ = exch.process_recv_order(ev.timestamp, None)?;
                            self.evs.update_exch_order(
                                ev.asset_no,
                                exch.earliest_recv_order_timestamp(),
                            );
                        }
                    }
                }
                None => {
                    return Ok(false);
                }
            }
        }
    }
}

impl<MD, Local, Exchange> Bot<MD> for MultiAssetSingleExchangeBacktest<MD, Local, Exchange>
where
    MD: MarketDepth,
    Local: LocalProcessor<MD, Event>,
    Exchange: Processor,
{
    type Error = BacktestError;

    #[inline]
    fn current_timestamp(&self) -> i64 {
        self.cur_ts
    }

    #[inline]
    fn num_assets(&self) -> usize {
        self.local.len()
    }

    #[inline]
    fn position(&self, asset_no: usize) -> f64 {
        self.local.get(asset_no).unwrap().position()
    }

    #[inline]
    fn state_values(&self, asset_no: usize) -> &StateValues {
        self.local.get(asset_no).unwrap().state_values()
    }

    fn depth(&self, asset_no: usize) -> &MD {
        self.local.get(asset_no).unwrap().depth()
    }

    fn trade(&self, asset_no: usize) -> &[Event] {
        self.local.get(asset_no).unwrap().trade()
    }

    #[inline]
    fn clear_last_trades(&mut self, asset_no: Option<usize>) {
        match asset_no {
            Some(an) => {
                let local = self.local.get_mut(an).unwrap();
                local.clear_last_trades();
            }
            None => {
                for local in self.local.iter_mut() {
                    local.clear_last_trades();
                }
            }
        }
    }

    #[inline]
    fn orders(&self, asset_no: usize) -> &HashMap<OrderId, Order> {
        &self.local.get(asset_no).unwrap().orders()
    }

    #[inline]
    fn submit_buy_order(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: f64,
        qty: f64,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.submit_order(
            order_id,
            Side::Buy,
            price,
            qty,
            order_type,
            time_in_force,
            self.cur_ts,
        )?;
        self.evs
            .update_exch_order(asset_no, local.earliest_send_order_timestamp());

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified(asset_no, order_id),
            );
        }
        Ok(true)
    }

    #[inline]
    fn submit_sell_order(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: f64,
        qty: f64,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.submit_order(
            order_id,
            Side::Sell,
            price,
            qty,
            order_type,
            time_in_force,
            self.cur_ts,
        )?;
        self.evs
            .update_exch_order(asset_no, local.earliest_send_order_timestamp());

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified(asset_no, order_id),
            );
        }
        Ok(true)
    }

    fn submit_order(
        &mut self,
        asset_no: usize,
        order: OrderRequest,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.submit_order(
            order.order_id,
            Side::Sell,
            order.price,
            order.qty,
            order.order_type,
            order.time_in_force,
            self.cur_ts,
        )?;
        self.evs
            .update_exch_order(asset_no, local.earliest_send_order_timestamp());
        self.evs
            .update_local_order(asset_no, local.earliest_recv_order_timestamp());

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified(asset_no, order.order_id),
            );
        }
        Ok(true)
    }

    #[inline]
    fn cancel(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        let local = self.local.get_mut(asset_no).unwrap();
        local.cancel(order_id, self.cur_ts)?;
        self.evs
            .update_exch_order(asset_no, local.earliest_send_order_timestamp());
        self.evs
            .update_local_order(asset_no, local.earliest_recv_order_timestamp());

        if wait {
            return self.goto::<false>(
                UNTIL_END_OF_DATA,
                WaitOrderResponse::Specified(asset_no, order_id),
            );
        }
        Ok(true)
    }

    #[inline]
    fn clear_inactive_orders(&mut self, asset_no: Option<usize>) {
        match asset_no {
            Some(asset_no) => {
                self.local
                    .get_mut(asset_no)
                    .unwrap()
                    .clear_inactive_orders();
            }
            None => {
                for local in self.local.iter_mut() {
                    local.clear_inactive_orders();
                }
            }
        }
    }

    #[inline]
    fn wait_order_response(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        timeout: i64,
    ) -> Result<bool, BacktestError> {
        self.goto::<false>(
            self.cur_ts + timeout,
            WaitOrderResponse::Specified(asset_no, order_id),
        )
    }

    fn wait_next_feed(
        &mut self,
        include_order_resp: bool,
        timeout: i64,
    ) -> Result<bool, Self::Error> {
        if self.cur_ts == i64::MAX {
            self.initialize_evs()?;
            match self.evs.next() {
                Some(ev) => {
                    self.cur_ts = ev.timestamp;
                }
                None => {
                    return Ok(false);
                }
            }
        }
        if include_order_resp {
            self.goto::<true>(self.cur_ts + timeout, WaitOrderResponse::Any)
        } else {
            self.goto::<true>(self.cur_ts + timeout, WaitOrderResponse::None)
        }
    }

    #[inline]
    fn elapse(&mut self, duration: i64) -> Result<bool, Self::Error> {
        if self.cur_ts == i64::MAX {
            self.initialize_evs()?;
            match self.evs.next() {
                Some(ev) => {
                    self.cur_ts = ev.timestamp;
                }
                None => {
                    return Ok(false);
                }
            }
        }
        self.goto::<false>(self.cur_ts + duration, WaitOrderResponse::None)
    }

    #[inline]
    fn elapse_bt(&mut self, duration: i64) -> Result<bool, Self::Error> {
        self.elapse(duration)
    }

    #[inline]
    fn close(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    fn feed_latency(&self, asset_no: usize) -> Option<(i64, i64)> {
        self.local.get(asset_no).unwrap().feed_latency()
    }

    #[inline]
    fn order_latency(&self, asset_no: usize) -> Option<(i64, i64, i64)> {
        self.local.get(asset_no).unwrap().order_latency()
    }
}
