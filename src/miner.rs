use crate::block::{Block, BlockChain};
use crate::wallet::Wallet;
use crate::logger::Logger;

use std::io::Write;

pub fn mine_block(mut block: Block) -> Block
{
    while !block.validate_pow() {
        block.pow += 1;
    }

    block
}

pub fn mine<W: Wallet>(chain: &mut BlockChain, wallet: &W, count: i32, logger: &mut Logger<impl Write>)
{
    for _ in 0..count
    {
        let block = Block::new(chain, wallet).expect("Can create new block");
        chain.add(mine_block(block), logger);
    }
}
