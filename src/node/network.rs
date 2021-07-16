use crate::logger::{LoggerLevel, Logger};
use crate::block::Block;
use std::io::{Write, BufReader, BufWriter};
use std::net::{TcpStream, TcpListener};
use std::thread::JoinHandle;
use std::sync::mpsc::{channel, Sender, Receiver, RecvTimeoutError, RecvError};
use std::sync::{Mutex, Arc};
use std::collections::{HashSet, HashMap};
use std::time::Duration;
use tcp_channel::{ReceiverBuilder, ChannelRecv};
use tcp_channel::{SenderBuilder, ChannelSend};
use tcp_channel::LittleEndian;
use serde::{Serialize, Deserialize};

type TcpReceiver<T> = tcp_channel::Receiver<T, LittleEndian>;
type TcpSender<T> = tcp_channel::Sender<T, LittleEndian>;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Packet
{
    KnownNode(String),
    OnConnected(u16),
    Block(Block),
    BlockRequest(u64),
    Ping,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Message
{
    Packet(String, Packet),
    Shutdown,
}

fn start_packet_reciver<W>(server_ip: String, mut recv: TcpReceiver<Packet>, 
                           message_sender: Sender<Message>, mut logger: Logger<W>) -> JoinHandle<()>
    where W: Write + Sync + Send + 'static
{
    std::thread::spawn(move || loop
    {
        match recv.recv()
        {
            Ok(packet) =>
            {
                match message_sender.send(Message::Packet(server_ip.clone(), packet)) 
                {
                    Ok(_) => {},
                    Err(err) => 
                    {
                        logger.log(LoggerLevel::Error, 
                            &format!("message_sender.send(packet): {}", err));
                        break;
                    },
                }
            },

            Err(tcp_channel::RecvError::IoError(e)) 
                if e.kind() == std::io::ErrorKind::UnexpectedEof ||
                   e.kind() == std::io::ErrorKind::ConnectionReset =>
            {
                // The stream has closed
                break;
            },
            
            Err(err) =>
            {
                logger.log(LoggerLevel::Error, &format!("recv.recv(): {}", err));
                break;
            },
        }
    })
}

fn start_node_listner<W>(port: u16, packet_sender: Arc<Mutex<ConnectionManager<W>>>,
                         should_shutdown: Arc<Mutex<bool>>, mut logger: Logger<W>) -> JoinHandle<()>
    where W: Write + Clone + Sync + Send + 'static
{
    let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).unwrap();
    std::thread::spawn(move || loop
    {
        match listener.accept()
        {
            Ok((stream, socket)) =>
            {
                let address = format!("{}:{}", socket.ip(), socket.port());
                logger.log(LoggerLevel::Info, 
                    &format!("[{}] Got connection request from {}", port, address));

                if *should_shutdown.lock().unwrap() {
                    break;
                }

                packet_sender.lock().unwrap().add_client(address, stream);
            },

            Err(_) => 
            {
                logger.log(LoggerLevel::Info, 
                    &format!("[{}] Shutdown node listner", port));

                break;
            },
        }
    })
}

pub trait PacketHandler<W>
    where W: Write + Clone + Sync + Send + 'static
{
    fn on_packet(&mut self, from: &str, packet: Packet, connection_manager: &mut ConnectionManager<W>);
}

fn handle_message_packet<P, W>(from: String, packet: Packet, connection_manager: &mut ConnectionManager<W>, 
                               port: u16, packet_handler: &mut P, logger: &mut Logger<W>)
    where P: PacketHandler<W> + Sync + Send + 'static,
          W: Write + Clone + Sync + Send + 'static
{
    match &packet
    {
        // NOTE: We don't send KnownNode packets to the handler
        Packet::KnownNode(address) =>
            connection_manager.register_node(&address, Some( &from )),

        Packet::OnConnected(node_port) =>
        {
            let ip = from.split(':').nth(0).unwrap();
            let node_address = format!("{}:{}", ip, node_port);
            if !connection_manager.open_connections.insert(node_address.clone())
            {
                logger.log(LoggerLevel::Verbose, 
                    &format!("[{}] Remove duplicate connection {}", port, node_address));

                connection_manager.disconnect_from(&from);
            }
            else
            {
                connection_manager.confirm_connection(&from, node_address.clone());
                connection_manager.register_node(&node_address, Some( &from ));
                packet_handler.on_packet(&from, packet, connection_manager);
            }
        },

        _ => 
            packet_handler.on_packet(&from, packet, connection_manager),
    }
}

