// Copyright 2022 TiKV Project Authors. Licensed under Apache-2.0.

use engine_traits::{data_cf_offset, KvEngine, Mutable, RaftEngine, CF_DEFAULT};
use kvproto::{import_sstpb::SstMeta, raft_cmdpb::RaftRequestHeader};
use raftstore::{
    store::{
        check_sst_for_ingestion, cmd_resp,
        fsm::{apply, MAX_PROPOSAL_SIZE_RATIO},
        metrics::PEER_WRITE_CMD_COUNTER,
        msg::ErrorCallback,
        util::{self, NORMAL_REQ_CHECK_CONF_VER, NORMAL_REQ_CHECK_VER},
    },
    Error, Result,
};
use slog::error;
use tikv_util::slog_panic;

use crate::{
    batch::StoreContext,
    fsm::ApplyResReporter,
    raft::{Apply, Peer},
    router::{ApplyTask, CmdResChannel},
};

mod simple_write;

pub use simple_write::{
    SimpleWriteBinary, SimpleWriteEncoder, SimpleWriteReqDecoder, SimpleWriteReqEncoder,
};

pub use self::simple_write::SimpleWrite;

impl<EK: KvEngine, ER: RaftEngine> Peer<EK, ER> {
    #[inline]
    pub fn on_simple_write<T>(
        &mut self,
        ctx: &mut StoreContext<EK, ER, T>,
        header: Box<RaftRequestHeader>,
        data: SimpleWriteBinary,
        ch: CmdResChannel,
    ) {
        if !self.serving() {
            apply::notify_req_region_removed(self.region_id(), ch);
            return;
        }
        if let Some(encoder) = self.simple_write_encoder_mut() {
            if encoder.amend(&header, &data) {
                encoder.add_response_channel(ch);
                self.set_has_ready();
                return;
            }
        }
        if let Err(e) = self.validate_command(&header, None, &mut ctx.raft_metrics) {
            let resp = cmd_resp::new_error(e);
            ch.report_error(resp);
            return;
        }
        // To maintain propose order, we need to make pending proposal first.
        self.propose_pending_writes(ctx);
        if let Some(conflict) = self.proposal_control_mut().check_conflict(None) {
            conflict.delay_channel(ch);
            return;
        }
        if self.proposal_control().has_pending_prepare_merge()
            || self.proposal_control().is_merging()
        {
            let resp = cmd_resp::new_error(Error::ProposalInMergingMode(self.region_id()));
            ch.report_error(resp);
            return;
        }
        // ProposalControl is reliable only when applied to current term.
        let call_proposed_on_success = self.applied_to_current_term();
        let mut encoder = SimpleWriteReqEncoder::new(
            header,
            data,
            (ctx.cfg.raft_entry_max_size.0 as f64 * MAX_PROPOSAL_SIZE_RATIO) as usize,
            call_proposed_on_success,
        );
        encoder.add_response_channel(ch);
        self.set_has_ready();
        self.simple_write_encoder_mut().replace(encoder);
    }

    #[inline]
    pub fn on_unsafe_write<T>(
        &mut self,
        ctx: &mut StoreContext<EK, ER, T>,
        data: SimpleWriteBinary,
    ) {
        if !self.serving() {
            return;
        }
        let bin = SimpleWriteReqEncoder::new(
            Box::<RaftRequestHeader>::default(),
            data,
            ctx.cfg.raft_entry_max_size.0 as usize,
            false,
        )
        .encode()
        .0
        .into_boxed_slice();
        if let Some(scheduler) = self.apply_scheduler() {
            scheduler.send(ApplyTask::UnsafeWrite(bin));
        }
    }

    pub fn propose_pending_writes<T>(&mut self, ctx: &mut StoreContext<EK, ER, T>) {
        if let Some(encoder) = self.simple_write_encoder_mut().take() {
            let call_proposed_on_success = if encoder.notify_proposed() {
                // The request has pass conflict check and called all proposed callbacks.
                false
            } else {
                // Epoch may have changed since last check.
                let from_epoch = encoder.header().get_region_epoch();
                let res = util::compare_region_epoch(
                    from_epoch,
                    self.region(),
                    NORMAL_REQ_CHECK_CONF_VER,
                    NORMAL_REQ_CHECK_VER,
                    true,
                );
                if let Err(e) = res {
                    // TODO: query sibling regions.
                    ctx.raft_metrics.invalid_proposal.epoch_not_match.inc();
                    encoder.encode().1.report_error(cmd_resp::new_error(e));
                    return;
                }
                // Only when it applies to current term, the epoch check can be reliable.
                self.applied_to_current_term()
            };
            let (data, chs) = encoder.encode();
            let res = self.propose(ctx, data);
            self.post_propose_command(ctx, res, chs, call_proposed_on_success);
        }
    }
}

