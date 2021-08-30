use crate::node::network::{NetworkConnection, Packet};
use crate::node::Node;
use crate::block::Block;
use crate::block::validate::{BlockValidate, BlockValidationResult};
use crate::block;
use crate::chain::BlockChainAddResult;
use crate::wallet::private_wallet::PrivateWallet;
use crate::miner;
use crate::logger::Logger;

use std::sync::{Arc, Mutex};
use std::io::Write;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::error::Error;

fn mine_next_block<W>(network_connection: &Arc<Mutex<NetworkConnection<Node<W>, W>>>,
                      wallet: &PrivateWallet) -> Result<(), Box<dyn Error>>
    where W: Write + Clone + Sync + Send + 'static
{
    let mut block;
    {
        // Create the next block
        let mut network_connection_lock = network_connection.lock().unwrap();
        let chain = &network_connection_lock.handler().chain();
        block = Block::new(&chain, wallet).unwrap();
    }

    // Do the mining work
    block = miner::mine_block_unless_found(network_connection, block)?;
    if block.is_pow_valid()? != BlockValidationResult::Ok {
        return Ok(());
    }

    // Add it to the chain if it's still the top
    let mut network_connection_lock = network_connection.lock().unwrap();
    let chain = &mut network_connection_lock.handler().chain();
    let top = chain.top();
    if top.is_none() || top.unwrap().block_id + 1 == block.block_id 
    {
        match chain.add(&block)?
        {
            BlockChainAddResult::Ok =>
            {
                println!("Won block {}! With difficulty {}", 
                    block.block_id, 
                    block::target::difficulty(&block.target));

                network_connection_lock.manager().send(Packet::Block(block));
            },

            _ => {},
        }
    }

    Ok(())
}

pub fn start_miner_thread<W>(network_connection: Arc<Mutex<NetworkConnection<Node<W>, W>>>,
                             mut logger: Logger<W>) -> JoinHandle<()>
    where W: Write + Clone + Sync + Send + 'static
{
    // Create chain a wallet
    let wallet = PrivateWallet::read_from_file(&PathBuf::from("N4L8.wallet"), &mut logger).unwrap();

    std::thread::spawn(move || loop 
    {
        mine_next_block(&network_connection, &wallet);
        if network_connection.lock().unwrap().should_shutdown() {
            break;
        }
    })
}
