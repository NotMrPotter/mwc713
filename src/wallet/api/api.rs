use std::collections::HashSet;
use std::marker::PhantomData;
use uuid::Uuid;

use grin_core::ser;
use grin_util::secp::key::PublicKey;
use grin_util::secp::pedersen;
use grin_util::secp::{ContextFlag, Secp256k1};
use grin_p2p::types::PeerInfoDisplay;

use crate::contacts::GrinboxAddress;

use super::keys;
use super::tx;
use super::types::{
    AcctPathMapping, Arc, BlockFees, CbData, ContextType, Error, ErrorKind, Identifier, Keychain,
    Mutex, NodeClient, OutputData, Slate, Transaction, TxLogEntry, TxLogEntryType, TxProof,
    TxWrapper, WalletBackend, WalletInfo,
};
use super::updater;

pub struct Wallet713OwnerAPI<W: ?Sized, C, K>
where
    W: WalletBackend<C, K>,
    C: NodeClient,
    K: Keychain,
{
    pub wallet: Arc<Mutex<W>>,
    phantom: PhantomData<K>,
    phantom_c: PhantomData<C>,
}

pub struct Wallet713ForeignAPI<W: ?Sized, C, K>
where
    W: WalletBackend<C, K>,
    C: NodeClient,
    K: Keychain,
{
    pub wallet: Arc<Mutex<W>>,
    phantom: PhantomData<K>,
    phantom_c: PhantomData<C>,
}

// struct for sending back node information
pub struct NodeInfo
{
    pub height: u64,
    pub total_difficulty: u64,
    pub peers: Vec<PeerInfoDisplay>,
}