fn start_message_handler<P, W>(port: u16, mut packet_handler: P, message_reciver: Receiver<Message>, 
                               connection_manager: Arc<Mutex<ConnectionManager<W>>>, mut logger: Logger<W>) -> JoinHandle<()>
    where P: PacketHandler<W> + Sync + Send + 'static,
          W: Write + Clone + Sync + Send + 'static
{
    std::thread::spawn(move || loop
    {
        match message_reciver.recv_timeout(Duration::from_millis(100))
        {
            Ok(Message::Packet(from, packet)) =>
            {
                logger.log(LoggerLevel::Verbose, 
                    &format!("[{}] Got packet {:?}", port, packet));
                logger.log(LoggerLevel::Info, &format!("[{}] Got packet", port));

                let mut connection_manager_lock = connection_manager.lock().unwrap();
                handle_message_packet(from, packet, &mut connection_manager_lock, 
                    port, &mut packet_handler, &mut logger);
                logger.log(LoggerLevel::Info, &format!("[{}] Handled packet", port));
            },

            Ok(Message::Shutdown) =>
            {
                logger.log(LoggerLevel::Info, 
                    &format!("[{}] Shut down message handler", port));
                break;
            },

            Err(RecvTimeoutError::Timeout) =>
            {
                let mut connection_manager_lock = connection_manager.lock().unwrap();
                connection_manager_lock.connect_to_known_nodes();
            },

            // TODO: Handle this
            Err(err) =>
            {
                logger.log(LoggerLevel::Error, 
                    &format!("message_reciver.recv(): {}", err));
                panic!()
            },
        }
    })
}

struct Connection
{
    stream: TcpStream,
    reciver_thread: Option<JoinHandle<()>>,
    sender: TcpSender<Packet>,
    public_address: Option<String>,
}

impl Connection
{

    pub fn new<W>(port: u16, address: &str, stream: TcpStream, message_sender: Sender<Message>, logger: Logger<W>) -> std::io::Result<Self>
        where W: Write + Sync + Send + 'static
    {
        let reciver = ReceiverBuilder::new()
            .with_type::<Packet>()
            .with_endianness::<LittleEndian>()
            .build(BufReader::new(stream.try_clone()?));
        let reciver_thread = start_packet_reciver(address.to_owned(), reciver, message_sender, logger);

        let mut sender = SenderBuilder::new()
            .with_type::<Packet>()
            .with_endianness::<LittleEndian>()
            .build(BufWriter::new(stream.try_clone()?));
        if sender.send(&Packet::OnConnected(port)).is_err() {
            return Err(std::io::Error::from(std::io::ErrorKind::NotConnected));
        }
        sender.flush()?;

        Ok(Self
        {
            stream,
            reciver_thread: Some( reciver_thread ),
            sender,
            public_address: None,
        })
    }

}

impl Drop for Connection
{

    fn drop(&mut self)
    {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        self.reciver_thread
            .take().unwrap()
            .join().expect("Join server connection");
    }

}

pub struct ConnectionManager<W>
    where W: Write + Clone + Sync + Send + 'static
{
    port: u16,
    message_sender: Sender<Message>,
    known_nodes: HashSet<String>,
    open_connections: HashSet<String>,
    connections: HashMap<String, Connection>,
    logger: Logger<W>
}

