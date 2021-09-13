use super::Node;
use crate::network::packet::{Packet, PacketHandler};
use crate::network::client_manager::ClientManager;

use std::sync::{Arc, Mutex, MutexGuard};
use std::error::Error;

#[derive(Clone)]
pub struct NodePacketHandler
{
    node: Arc<Mutex<Node>>,
}

impl NodePacketHandler
{

    pub fn new(node: Arc<Mutex<Node>>) -> Self
    {
        Self
        {
            node,
        }
    }

    pub fn node(&self) -> MutexGuard<Node>
    {
        self.node.lock().unwrap()
    }

}

impl PacketHandler for NodePacketHandler
{

    fn handle(&self, from: &str, packet: Packet, manager: &mut ClientManager)
        -> Result<(), Box<dyn Error>>
    {
        let mut node = self.node.lock().unwrap();
        match packet
        {
            Packet::OnConnected => 
            {
                match node.chain.top()
                {
                    Some(top) =>
                    {
                        let data = node.data_store.for_page_updates(&top.pages)?;
                        manager.send_to(Packet::Block(top.clone(), data),
                            |addr| addr == from)?;
                    },
                    None => {},
                }
            },

            Packet::Block(block, data) => 
                node.handle_block(manager, from, block, data)?,

            Packet::BlockRequest(id) =>
                node.handle_block_request(manager, from, id)?,

            Packet::Transfer(transfer) =>
                node.handle_transfer(manager, from, transfer)?,

            Packet::Page(page, data) =>
                node.handle_page(manager, from, page, data)?,
            
            Packet::Ping => 
                info!("Ping!"),
        }

        Ok(())
    }

}