impl<W: ?Sized, C, K> Wallet713OwnerAPI<W, C, K>
where
    W: WalletBackend<C, K>,
    C: NodeClient,
    K: Keychain,
{
    pub fn new(wallet_in: Arc<Mutex<W>>) -> Self {
        Self {
            wallet: wallet_in,
            phantom: PhantomData,
            phantom_c: PhantomData,
        }
    }

    pub fn invoice_tx(
        &mut self,
        slate: &mut Slate,
        minimum_confirmations: u64,
        max_outputs: usize,
        num_change_outputs: usize,
        selection_strategy_is_use_all: bool,
        message: Option<String>,
    ) -> Result<(impl FnOnce(&mut W, &Transaction) -> Result<(), Error>), Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        let parent_key_id = w.get_parent_key_id();
        let tx = updater::retrieve_txs(&mut *w, None, Some(slate.id), Some(&parent_key_id), false, 0, 0)?;
        for t in &tx {
            if t.tx_type == TxLogEntryType::TxReceived {
                return Err(ErrorKind::TransactionAlreadyReceived(slate.id.to_string()).into());
            }
        }

        let res = tx::invoice_tx(
            &mut *w,
            slate,
            minimum_confirmations,
            max_outputs,
            num_change_outputs,
            selection_strategy_is_use_all,
            parent_key_id.clone(),
            message,
        );
        w.close()?;
        res
    }

    pub fn accounts(&self) -> Result<Vec<AcctPathMapping>, Error> {
        let mut w = self.wallet.lock();
        keys::accounts(&mut *w)
    }

    pub fn create_account_path(&self, label: &str) -> Result<Identifier, Error> {
        let mut w = self.wallet.lock();
        keys::new_acct_path(&mut *w, label)
    }

    pub fn rename_account_path(&self, old_label: &str, new_label: &str) -> Result<(), Error> {
        let accounts = self.accounts()?;
        let mut w = self.wallet.lock();
        keys::rename_acct_path(&mut *w, accounts, old_label, new_label)?;
        Ok(())
    }

   pub fn retrieve_tx_id_by_slate_id(&self, slate_id: Uuid) -> Result<u32, Error> {
       let mut w = self.wallet.lock();
       w.open_with_credentials()?;
       let tx = updater::retrieve_txs(&mut *w, None, Some(slate_id), None, false, 0, 0)?;
       let mut ret = 1000000000;
       for t in &tx {
           ret = t.id;
       }

       Ok(ret)
   }

    pub fn retrieve_outputs(
        &self,
        include_spent: bool,
        refresh_from_node: bool,
        tx_id: Option<u32>,
        pagination_start: u32,
        pagination_len: u32,
    ) -> Result<(bool, Vec<(OutputData, pedersen::Commitment)>), Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        let parent_key_id = w.get_parent_key_id();

        let mut validated = false;
        if refresh_from_node {
            validated = self.update_outputs(&mut w, false);
        }

        let res = Ok((
            validated,
            updater::retrieve_outputs(&mut *w,
                                      include_spent,
                                      tx_id,
                                      Some(&parent_key_id),
                                      pagination_start,
                                      pagination_len)?,
        ));

        w.close()?;
        res
    }

    pub fn retrieve_txs(
        &self,
        refresh_from_node: bool,
        tx_id: Option<u32>,
        tx_slate_id: Option<Uuid>,
    ) -> Result<(bool, Vec<TxLogEntry>), Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        let parent_key_id = w.get_parent_key_id();

        let mut validated = false;
        if refresh_from_node {
            validated = self.update_outputs(&mut w, false);
        }

        let res = Ok((
            validated,
            updater::retrieve_txs(&mut *w, tx_id, tx_slate_id, Some(&parent_key_id), false, 0, 0)?,
        ));

        w.close()?;
        res
    }

    pub fn retrieve_txs_with_proof_flag(
        &self,
        refresh_from_node: bool,
        tx_id: Option<u32>,
        tx_slate_id: Option<Uuid>,
        pagination_start: u32,
        pagination_length: u32,
    ) -> Result<(bool, Vec<(TxLogEntry, bool)>), Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        let parent_key_id = w.get_parent_key_id();

        let mut validated = false;
        if refresh_from_node {
            validated = self.update_outputs(&mut w, false);
        }

        let txs: Vec<TxLogEntry> =
            updater::retrieve_txs(&mut *w, tx_id, tx_slate_id, Some(&parent_key_id), false, pagination_start, pagination_length)?;
        let txs = txs
            .into_iter()
            .map(|t| {
                let tx_slate_id = t.tx_slate_id.clone();
                (
                    t,
                    tx_slate_id
                        .map(|i| w.has_stored_tx_proof(&i.to_string()).unwrap_or(false))
                        .unwrap_or(false),
                )
            })
            .collect();

        let res = Ok((validated, txs));

        w.close()?;
        res
    }

    pub fn retrieve_summary_info(
        &mut self,
        refresh_from_node: bool,
        minimum_confirmations: u64,
    ) -> Result<(bool, WalletInfo), Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        let parent_key_id = w.get_parent_key_id();

        let mut validated = false;
        if refresh_from_node {
            validated = self.update_outputs(&mut w, false);
        }

        let wallet_info = updater::retrieve_info(&mut *w, &parent_key_id, minimum_confirmations)?;
        let res = Ok((validated, wallet_info));

        w.close()?;
        res
    }

    pub fn initiate_tx(
        &mut self,
        address: Option<String>,
        amount: u64,
        minimum_confirmations: u64,
        max_outputs: usize,
        num_change_outputs: usize,
        selection_strategy_is_use_all: bool,
        message: Option<String>,
        outputs: Option<Vec<&str>>,
        version: Option<u16>,
    ) -> Result<
        (
            Slate,
            impl FnOnce(&mut W, &Transaction) -> Result<(), Error>,
        ),
        Error,
    > {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        let parent_key_id = w.get_parent_key_id();
        let (slate, mut context, lock_fn) = tx::create_send_tx(
            &mut *w,
            address,
            amount,
            minimum_confirmations,
            max_outputs,
            num_change_outputs,
            selection_strategy_is_use_all,
            &parent_key_id,
            message,
            outputs,
            version,
        )?;

        for input in slate.tx.inputs() {
            context.input_commits.push(input.commit.clone());
        }

        for output in slate.tx.outputs() {
            context.output_commits.push(output.commit.clone());
        }

        // Save the aggsig context in our DB for when we
        // recieve the transaction back
        {
            let mut batch = w.batch()?;
            batch.save_private_context(&slate.id.to_string(), &context)?;
            batch.commit()?;
        }

        w.close()?;
        Ok((slate, lock_fn))
    }

    pub fn tx_lock_outputs(
        &mut self,
        tx: &Transaction,
        lock_fn: impl FnOnce(&mut W, &Transaction) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        lock_fn(&mut *w, tx)?;
        Ok(())
    }

    pub fn finalize_tx(
        &mut self,
        slate: &mut Slate,
        tx_proof: Option<&mut TxProof>,
    ) -> Result<bool, Error> {
        let context = {
            let mut w = self.wallet.lock();
            w.open_with_credentials()?;
            let context = w.get_private_context(&slate.id.to_string())?;
            w.close()?;
            context
        };

        match context.context_type {
            ContextType::Tx => {
                let mut w = self.wallet.lock();
                w.open_with_credentials()?;
                tx::complete_tx(&mut *w, slate, &context)?;

                let tx_proof = tx_proof.map(|proof| {
                    proof.amount = context.amount;
                    proof.fee = context.fee;
                    for input in context.input_commits {
                        proof.inputs.push(input.clone());
                    }
                    for output in context.output_commits {
                        proof.outputs.push(output.clone());
                    }
                    proof
                });

                tx::update_stored_tx(&mut *w, slate, tx_proof)?;
                {
                    let mut batch = w.batch()?;
                    batch.delete_private_context(&slate.id.to_string())?;
                    batch.commit()?;
                }
                w.close()?;
                Ok(true)
            }
        }
    }

    pub fn cancel_tx(
        &mut self,
        tx_id: Option<u32>,
        tx_slate_id: Option<Uuid>,
    ) -> Result<(), Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        let parent_key_id = w.get_parent_key_id();
        if !self.update_outputs(&mut w, false) {
            return Err(ErrorKind::TransactionCancellationError(
                "Can't contact running Grin node. Not Cancelling.",
            ))?;
        }
        tx::cancel_tx(&mut *w, &parent_key_id, tx_id, tx_slate_id)?;
        w.close()?;
        Ok(())
    }

    pub fn get_stored_tx(&self, uuid: &str) -> Result<Transaction, Error> {
        let w = self.wallet.lock();
        w.get_stored_tx(uuid)
    }

    pub fn node_info(&self) -> Result<(NodeInfo), Error> {
        // first get height
        let height = {
            let mut w = self.wallet.lock();
            w.open_with_credentials()?;
            w.w2n_client().get_chain_height()
        };

        // next total_difficulty
        let total_difficulty = {
            let mut w = self.wallet.lock();
            w.open_with_credentials()?;
            w.w2n_client().get_total_difficulty()
        };

        // peer info
        let peers = {
            let mut w = self.wallet.lock();
            w.open_with_credentials()?;
            w.w2n_client().get_connected_peer_info()
        };

        // handle any errors that occurred
        match height {
           Ok(height) => {
               match total_difficulty {
                   Ok(total_difficulty) => {
                       match peers {
                           Ok(peers) => {
                               Ok(NodeInfo{height:height,total_difficulty:total_difficulty,peers:peers})
                           },
                           Err(_) => {
                               Ok(NodeInfo{height:0,total_difficulty:0,peers:Vec::new()})
                           }
                       }
                   },
                   Err(_) => {
                       Ok(NodeInfo{height:0,total_difficulty:0,peers:Vec::new()})
                   }
               }
           },
           Err(_) => {
               Ok(NodeInfo{height:0,total_difficulty:0,peers:Vec::new()})
           }
        }
    }

    pub fn post_tx(&self, tx: &Transaction, fluff: bool) -> Result<(), Error> {
        let tx_hex = grin_util::to_hex(ser::ser_vec(tx).unwrap());
        let client = {
            let mut w = self.wallet.lock();
            w.w2n_client().clone()
        };
        client.post_tx(&TxWrapper { tx_hex: tx_hex }, fluff)?;
        Ok(())
    }

    pub fn verify_slate_messages(&mut self, slate: &Slate) -> Result<(), Error> {
        slate.verify_messages()?;
        Ok(())
    }

    pub fn restore(&mut self) -> Result<(), Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        let res = w.restore();
        w.close()?;
        res
    }

    pub fn check_repair(&mut self) -> Result<(), Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        self.update_outputs(&mut w, true);
        w.check_repair()?;
        w.close()?;
        Ok(())
    }

    pub fn node_height(&mut self) -> Result<(u64, bool), Error> {
        let res = {
            let mut w = self.wallet.lock();
            w.open_with_credentials()?;
            w.w2n_client().get_chain_height()
        };
        match res {
            Ok(height) => Ok((height, true)),
            Err(_) => {
                let outputs = self.retrieve_outputs(true, false, None, 0, 0)?;
                let height = match outputs.1.iter().map(|(out, _)| out.height).max() {
                    Some(height) => height,
                    None => 0,
                };
                Ok((height, false))
            }
        }
    }

    fn update_outputs(&self, w: &mut W, update_all: bool) -> bool {
        let parent_key_id = w.get_parent_key_id();
        match updater::refresh_outputs(&mut *w, &parent_key_id, update_all) {
            Ok(_) => true,
            Err(_) => false,
        }
    }

    pub fn get_stored_tx_proof(&mut self, id: u32) -> Result<TxProof, Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        let parent_key_id = w.get_parent_key_id();
        let txs: Vec<TxLogEntry> =
            updater::retrieve_txs(&mut *w, Some(id), None, Some(&parent_key_id), false, 0, 0)?;
        if txs.len() != 1 {
            return Err(ErrorKind::TransactionHasNoProof)?;
        }
        let uuid = txs[0]
            .tx_slate_id
            .ok_or_else(|| ErrorKind::TransactionHasNoProof)?;
        w.get_stored_tx_proof(&uuid.to_string())
    }

    pub fn verify_tx_proof(
        &mut self,
        tx_proof: &TxProof,
    ) -> Result<
        (
            Option<GrinboxAddress>,
            GrinboxAddress,
            u64,
            Vec<pedersen::Commitment>,
            pedersen::Commitment,
        ),
        Error,
    > {
        let secp = &Secp256k1::with_caps(ContextFlag::Commit);

        let (destination, slate) = tx_proof
            .verify_extract(None)
            .map_err(|_| ErrorKind::VerifyProof)?;

        let inputs_ex = tx_proof.inputs.iter().collect::<HashSet<_>>();

        let mut inputs: Vec<pedersen::Commitment> = slate
            .tx
            .inputs()
            .iter()
            .map(|i| i.commitment())
            .filter(|c| !inputs_ex.contains(c))
            .collect();

        let outputs_ex = tx_proof.outputs.iter().collect::<HashSet<_>>();

        let outputs: Vec<pedersen::Commitment> = slate
            .tx
            .outputs()
            .iter()
            .map(|o| o.commitment())
            .filter(|c| !outputs_ex.contains(c))
            .collect();

        let excess = &slate.participant_data[1].public_blind_excess;

        let excess_parts: Vec<&PublicKey> = slate
            .participant_data
            .iter()
            .map(|p| &p.public_blind_excess)
            .collect();
        let excess_sum =
            PublicKey::from_combination(secp, excess_parts).map_err(|_| ErrorKind::VerifyProof)?;

        let commit_amount = secp.commit_value(tx_proof.amount)?;
        inputs.push(commit_amount);

        let commit_excess = secp.commit_sum(outputs.clone(), inputs)?;
        let pubkey_excess = commit_excess.to_pubkey(secp)?;

        if excess != &pubkey_excess {
            return Err(ErrorKind::VerifyProof.into());
        }

        let mut input_com: Vec<pedersen::Commitment> =
            slate.tx.inputs().iter().map(|i| i.commitment()).collect();

        let mut output_com: Vec<pedersen::Commitment> =
            slate.tx.outputs().iter().map(|o| o.commitment()).collect();

        input_com.push(secp.commit(0, slate.tx.offset.secret_key(secp)?)?);

        output_com.push(secp.commit_value(slate.fee)?);

        let excess_sum_com = secp.commit_sum(output_com, input_com)?;

        if excess_sum_com.to_pubkey(secp)? != excess_sum {
            return Err(ErrorKind::VerifyProof.into());
        }

        return Ok((
            destination,
            tx_proof.address.clone(),
            tx_proof.amount,
            outputs,
            excess_sum_com,
        ));
    }
}