impl<W> ConnectionManager<W>
    where W: Write + Clone + Sync + Send + 'static
{

    pub fn new(port: u16, message_sender: Sender<Message>, logger: Logger<W>) -> Arc<Mutex<Self>>
    {
        Arc::from(Mutex::from(Self
        {
            port,
            message_sender,
            known_nodes: HashSet::new(),
            open_connections: HashSet::new(),
            connections: HashMap::new(),
            logger,
        }))
    }

    fn add_client(&mut self, address: String, stream: TcpStream)
    {
        self.logger.log(LoggerLevel::Info, 
            &format!("[{}] Connected to {}", self.port, address));

        match Connection::new(self.port, &address, stream, self.message_sender.clone(), self.logger.clone())
        {
            Ok(connection) => {
                self.connections.insert(address, connection);
            },

            _ => {},
        };
    }

    fn confirm_connection(&mut self, address: &str, public_address: String)
    {
        let connection = &mut self.connections.get_mut(address).unwrap();
        connection.public_address = Some( public_address );
    }

    pub fn register_node(&mut self, address: &str, from: Option<&str>)
    {
        if self.known_nodes.insert(address.to_owned())
        {
            self.logger.log(LoggerLevel::Verbose, 
                &format!("[{}] Regestered new node {}", self.port, address));
            
            if from.is_some() {
                self.send_to(Packet::KnownNode(address.to_owned()), |addr| addr != from.unwrap());
            }
        }
    }

    fn connect(&mut self, address: &str)
    {
        // TODO: Test we're not connecting to our self properly
        if address == &format!("127.0.0.1:{}", self.port) {
            return;
        }

        // Make sure we're not already connected
        if self.open_connections.contains(address)
        {
            self.logger.log(LoggerLevel::Warning,
                &format!("[{}] Already to connected to {}", self.port, address));
            return;
        }

        let stream_or_error = TcpStream::connect(address);
        if stream_or_error.is_err() 
        {
            self.logger.log(LoggerLevel::Verbose,
                &format!("[{}] Unable to connect to {}", self.port, address));
            return;
        }

        let stream = stream_or_error.unwrap();
        self.add_client(address.to_owned(), stream);
    }

    pub fn connect_to_known_nodes(&mut self)
    {
        // TODO: Limit the number of connections we make

        for address in self.known_nodes.clone() 
        {
            if !self.open_connections.contains(&address) {
                self.connect(&address);
            }
        }
    }

    pub fn send(&mut self, packet: Packet)
    {
        let mut disconnected = Vec::<String>::new();
        for (address, connection) in &mut self.connections
        {
            if connection.public_address.is_none() {
                continue;
            }

            self.logger.log(LoggerLevel::Verbose, 
                &format!("[{}] Sending {:?} to {}", self.port, packet, address));

            if connection.sender.send(&packet).is_err() 
                || connection.sender.flush().is_err()
            {
                disconnected.push(address.clone());
            }
        }

        // Remove any 
        for address in disconnected {
            self.disconnect_from(&address);
        }
    }

    pub fn send_to<F>(&mut self, packet: Packet, predicate: F)
        where F: Fn(&str) -> bool
    {
        for (address, connection) in &mut self.connections 
        {
            if connection.public_address.is_none() {
                continue;
            }

            if predicate(address)
            {
                self.logger.log(LoggerLevel::Verbose, 
                    &format!("[{}] Sending {:?} to {}", self.port, packet, address));
        
                connection.sender.send(&packet).expect("Sent packet");
                connection.sender.flush().expect("Flushed");
            }
        }
    }

    pub fn disconnect_from(&mut self, address: &str)
    {
        match self.connections.remove(address)
        {
            Some(connection) => 
                match &connection.public_address
                {
                    Some(address) => self.open_connections.remove(address),
                    None => false,
                },

            None => false,
        };
    }

}

impl<W> Drop for ConnectionManager<W>
    where W: Write + Clone + Sync + Send + 'static
{

    fn drop(&mut self)
    {
        self.logger.log(LoggerLevel::Info, 
            &format!("[{}] Shutting down {} connection(s)", self.port, self.connections.len()));
        self.connections.clear();
    }

}

pub struct NetworkConnection<W>
    where W: Write + Clone + Sync + Send + 'static
{
    port: u16,
    should_shutdown: Arc<Mutex<bool>>,
    message_sender: Sender<Message>,
    connection_manager: Arc<Mutex<ConnectionManager<W>>>,
    logger: Logger<W>,

    node_listner_thread: Option<JoinHandle<()>>,
    message_handler_thread: Option<JoinHandle<()>>,
}

impl<W> NetworkConnection<W>
    where W: Write + Clone + Sync + Send + 'static
{

    pub fn new<P>(port: u16, packet_handler: P, logger: Logger<W>) -> Self
        where P: PacketHandler<W> + Sync + Send + 'static
    {
        // Create channel for recived packets to be send through
        let (message_sender, message_reciver) = channel::<Message>();
        let connection_manager = ConnectionManager::new(port, message_sender.clone(), logger.clone());

        // Start server for other nodes to connect to
        let should_shutdown = Arc::from(Mutex::from(false));
        let node_listner_thread = start_node_listner(port, connection_manager.clone(), 
            should_shutdown.clone(), logger.clone());

        // Start thread to handle incoming packets
        let message_handler_thread = start_message_handler(port, packet_handler, 
            message_reciver, connection_manager.clone(), logger.clone());

        Self
        {
            port,
            should_shutdown,
            message_sender,
            connection_manager,
            logger,

            node_listner_thread: Some( node_listner_thread ),
            message_handler_thread: Some( message_handler_thread ),
        }
    }

    pub fn sender(&mut self) -> std::sync::MutexGuard<'_, ConnectionManager<W>>
    {
        self.connection_manager.lock().unwrap()
    }

}

