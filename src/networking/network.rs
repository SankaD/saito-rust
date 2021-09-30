use crate::blockchain::Blockchain;
use crate::consensus::SaitoMessage;
use crate::crypto::{hash, SaitoHash, SaitoPublicKey};
use crate::mempool::Mempool;
use crate::networking::api_message::APIMessage;
use crate::networking::filters::{
    get_block_route_filter, post_transaction_route_filter, ws_upgrade_route_filter,
};
use crate::networking::peer::{OutboundPeer, SaitoPeer};
use crate::util::format_url_string;

use crate::wallet::Wallet;
use futures::StreamExt;
use secp256k1::PublicKey;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use uuid::Uuid;

use std::sync::Arc;
use std::time::Duration;
use warp::{Filter, Rejection};

use super::peer::{PeerSetting, OUTBOUND_PEER_CONNECTIONS_GLOBAL, PEERS_DB_GLOBAL};

use config::Config;

pub const CHALLENGE_SIZE: usize = 82;
pub const CHALLENGE_EXPIRATION_TIME: u64 = 60000;

pub type Result<T> = std::result::Result<T, Rejection>;

//
// In addition to responding to global broadcast messages, the
// network has a local broadcast channel it uses to coordinate
// attempts to check that connections are stable and clean up
// problematic peers.
//
#[derive(Clone, Debug)]
pub enum NetworkMessage {
    LocalNetworkMonitoring,
}

pub struct Network {
    config_settings: Config,
    wallet_lock: Arc<RwLock<Wallet>>,
    mempool_lock: Arc<RwLock<Mempool>>,
    blockchain_lock: Arc<RwLock<Blockchain>>,
    broadcast_channel_sender: Option<broadcast::Sender<SaitoMessage>>,
}

impl Network {
    pub fn new(
        config_settings: Config,
        wallet_lock: Arc<RwLock<Wallet>>,
        mempool_lock: Arc<RwLock<Mempool>>,
        blockchain_lock: Arc<RwLock<Blockchain>>,
    ) -> Network {
        Network {
            config_settings,
            wallet_lock,
            mempool_lock,
            blockchain_lock,
            broadcast_channel_sender: None,
        }
    }

    pub fn set_broadcast_channel_sender(&mut self, bcs: broadcast::Sender<SaitoMessage>) {
        self.broadcast_channel_sender = Some(bcs);
    }