impl<EK: KvEngine, R: ApplyResReporter> Apply<EK, R> {
    #[inline]
    pub fn apply_put(&mut self, cf: &str, index: u64, key: &[u8], value: &[u8]) -> Result<()> {
        PEER_WRITE_CMD_COUNTER.put.inc();
        let off = data_cf_offset(cf);
        if self.should_skip(off, index) {
            return Ok(());
        }
        util::check_key_in_region(key, self.region())?;
        if let Some(s) = self.buckets.as_mut() {
            s.write_key(key, value.len() as u64);
        }
        // Technically it's OK to remove prefix for raftstore v2. But rocksdb doesn't
        // support specifying infinite upper bound in various APIs.
        keys::data_key_with_buffer(key, &mut self.key_buffer);
        self.ensure_write_buffer();
        let res = if cf.is_empty() || cf == CF_DEFAULT {
            // TODO: use write_vector
            self.write_batch
                .as_mut()
                .unwrap()
                .put(&self.key_buffer, value)
        } else {
            self.write_batch
                .as_mut()
                .unwrap()
                .put_cf(cf, &self.key_buffer, value)
        };
        res.unwrap_or_else(|e| {
            slog_panic!(
                self.logger,
                "failed to write";
                "key" => %log_wrappers::Value::key(key),
                "value" => %log_wrappers::Value::value(value),
                "cf" => cf,
                "error" => ?e
            );
        });
        fail::fail_point!("APPLY_PUT", |_| Err(raftstore::Error::Other(
            "aborted by failpoint".into()
        )));
        self.metrics.size_diff_hint += (self.key_buffer.len() + value.len()) as i64;
        if index != u64::MAX {
            self.modifications_mut()[off] = index;
        }
        Ok(())
    }

    #[inline]
    pub fn apply_delete(&mut self, cf: &str, index: u64, key: &[u8]) -> Result<()> {
        PEER_WRITE_CMD_COUNTER.delete.inc();
        let off = data_cf_offset(cf);
        if self.should_skip(off, index) {
            return Ok(());
        }
        util::check_key_in_region(key, self.region())?;
        if let Some(s) = self.buckets.as_mut() {
            s.write_key(key, 0);
        }
        keys::data_key_with_buffer(key, &mut self.key_buffer);
        self.ensure_write_buffer();
        let res = if cf.is_empty() || cf == CF_DEFAULT {
            // TODO: use write_vector
            self.write_batch.as_mut().unwrap().delete(&self.key_buffer)
        } else {
            self.write_batch
                .as_mut()
                .unwrap()
                .delete_cf(cf, &self.key_buffer)
        };
        res.unwrap_or_else(|e| {
            slog_panic!(
                self.logger,
                "failed to delete";
                "key" => %log_wrappers::Value::key(key),
                "cf" => cf,
                "error" => ?e
            );
        });
        self.metrics.size_diff_hint -= self.key_buffer.len() as i64;
        if index != u64::MAX {
            self.modifications_mut()[off] = index;
        }
        Ok(())
    }

    #[inline]
    pub fn apply_delete_range(
        &mut self,
        _cf: &str,
        _index: u64,
        _start_key: &[u8],
        _end_key: &[u8],
        _notify_only: bool,
    ) -> Result<()> {
        // TODO: reuse the same delete as split/merge.
        Ok(())
    }

    #[inline]
    pub fn apply_ingest(&mut self, ssts: Vec<SstMeta>) -> Result<()> {
        PEER_WRITE_CMD_COUNTER.ingest_sst.inc();
        let mut infos = Vec::with_capacity(ssts.len());
        for sst in &ssts {
            if let Err(e) = check_sst_for_ingestion(sst, self.region()) {
                error!(
                    self.logger,
                    "ingest fail";
                    "sst" => ?sst,
                    "region" => ?self.region(),
                    "error" => ?e
                );
                let _ = self.sst_importer().delete(sst);
                return Err(e);
            }
            match self.sst_importer().validate(sst) {
                Ok(meta_info) => infos.push(meta_info),
                Err(e) => {
                    slog_panic!(self.logger, "corrupted sst"; "sst" => ?sst, "error" => ?e);
                }
            }
        }
        // Unlike v1, we can't batch ssts accross regions.
        self.flush();
        if let Err(e) = self.sst_importer().ingest(&infos, self.tablet()) {
            slog_panic!(self.logger, "ingest fail"; "ssts" => ?ssts, "error" => ?e);
        }
        Ok(())
    }
}