impl<W> Drop for NetworkConnection<W>
    where W: Write + Clone + Sync + Send + 'static
{

    fn drop(&mut self)
    {
        self.logger.log(LoggerLevel::Info, 
            &format!("[{}] Shutting down network connection", self.port));

        // Shutdown node listner
        if self.node_listner_thread.is_some()
        {
            let node_listner_thread = self.node_listner_thread.take().unwrap();
            *self.should_shutdown.lock().unwrap() = true;
            let _ = TcpStream::connect(&format!("127.0.0.1:{}", self.port));
            node_listner_thread.join().expect("Joined server thread");
        }

        // Shutdown message handler
        if self.message_handler_thread.is_some()
        {
            let message_handler_thread = self.message_handler_thread.take().unwrap();
            self.message_sender.send(Message::Shutdown).expect("Sent shutdown message");
            message_handler_thread.join().expect("Joined message handler thread");
        }
    }

}

#[cfg(test)]
mod tests
{

    use super::*;
    use crate::logger::StdLoggerOutput;

    struct TestPacketHandler
    {
        test_sender: Mutex<Sender<Packet>>,
    }

    impl<W> PacketHandler<W> for TestPacketHandler
        where W: Write + Clone + Sync + Send + 'static
    {

        fn on_packet(&mut self, _: &str, packet: Packet, _: &mut ConnectionManager<W>)
        {
            let _ = self.test_sender.lock().unwrap().send(packet);
        }

    }

    fn create_connection<W>(port: u16, logger: Logger<W>) -> (NetworkConnection<W>, Receiver<Packet>)
        where W: Write + Clone + Sync + Send + 'static
    {
        let (send, recv) = channel();
        let packet_handler = TestPacketHandler { test_sender: Mutex::from(send) };
        let connection = NetworkConnection::new(port, packet_handler, logger);

        (connection, recv)
    }

    #[test]
    fn test_network_disconnect()
    {
        let logger = Logger::new(StdLoggerOutput::new(), LoggerLevel::Error);
        let (mut connection_a, recv_a) = create_connection(8080, logger.clone());
        {
            let (mut connection_b, _recv_b) = create_connection(8081, logger.clone());
            connection_b.sender().register_node("127.0.0.1:8080", None);
            println!("{:?}", recv_a.recv());
        }

        connection_a.sender().send(Packet::Ping);
    }

    #[test]
    fn test_network()
    {
        let logger = Logger::new(StdLoggerOutput::new(), LoggerLevel::Error);

        let (mut connection_a, recv_a) = create_connection(8000, logger.clone());
        let (mut connection_b, recv_b) = create_connection(8001, logger.clone());
        let (mut connection_c, recv_c) = create_connection(8002, logger.clone());
        connection_b.sender().register_node("127.0.0.1:8000", None);
        connection_c.sender().register_node("127.0.0.1:8000", None);

        let recv_on_connect_packets = |recv: &Receiver<Packet>, ports: &[u16]|
        {
            for _ in 0..ports.len()
            {
                match recv.recv_timeout(std::time::Duration::from_secs(10))
                {
                    Ok(Packet::OnConnected(port)) => 
                        assert_eq!(ports.contains(&port), true),

                    _ => panic!(),
                }
            }
        };

        recv_on_connect_packets(&recv_a, &[8001, 8002]);
        recv_on_connect_packets(&recv_b, &[8000, 8002]);
        recv_on_connect_packets(&recv_c, &[8001, 8000]);

        connection_a.sender().send(Packet::Ping);
        connection_b.sender().send(Packet::Ping);
        assert_eq!(recv_a.recv().expect("Got packet"), Packet::Ping);
        assert_eq!(recv_b.recv().expect("Got packet"), Packet::Ping);
        assert_eq!(recv_c.recv().expect("Got packet"), Packet::Ping);
        assert_eq!(recv_c.recv().expect("Got packet"), Packet::Ping);

        let (mut connection_d, recv_d) = create_connection(8003, logger.clone());
        connection_d.sender().register_node("127.0.0.1:8000", None);
        recv_on_connect_packets(&recv_a, &[8003]);
        recv_on_connect_packets(&recv_b, &[8003]);
        recv_on_connect_packets(&recv_c, &[8003]);
        recv_on_connect_packets(&recv_d, &[8000, 8001, 8002]);

        connection_d.sender().send(Packet::Ping);
        assert_eq!(recv_a.recv().expect("Got packet"), Packet::Ping);
        assert_eq!(recv_b.recv().expect("Got packet"), Packet::Ping);
        assert_eq!(recv_c.recv().expect("Got packet"), Packet::Ping);
    }

}