    pub async fn connect_to_peer(connection_id: SaitoHash, wallet_lock: Arc<RwLock<Wallet>>) {
        let peers_db_global = PEERS_DB_GLOBAL.clone();
        let peer_url;
        {
            let mut peer_db = peers_db_global.write().await;
            let peer = peer_db.get_mut(&connection_id).unwrap();
            peer_url = url::Url::parse(&format!(
                "ws://{}/wsopen",
                format_url_string(peer.get_host().unwrap(), peer.get_port().unwrap()),
            ))
            .unwrap();
            peer.set_is_connected_or_connecting(true).await;
        }

        let ws_stream_result = connect_async(peer_url).await;
        match ws_stream_result {
            Ok((ws_stream, _)) => {
                let (write_sink, mut read_stream) = ws_stream.split();
                {
                    let outbound_peer_db_global = OUTBOUND_PEER_CONNECTIONS_GLOBAL.clone();
                    outbound_peer_db_global
                        .write()
                        .await
                        .insert(connection_id, OutboundPeer { write_sink });
                }
                let publickey: SaitoPublicKey;
                {
                    let wallet = wallet_lock.read().await;
                    publickey = wallet.get_publickey();
                }
                let mut message_data = vec![127, 0, 0, 1];
                message_data.extend(
                    PublicKey::from_slice(&publickey)
                        .unwrap()
                        .serialize()
                        .to_vec(),
                );
                let _foo = tokio::spawn(async move {
                    while let Some(result) = read_stream.next().await {
                        match result {
                            Ok(message) => {
                                if !message.is_empty() {
                                    let api_message = APIMessage::deserialize(&message.into_data());
                                    SaitoPeer::handle_peer_message(api_message, connection_id)
                                        .await;
                                } else {
                                    println!("Message of length 0... why?");
                                    println!("This seems to occur if we aren't holding a reference to the sender/stream on the");
                                    println!("other end of the connection. I suspect that when the stream goes out of scope,");
                                    println!("it's deconstructor is being called and sends a 0 length message to indicate");
                                    println!("that the stream has ended... I'm leaving this println here for now because");
                                    println!("it would be very helpful to see this if starts to occur again. We may want to");
                                    println!("treat this as a disconnect.");
                                }
                            }
                            Err(error) => {
                                println!("Error reading from peer socket {}", error);
                                let peers_db_global = PEERS_DB_GLOBAL.clone();
                                let mut peer_db = peers_db_global.write().await;
                                let peer = peer_db.get_mut(&connection_id).unwrap();
                                peer.set_is_connected_or_connecting(false).await;
                            }
                        }
                    }
                });
                {
                    let peers_db_global = PEERS_DB_GLOBAL.clone();
                    let mut peer_db = peers_db_global.write().await;
                    let peer = peer_db.get_mut(&connection_id).unwrap();
                    SaitoPeer::send_command(peer, &String::from("SHAKINIT"), message_data).await;
                }
            }
            Err(error) => {
                println!("Error connecting to peer {}", error);
                let mut peer_db = peers_db_global.write().await;
                let peer = peer_db.get_mut(&connection_id).unwrap();
                peer.set_is_connected_or_connecting(false).await;
            }
        }
    }
    pub async fn connect_to_configured_peers(&self) {
        let peer_settings = match self
            .config_settings
            .get::<Vec<PeerSetting>>("network.peers")
        {
            Ok(peer_settings) => Some(peer_settings),
            Err(_) => None,
        };

        if let Some(peer_settings) = peer_settings {
            // TODO replace let peer with for peer
            // This was a problem because of peer_db_lock move in each loop...
            for peer_setting in peer_settings {
                let connection_id: SaitoHash = hash(&Uuid::new_v4().as_bytes().to_vec());

                let peer = SaitoPeer::new(
                    connection_id,
                    Some(peer_setting.host),
                    Some(peer_setting.port),
                    false,
                    false,
                    true,
                    self.wallet_lock.clone(),
                    self.mempool_lock.clone(),
                    self.blockchain_lock.clone(),
                );
                {
                    let peers_db_global = PEERS_DB_GLOBAL.clone();
                    peers_db_global
                        .write()
                        .await
                        .insert(connection_id.clone(), peer);
                }
            }
        }
    }
}

