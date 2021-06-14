use saito_rust::{block::Block, blockchain::AddBlockEvent, keypair::Keypair, slip::{OutputSlip, SlipType}, test_utilities, time::create_timestamp, transaction::{Transaction, TransactionType}};
use secp256k1::Signature;

#[tokio::main]
pub async fn main() -> saito_rust::Result<()> {
    let keypair = Keypair::new();

	let (mut blockchain, mut slips) =
        test_utilities::make_mock_blockchain_and_slips(&keypair, 3 * 100000);
	let prev_block = blockchain.latest_block().unwrap();
    
    let mut prev_block_hash = prev_block.hash().clone();
    let mut prev_block_id = prev_block.id();

    let mut add_block_timestamps = vec![];
    let mut start_ts;
    let mut finish_ts;

    for _ in 0..100 {
        let mut txs = vec![];
        for _ in 0..1000 {
            let slip_pair = slips.pop().unwrap();
            let to_slip = OutputSlip::new(*keypair.public_key(), SlipType::Normal, slip_pair.1.amount());
            txs.push(Transaction::new(
                Signature::from_compact(&[0; 64]).unwrap(),
                vec![],
                create_timestamp(),
                vec![slip_pair.0],
                vec![to_slip],
                TransactionType::Normal,
                vec![],
            ));

            // txs.push(test_utilities::make_mock_sig_tx(
            //     &keypair,
            //     slips.pop().unwrap().0,
            //     10,
            //     *keypair.public_key(),
            //     1024,
            // ))
                
        }
        let block = Block::new_mock(prev_block_hash, &mut txs, prev_block_id + 1);
        prev_block_hash = block.hash().clone();
        prev_block_id = block.id();

        start_ts = create_timestamp();
        let result = blockchain.add_block(block);
        assert!(result == AddBlockEvent::AcceptedAsLongestChain);
        finish_ts = create_timestamp();
        add_block_timestamps.push(finish_ts - start_ts);
    }

    let add_block_sum: u64 = add_block_timestamps.iter().sum();
    let add_block_len: u64 = add_block_timestamps.len() as u64;
    let add_block_avg = add_block_sum as f32 / add_block_len as f32;
    println!("AVERAGE ADD BLOCK TIME: {:?}", add_block_avg);

    Ok(())
}
