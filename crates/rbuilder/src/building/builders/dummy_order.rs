use reth_primitives::{
    Address, Transaction, TransactionKind, TransactionSignedEcRecovered, TxEip1559,
};

use crate::{
    primitives::{MempoolTx, Order, TransactionSignedEcRecoveredWithBlobs},
    utils::Signer,
};
use alloy_primitives::U256;
pub struct DummyOrderFactory {
    chain_id: u64,
    signer: Signer,
    nonce: u64,
    receiver: Address,
}

impl DummyOrderFactory {
    pub fn new(signer: Signer, chain_id: u64, nonce: u64, receiver: Address) -> Self {
        Self {
            chain_id,
            signer,
            nonce,
            receiver,
        }
    }

    pub fn generate_tx(&self, basefee: U256) -> TransactionSignedEcRecovered {
        let gas_limit = basefee * U256::from(21000) + U256::from(5000);
        let tx = Transaction::Eip1559(TxEip1559 {
            chain_id: self.chain_id,
            nonce: self.nonce,
            gas_limit: gas_limit.to(),
            max_fee_per_gas: basefee.to(),
            max_priority_fee_per_gas: 0,
            to: TransactionKind::Call(self.receiver),
            value: U256::from(2000000000000000u64),
            ..Default::default()
        });
        self.signer.sign_tx(tx).expect("sign works")
    }

    pub fn generate_order(&self, basefee: U256) -> Order {
        let signed_tx = self.generate_tx(basefee);
        let tx_with_blob =
            TransactionSignedEcRecoveredWithBlobs::new_no_blobs(signed_tx).expect("blob tx");

        Order::Tx(MempoolTx::new(tx_with_blob))
    }

    pub fn increase_nonce(&mut self) {
        self.nonce += 1;
    }
}