//
// Starts up the Server and Other Threads
//
pub async fn run(
    network_lock: Arc<RwLock<Network>>,
    wallet_lock: Arc<RwLock<Wallet>>,
    mempool_lock: Arc<RwLock<Mempool>>,
    blockchain_lock: Arc<RwLock<Blockchain>>,
    broadcast_channel_sender: broadcast::Sender<SaitoMessage>,
    mut broadcast_channel_receiver: broadcast::Receiver<SaitoMessage>,
) -> crate::Result<()> {
    let host: [u8; 4];
    let port: u16;

    //
    // get network settings from  configuation file
    //
    {
        let network = network_lock.read().await;
        host = network
            .config_settings
            .get::<[u8; 4]>("network.host")
            .unwrap();
        port = network.config_settings.get::<u16>("network.port").unwrap();
    }

    //
    // set global broadcast channel and connect to peers
    //
    {
        //
        // set global broadcast channel and connect to peers
        //
        let mut network = network_lock.write().await;
        network.set_broadcast_channel_sender(broadcast_channel_sender.clone());
        network.connect_to_configured_peers().await;
    }

    //
    // initialize routes for server
    //
    let routes = get_block_route_filter()
        .or(post_transaction_route_filter(
            mempool_lock.clone(),
            blockchain_lock.clone(),
        ))
        .or(ws_upgrade_route_filter(
            wallet_lock.clone(),
            mempool_lock.clone(),
            blockchain_lock.clone(),
        ));

    //
    // create local broadcast channel
    //
    let (network_channel_sender, mut network_channel_receiver) = mpsc::channel(4);

    //
    // local channel sending thread
    //
    let network_channel_sender_clone = network_channel_sender.clone();
    tokio::spawn(async move {
        loop {
            network_channel_sender_clone
                .send(NetworkMessage::LocalNetworkMonitoring)
                .await
                .expect("error: LocalNetworkMonitor message failed to send");
            sleep(Duration::from_millis(10000)).await;
        }
    });

    //
    // start the server (separate thread)
    //
    tokio::spawn(async move {
        warp::serve(routes).run((host, port)).await;
    });

    //
    // global and local channel receivers
    //
    loop {
        tokio::select! {

            //
            // local broadcast channel receivers
            //
            Some(message) = network_channel_receiver.recv() => {
                match message {
                    //
                    // reconnect to dropped peers
                    //
                    NetworkMessage::LocalNetworkMonitoring => {
                reconnect_to_dropped_peers(wallet_lock.clone()).await;
                    },
                }
            }

            //
            // global broadcast channel receivers
            //
            Ok(message) = broadcast_channel_receiver.recv() => {
                match message {
                    _ => {},
                }
            }
        }
    }
}