impl<W: ?Sized, C, K> Wallet713ForeignAPI<W, C, K>
where
    W: WalletBackend<C, K>,
    C: NodeClient,
    K: Keychain,
{
    pub fn new(wallet_in: Arc<Mutex<W>>) -> Self {
        Self {
            wallet: wallet_in,
            phantom: PhantomData,
            phantom_c: PhantomData,
        }
    }

    pub fn initiate_receive_tx(
        &mut self,
        amount: u64,
        num_outputs: usize,
        message: Option<String>,
    ) -> Result<
        (
            Slate,
            impl FnOnce(&mut W, &Transaction) -> Result<(), Error>,
        ),
        Error,
    > {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        let parent_key_id = w.get_parent_key_id();
        let (slate, context, add_fn) =
            tx::create_receive_tx(&mut *w, amount, num_outputs, &parent_key_id, message)?;

        {
            let mut batch = w.batch()?;
            batch.save_private_context(&slate.id.to_string(), &context)?;
            batch.commit()?;
        }

        w.close()?;
        Ok((slate, add_fn))
    }

    pub fn tx_add_invoice_outputs(
        &mut self,
        slate: &Slate,
        add_fn: impl FnOnce(&mut W, &Transaction) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        add_fn(&mut *w, &slate.tx)?;
        Ok(())
    }

    pub fn build_coinbase(&mut self, block_fees: &BlockFees) -> Result<CbData, Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        let res = updater::build_coinbase(&mut *w, block_fees);
        w.close()?;
        res
    }

    pub fn receive_tx(
        &mut self,
        address: Option<String>,
        slate: &mut Slate,
        message: Option<String>,
    ) -> Result<(), Error> {
        let mut w = self.wallet.lock();
        w.open_with_credentials()?;
        let parent_key_id = w.get_parent_key_id();
        // Don't do this multiple times
        let tx = updater::retrieve_txs(&mut *w, None, Some(slate.id), Some(&parent_key_id), false, 0, 0)?;
        for t in &tx {
            if t.tx_type == TxLogEntryType::TxReceived {
                return Err(ErrorKind::TransactionAlreadyReceived(slate.id.to_string()).into());
            }
        }
        let res = tx::receive_tx(&mut *w, address, slate, &parent_key_id, message);
        w.close()?;

        if let Err(e) = res {
            Err(e)
        } else {
            Ok(())
        }
    }
}
