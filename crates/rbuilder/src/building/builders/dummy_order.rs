use reth_primitives::{
    Address, Transaction, TransactionKind, TransactionSignedEcRecovered, TxEip1559,
};

use crate::utils::Signer;
use alloy_primitives::U256;
pub struct DummyOrderFactory {
    chain_id: u64,
    signer: Signer,
    nonce: u64,
    receiver: Address,
    send_value: u64,
}

impl DummyOrderFactory {
    pub fn new(
        signer: Signer,
        chain_id: u64,
        nonce: u64,
        receiver: Address,
        send_value: u64,
    ) -> Self {
        Self {
            chain_id,
            signer,
            nonce,
            receiver,
            send_value,
        }
    }

    pub fn generate_tx(&mut self, basefee: U256) -> TransactionSignedEcRecovered {
        let gas_limit = basefee * U256::from(21000) + U256::from(5000);
        let tx = Transaction::Eip1559(TxEip1559 {
            chain_id: self.chain_id,
            nonce: self.nonce,
            gas_limit: gas_limit.to(),
            max_fee_per_gas: basefee.to(),
            max_priority_fee_per_gas: 0,
            to: TransactionKind::Call(self.receiver),
            value: U256::from(self.send_value),
            ..Default::default()
        });
        let res = self.signer.sign_tx(tx).expect("sign works");

        self.increase_nonce();
        res
    }

    fn increase_nonce(&mut self) {
        self.nonce += 1;
    }
}