pub async fn reconnect_to_dropped_peers(wallet_lock: Arc<RwLock<Wallet>>) {
    let peer_states: Vec<(SaitoHash, bool)>;
    {
        let peers_db_global = PEERS_DB_GLOBAL.clone();
        let peers_db = peers_db_global.read().await;
        peer_states = peers_db
            .keys()
            .map(|connection_id| {
                let peer = peers_db.get(connection_id).unwrap();
                let should_try_reconnect =
                    peer.get_is_from_peer_list() && !peer.get_is_connected_or_connecting();
                (*connection_id, should_try_reconnect)
            })
            .collect::<Vec<(SaitoHash, bool)>>();
    }

    for (connection_id, should_try_reconnect) in peer_states {
        if should_try_reconnect {
            println!("found disconnected peer in peer settings, connecting...");
            Network::connect_to_peer(connection_id, wallet_lock.clone()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::TryInto;

    use super::*;
    use crate::{
        crypto::{generate_keys, hash, sign_blob, verify, SaitoSignature},
        mempool::Mempool,
        networking::{
            api_message::APIMessage, filters::ws_upgrade_route_filter,
            message_types::handshake_challenge::HandshakeChallenge,
        },
        transaction::Transaction,
    };
    use secp256k1::PublicKey;
    use warp::ws::Message;

    #[tokio::test]
    async fn test_message_serialize() {
        let api_message = APIMessage {
            message_name: String::from("HLLOWRLD").as_bytes().try_into().unwrap(),
            message_id: 1,
            message_data: String::from("SOMEDATA").as_bytes().try_into().unwrap(),
        };
        let serialized_api_message = api_message.serialize();

        let deserialized_api_message = APIMessage::deserialize(&serialized_api_message);
        assert_eq!(api_message, deserialized_api_message);
    }
    #[tokio::test]
    async fn test_handshake_new() {
        let mut settings = config::Config::default();
        settings.merge(config::File::with_name("config")).unwrap();

        let wallet_lock = Arc::new(RwLock::new(Wallet::new()));
        {
            let mut wallet = wallet_lock.write().await;
            wallet.load_keys("test/testwallet", Some("asdf"));
        }
        let mempool_lock = Arc::new(RwLock::new(Mempool::new(wallet_lock.clone())));
        let blockchain_lock = Arc::new(RwLock::new(Blockchain::new(wallet_lock.clone())));

        let (publickey, privatekey) = generate_keys();

        let socket_filter = ws_upgrade_route_filter(
            wallet_lock.clone(),
            mempool_lock.clone(),
            blockchain_lock.clone(),
        );
        let mut ws_client = warp::test::ws()
            .path("/wsopen")
            .handshake(socket_filter)
            .await
            .expect("handshake");

        // let base58_pubkey = PublicKey::from_slice(&publickey).unwrap().serialize().to_base58();
        let mut message_data = vec![127, 0, 0, 1];
        message_data.extend(
            PublicKey::from_slice(&publickey)
                .unwrap()
                .serialize()
                .to_vec(),
        );
        let api_message = APIMessage::new("SHAKINIT", 42, message_data);

        let serialized_api_message = api_message.serialize();

        let _socket_resp = ws_client
            .send(Message::binary(serialized_api_message))
            .await;
        let resp = ws_client.recv().await.unwrap();

        let command = String::from_utf8_lossy(&resp.as_bytes()[0..8]);
        let index: u32 = u32::from_be_bytes(resp.as_bytes()[8..12].try_into().unwrap());

        assert_eq!(command, "RESULT__");
        assert_eq!(index, 42);

        let deserialize_challenge =
            HandshakeChallenge::deserialize(&resp.as_bytes()[12..].to_vec());
        let raw_challenge: [u8; CHALLENGE_SIZE] =
            resp.as_bytes()[12..][..CHALLENGE_SIZE].try_into().unwrap();
        let sig: SaitoSignature = resp.as_bytes()[12..][CHALLENGE_SIZE..CHALLENGE_SIZE + 64]
            .try_into()
            .unwrap();

        assert_eq!(
            deserialize_challenge.challenger_ip_address(),
            [127, 0, 0, 1]
        );
        assert_eq!(deserialize_challenge.opponent_ip_address(), [127, 0, 0, 1]);
        assert_eq!(deserialize_challenge.opponent_pubkey(), publickey);
        assert!(verify(
            &hash(&raw_challenge.to_vec()),
            sig,
            deserialize_challenge.challenger_pubkey()
        ));

        let signed_challenge =
            sign_blob(&mut resp.as_bytes()[12..].to_vec(), privatekey).to_owned();

        let api_message = APIMessage::new("SHAKCOMP", 43, signed_challenge);
        let serialized_api_message = api_message.serialize();

        let _socket_resp = ws_client
            .send(Message::binary(serialized_api_message))
            .await;
        let resp = ws_client.recv().await.unwrap();

        let command = String::from_utf8_lossy(&resp.as_bytes()[0..8]);
        let index: u32 = u32::from_be_bytes(resp.as_bytes()[8..12].try_into().unwrap());
        let msg = String::from_utf8_lossy(&resp.as_bytes()[12..]);
        assert_eq!(command, "RESULT__");
        assert_eq!(index, 43);
        assert_eq!(msg, "OK");
    }

    #[tokio::test]
    async fn test_send_transaction() {
        let mut settings = config::Config::default();
        settings.merge(config::File::with_name("config")).unwrap();

        let wallet_lock = Arc::new(RwLock::new(Wallet::new()));
        {
            let mut wallet = wallet_lock.write().await;
            wallet.load_keys("test/testwallet", Some("asdf"));
        }
        let mempool_lock = Arc::new(RwLock::new(Mempool::new(wallet_lock.clone())));
        let blockchain_lock = Arc::new(RwLock::new(Blockchain::new(wallet_lock.clone())));

        let (publickey, _privatekey) = generate_keys();

        let socket_filter = ws_upgrade_route_filter(
            wallet_lock.clone(),
            mempool_lock.clone(),
            blockchain_lock.clone(),
        );
        let mut ws_client = warp::test::ws()
            .path("/wsopen")
            .handshake(socket_filter)
            .await
            .expect("transaction websocket");

        let mut message_data = vec![127, 0, 0, 1];
        message_data.extend(
            PublicKey::from_slice(&publickey)
                .unwrap()
                .serialize()
                .to_vec(),
        );
        let transaction = Transaction::new();
        let api_message = APIMessage::new("SNDTRANS", 0, transaction.serialize_for_net());
        let serialized_api_message = api_message.serialize();

        // TODO repair this test
        let _socket_resp = ws_client
            .send(Message::binary(serialized_api_message))
            .await;
        // let _resp = ws_client.recv().await.unwrap();

        // let mempool = mempool_lock.read().await;
        // assert_eq!(mempool.transactions.len(), 1);
    }

    // fn parse_response(message: Message) -> (String, u32, Vec<u8>) {
    //     let api_message = APIMessage::deserialize(message);
    //     let command = String::from_utf8_lossy(&api_message.message_name).to_string();
    //     (command, api_message.message_id, api_message.message_data)
    // }

    /****
        #[tokio::test]
        async fn test_send_block_header() {
            let mut settings = config::Config::default();
            settings.merge(config::File::with_name("config")).unwrap();

            let wallet_lock = Arc::new(RwLock::new(Wallet::new()));
            {
                let mut wallet = wallet_lock.write().await;
                wallet.load_keys("test/testwallet", Some("asdf"));
            }
            let mempool_lock = Arc::new(RwLock::new(Mempool::new(wallet_lock.clone())));
            let (blockchain_lock, block_hashes) =
                make_mock_blockchain(wallet_lock.clone(), 4 as u64).await;

            let socket_filter = ws_upgrade_route_filter(
                wallet_lock.clone(),
                mempool_lock.clone(),
                blockchain_lock.clone(),
            );
            let mut ws_client = warp::test::ws()
                .path("/wsopen")
                .handshake(socket_filter)
                .await
                .expect("transaction websocket");

            let mut message_bytes: Vec<u8> = vec![];
            message_bytes.extend_from_slice(&block_hashes[0]);
            message_bytes.extend_from_slice(&[0u8; 32]);

            let api_message = APIMessage::new("REQBLKHD", 0, message_bytes);
            let serialized_api_message = api_message.serialize();

            let _socket_resp = ws_client
                .send(Message::binary(serialized_api_message))
                .await;
            let resp = ws_client.recv().await.unwrap();

            let api_message = APIMessage::deserialize(&resp.as_bytes().to_vec());

            assert_eq!(api_message.message_name_as_string(), "RESULT__");
            assert_eq!(api_message.message_id, 0);
            assert_eq!(
                String::from_utf8_lossy(&api_message.message_data).to_string(),
                String::from("OK")
            );
        }
    ****/
    /****
        #[tokio::test]
        async fn test_send_blockchain() {
            let mut settings = config::Config::default();
            settings.merge(config::File::with_name("config")).unwrap();

            let wallet_lock = Arc::new(RwLock::new(Wallet::new()));
            {
                let mut wallet = wallet_lock.write().await;
                wallet.load_keys("test/testwallet", Some("asdf"));
            }
            let mempool_lock = Arc::new(RwLock::new(Mempool::new(wallet_lock.clone())));
            let (blockchain_lock, _block_hashes) =
                make_mock_blockchain(wallet_lock.clone(), 1 as u64).await;

            let mut settings = config::Config::default();
            settings.merge(config::File::with_name("config")).unwrap();

            let socket_filter = ws_upgrade_route_filter(
                wallet_lock.clone(),
                mempool_lock.clone(),
                blockchain_lock.clone(),
            );
            let mut ws_client = warp::test::ws()
                .path("/wsopen")
                .handshake(socket_filter)
                .await
                .expect("transaction websocket");

            //
            // first confirm the whole blockchain is received when sent zeroed block hash
            //
            let request_blockchain_message = RequestBlockchainMessage::new(0, [0; 32], [42; 32]);

            let api_message = APIMessage::new("REQCHAIN", 0, request_blockchain_message.serialize());
            let serialized_api_message = api_message.serialize();

            let _socket_resp = ws_client
                .send(Message::binary(serialized_api_message))
                .await;
            let _resp = ws_client.recv().await.unwrap();
            // let command = String::from_utf8_lossy(&resp.as_bytes()[0..8]);
            // let index: u32 = u32::from_be_bytes(resp.as_bytes()[8..12].try_into().unwrap());
            // let msg = resp.as_bytes()[12..].to_vec();

            // assert_eq!(command, "RESULT__");
            // assert_eq!(index, 0);
            // assert_eq!(String::from_utf8_lossy(&api_message.message_data).to_string(), String::from("OK"));

            // // then confirm that the program only receives three hashes
            // message_bytes = vec![];
            // message_bytes.extend_from_slice(&block_hashes[0]);
            // message_bytes.extend_from_slice(&[0u8; 32]);

            // let api_message = APIMessage::new("REQCHAIN", 0, message_bytes);
            // let serialized_api_message = api_message.serialize();

            // let _socket_resp = ws_client
            //     .send(Message::binary(serialized_api_message))
            //     .await;
            // let resp = ws_client.recv().await.unwrap();

            // let api_message = APIMessage::deserialize(&resp.as_bytes().to_vec());

            // assert_eq!(api_message.message_name_as_stringing(), "RESULT__");
            // assert_eq!(api_message.message_id, 0);

            // TODO this is length 0 on my machine...
            // assert_eq!(api_message.message_data.len(), 96);

            // TODO repair this test:
            // next block should have only 2 hashes
            // message_bytes = vec![];
            // message_bytes.extend_from_slice(&block_hashes[1]);
            // message_bytes.extend_from_slice(&[0u8; 32]);

            // let api_message = APIMessage::new("REQCHAIN", 0, message_bytes);
            // let serialized_api_message = api_message.serialize();

            // let _socket_resp = ws_client
            //     .send(Message::binary(serialized_api_message))
            //     .await;
            // let resp = ws_client.recv().await.unwrap();

            // let api_message = APIMessage::deserialize(&resp.as_bytes().to_vec());

            // assert_eq!(api_message.message_name_as_string(), "RESULT__");
            // assert_eq!(api_message.message_id, 0);
            // assert_eq!(api_message.message_data.len(), 64);

            // // next block should have only 2 hashes
            // message_bytes = vec![];
            // message_bytes.extend_from_slice(&block_hashes[2]);
            // message_bytes.extend_from_slice(&[0u8; 32]);

            // let api_message = APIMessage::new("REQCHAIN", 0, message_bytes);
            // let serialized_api_message = api_message.serialize();

            // let _socket_resp = ws_client
            //     .send(Message::binary(serialized_api_message))
            //     .await;
            // let resp = ws_client.recv().await.unwrap();

            // let api_message = APIMessage::deserialize(&resp.as_bytes().to_vec());

            // assert_eq!(api_message.message_name_as_string(), "RESULT__");
            // assert_eq!(api_message.message_id, 0);
            // assert_eq!(api_message.message_data.len(), 32);

            // // sending the latest block hash should return with nothing
            // message_bytes = vec![];
            // message_bytes.extend_from_slice(&block_hashes[3]);
            // message_bytes.extend_from_slice(&[0u8; 32]);

            // let api_message = APIMessage::new("REQCHAIN", 0, message_bytes);
            // let serialized_api_message = api_message.serialize();

            // let _socket_resp = ws_client
            //     .send(Message::binary(serialized_api_message))
            //     .await;
            // let resp = ws_client.recv().await.unwrap();

            // let api_message = APIMessage::deserialize(&resp.as_bytes().to_vec());

            // assert_eq!(api_message.message_name_as_string(), "RESULT__");
            // assert_eq!(api_message.message_id, 0);
            // assert_eq!(api_message.message_data.len(), 0);
        }
    ****/
}
