// Copyright (c) 2019 Chaintope Inc.
// Distributed under the MIT software license, see the accompanying
// file COPYING or http://www.opensource.org/licenses/mit-license.php.

mod message_processor;
mod node_parameters;
pub mod node_state;
mod utils;

pub use crate::signer_node::node_parameters::NodeParameters;
pub use crate::signer_node::node_state::NodeState;

use crate::crypto::multi_party_schnorr::*;
use crate::net::MessageType::{BlockGenerationRoundMessages, KeyGenerationMessage};
use crate::net::{
    BlockGenerationRoundMessageType, ConnectionManager, KeyGenerationMessageType, Message,
    MessageType, SignerID,
};
use crate::rpc::{GetBlockchainInfoResult, TapyrusApi};
use crate::sign::Sign;
use crate::signer_node::message_processor::create_block_vss;
use crate::signer_node::message_processor::process_blockparticipants;
use crate::signer_node::message_processor::process_blocksig;
use crate::signer_node::message_processor::process_blockvss;
use crate::signer_node::message_processor::process_candidateblock;
use crate::signer_node::message_processor::process_completedblock;
use crate::signer_node::message_processor::process_nodevss;
use crate::signer_node::node_state::builder::{Builder, Master, Member};
use crate::timer::RoundTimeOutObserver;
use crate::util::*;
use curv::cryptographic_primitives::secret_sharing::feldman_vss::VerifiableSS;
use curv::elliptic::curves::traits::*;
use curv::{FE, GE};
use redis::ControlFlow;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::time::Duration;

/// Round interval.
pub static ROUND_INTERVAL_DEFAULT_SECS: u64 = 60;
/// Round time limit delta. Round timeout timer should be little longer than `ROUND_INTERVAL_DEFAULT_SECS`.
static ROUND_TIMELIMIT_DELTA: u64 = 10;

pub struct SignerNode<T: TapyrusApi, C: ConnectionManager> {
    connection_manager: C,
    params: NodeParameters<T>,
    current_state: NodeState,
    stop_signal: Option<Receiver<u32>>,
    /// ## Round Timer
    /// If the round duration is over, notify it and go through next round.
    ///
    /// Round timer must follow below rules.
    /// * The timer is started on rounds start only.
    /// * New round is started on only receiving completedblock message
    ///   or previous round is timeout.
    round_timer: RoundTimeOutObserver,
    priv_shared_keys: Option<SharedKeys>,
    shared_secrets: SharedSecretMap,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SharedSecret {
    pub vss: VerifiableSS,
    pub secret_share: FE,
}

pub type SharedSecretMap = BTreeMap<SignerID, SharedSecret>;

pub type BidirectionalSharedSecretMap = BTreeMap<SignerID, (SharedSecret, SharedSecret)>;

pub trait ToVerifiableSS {
    fn to_vss(&self) -> Vec<VerifiableSS>;
}

impl ToVerifiableSS for SharedSecretMap {
    fn to_vss(&self) -> Vec<VerifiableSS> {
        self.values().map(|i| i.vss.clone()).collect()
    }
}

pub trait ToSharedSecretMap {
    fn for_negative(&self) -> SharedSecretMap;
    fn for_positive(&self) -> SharedSecretMap;
}

impl ToSharedSecretMap for BidirectionalSharedSecretMap {
    fn for_positive(&self) -> SharedSecretMap {
        let mut map = SharedSecretMap::new();
        for (key, value) in self.iter() {
            map.insert(*key, value.0.clone());
        }
        map
    }
    fn for_negative(&self) -> SharedSecretMap {
        let mut map = SharedSecretMap::new();
        for (key, value) in self.iter() {
            map.insert(*key, value.1.clone());
        }
        map
    }
}
pub trait ToShares {
    fn to_shares(&self) -> Vec<FE>;
}

impl ToShares for SharedSecretMap {
    fn to_shares(&self) -> Vec<FE> {
        self.values().map(|i| i.secret_share).collect()
    }
}

static INITIAL_MASTER_INDEX: usize = 0;

impl<T: TapyrusApi, C: ConnectionManager> SignerNode<T, C> {
    pub fn new(connection_manager: C, params: NodeParameters<T>) -> Self
    where
        Self: Sized,
    {
        let timer_limit = params.round_duration + ROUND_TIMELIMIT_DELTA;
        SignerNode {
            connection_manager,
            params,
            current_state: NodeState::Joining,
            stop_signal: None,
            round_timer: RoundTimeOutObserver::new("round_timer", timer_limit),
            priv_shared_keys: None,
            shared_secrets: BTreeMap::new(),
        }
    }

    pub fn stop_handler(&mut self, receiver: Receiver<u32>) {
        self.stop_signal = Some(receiver);
    }

    pub fn start(&mut self) {
        if !self.params.skip_waiting_ibd {
            self.wait_for_ibd_finish(std::time::Duration::from_secs(10));
        } else {
            log::info!("Skip waiting for ibd finish.")
        }

        log::info!("Start thread for redis subscription");
        let (sender, receiver): (Sender<Message>, Receiver<Message>) = channel();
        let closure = move |message: Message| match sender.send(message) {
            Ok(_) => ControlFlow::Continue,
            Err(error) => {
                log::warn!("Happened error!: {:?}", error);
                ControlFlow::Break(())
            }
        };
        let id = self.params.signer_id;
        let _handler = self.connection_manager.start(closure, id);

        log::info!("Start Key generation Protocol");
        // Idle 5s, before node starts Key Generation Protocol communication.
        // To avoid that nodes which is late to startup can't receive messages.
        log::info!("Idle 5 secs... ");
        std::thread::sleep(Duration::from_secs(5));
        self.create_node_share();

        // Start First Round
        log::info!("Start block creation rounds.");
        self.start_next_round(INITIAL_MASTER_INDEX);

        // get error_handler that is for catch error within connection_manager.
        let connection_manager_error_handler = self.connection_manager.error_handler();
        loop {
            // After process when received message. Get message from receiver,
            // then change that state in main thread side.
            // messageを受け取った後の処理。receiverからmessageを受け取り、
            // stateの変更はmain thread側で行う。
            match &self.stop_signal {
                Some(ref r) => match r.try_recv() {
                    Ok(_) => {
                        log::warn!("Stop by Terminate Signal.");
                        self.round_timer.stop();
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        // Stop signal is empty. Continue to run. Do nothing.
                    }
                    Err(e) => {
                        panic!("{:?}", e);
                    }
                },
                None => {
                    // Stop signal receiver is not set. Do nothing.
                }
            }

            // Receiving message.
            match receiver.try_recv() {
                Ok(Message {
                    message_type,
                    sender_id,
                    ..
                }) => {
                    log::debug!(
                        "Got {} message from {:?}. MessageType: {:?}",
                        message_type,
                        sender_id,
                        message_type
                    );

                    match message_type {
                        KeyGenerationMessage(msg) => {
                            self.process_key_generation_message(&sender_id, msg);
                        }
                        BlockGenerationRoundMessages(msg) => {
                            let next = self.process_round_message(&sender_id, msg);
                            self.current_state = next;

                            if let NodeState::RoundComplete {
                                next_master_index, ..
                            } = &self.current_state
                            {
                                let v = *next_master_index;
                                self.start_next_round(v)
                            }
                        }
                    }

                    log::debug!("Current state updated as {:?}", self.current_state);
                }
                Err(TryRecvError::Empty) => {
                    // No new messages. Do nothing.
                }
                Err(e) => log::debug!("{:?}", e),
            }

            // Checking whether the time limit of a round exceeds.
            match self.round_timer.receiver.try_recv() {
                Ok(_) => {
                    // Round duration is timeout. Starting next round.
                    let next_master_index = next_master_index(&self.current_state, &self.params);
                    self.start_next_round(next_master_index);
                    log::debug!("Current state updated as {:?}", self.current_state);
                }
                Err(TryRecvError::Empty) => {
                    // Still waiting round duration interval. Do nothing.
                }
                Err(e) => {
                    log::debug!("{:?}", e);
                }
            }
            // Checking network connection error
            match connection_manager_error_handler {
                Some(ref receiver) => match receiver.try_recv() {
                    Ok(e) => {
                        self.round_timer.stop();
                        log::error!("Connection Manager Error {:?}", e);
                        panic!(e.to_string());
                    }
                    Err(TryRecvError::Empty) => {
                        // No errors.
                    }
                    Err(e) => log::debug!("{:?}", e),
                },
                None => {
                    log::warn!("Failed to get error_handler of connection_manager!");
                }
            }

            // Wait for next loop 300 ms.
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Signer Node waits for connected Tapyrus Core Node complete IBD(Initial Block Download).
    fn wait_for_ibd_finish(&self, interval: Duration) {
        log::info!("Waiting finish Initial Block Download ...");
        log::info!("If you start right away, you can set `--skip-waiting-ibd` option. ");

        loop {
            match self
                .params
                .rpc
                .getblockchaininfo()
                .expect("RPC connection failed")
            {
                GetBlockchainInfoResult {
                    initialblockdownload: false,
                    ..
                } => {
                    break;
                }
                GetBlockchainInfoResult {
                    initialblockdownload: true,
                    blocks: height,
                    bestblockhash: hash,
                    ..
                } => {
                    log::info!("Waiting for finish Initial Block Download. Current block height: {}, current best hash: {}", height, hash);
                }
            }
            std::thread::sleep(interval);
        }
    }

    pub fn start_new_round(&mut self) -> NodeState {
        std::thread::sleep(Duration::from_secs(self.params.round_duration));

        let block = match self.params.rpc.getnewblock(&self.params.address) {
            Ok(block) => block,
            Err(e) => {
                log::error!("RPC getnewblock failed. reason={:?}", e);
                //Behave as master without block.
                return Master::default().build();
            }
        };

        log::info!(
            "Broadcast candidate block. block hash for signing: {:?}",
            block.sighash()
        );
        self.connection_manager.broadcast_message(Message {
            message_type: MessageType::BlockGenerationRoundMessages(
                BlockGenerationRoundMessageType::Candidateblock(block.clone()),
            ),
            sender_id: self.params.signer_id,
            receiver_id: None,
        });

        let (keys, shared_secret_for_positive, shared_secret_for_negative) =
            create_block_vss(block.clone(), &self.params, &self.connection_manager);

        Master::default()
            .candidate_block(Some(block))
            .block_key(Some(keys.u_i))
            .insert_shared_block_secrets(
                self.params.signer_id.clone(),
                shared_secret_for_positive,
                shared_secret_for_negative,
            )
            .build()
    }

    pub fn process_key_generation_message(
        &mut self,
        sender_id: &SignerID,
        message: KeyGenerationMessageType,
    ) {
        match message {
            KeyGenerationMessageType::Nodevss(vss, secret_share) => {
                process_nodevss(&sender_id, vss, secret_share, self);
            }
        }
    }

    pub fn process_round_message(
        &mut self,
        sender_id: &SignerID,
        message: BlockGenerationRoundMessageType,
    ) -> NodeState {
        match message {
            BlockGenerationRoundMessageType::Candidateblock(block) => process_candidateblock(
                &sender_id,
                &block,
                &self.current_state,
                &self.connection_manager,
                &self.params,
            ),
            BlockGenerationRoundMessageType::Completedblock(block) => {
                process_completedblock(&sender_id, &block, &self.current_state, &self.params)
            }
            BlockGenerationRoundMessageType::Blockvss(
                blockhash,
                vss_for_positive,
                secret_share_for_positive,
                vss_for_negative,
                secret_share_for_negative,
            ) => process_blockvss(
                &sender_id,
                blockhash,
                vss_for_positive,
                secret_share_for_positive,
                vss_for_negative,
                secret_share_for_negative,
                &self.current_state,
                &self.priv_shared_keys.as_ref().expect("priv_share_keys should be stored by when the blockvss message communication starts."),
                &self.connection_manager,
                &self.params,
            ),
            BlockGenerationRoundMessageType::Blockparticipants(
                blockhash,
                participants
            ) => process_blockparticipants(
                &sender_id,
                blockhash,
                participants,
                &self.priv_shared_keys.as_ref().expect("priv_share_keys should be stored by when the blockparticipants message communication starts."),
                &self.current_state,
                &self.connection_manager,
                &self.params,
            ),
            BlockGenerationRoundMessageType::Blocksig(blockhash, gamma_i, e) => {
                process_blocksig(
                    &sender_id,
                    blockhash,
                    gamma_i,
                    e,
                    &self.priv_shared_keys.as_ref().expect("priv_share_keys should be stored by when the blocksig message communication starts."),
                    &self.shared_secrets,
                    &self.current_state,
                    &self.connection_manager,
                    &self.params,
                )
            }
            BlockGenerationRoundMessageType::Roundfailure => self.process_roundfailure(&sender_id),
        }
    }

    /// Start next round.
    /// decide master of next round according to Round-robin.
    fn start_next_round(&mut self, next_master_index: usize) {
        self.round_timer.restart().unwrap();

        log::info!(
            "Start next round: self_index={}, master_index={}",
            self.params.self_node_index,
            next_master_index,
        );

        if self.params.self_node_index == next_master_index {
            self.current_state = self.start_new_round();
        } else {
            self.current_state = Member::default().master_index(next_master_index).build();
        }
    }

    fn process_roundfailure(&self, _sender_id: &SignerID) -> NodeState {
        self.current_state.clone()
    }

    fn create_node_share(&mut self) {
        let params = self.params.sharing_params();
        let key = Sign::create_key(
            self.params.self_node_index + 1,
            Sign::private_key_to_big_int(self.params.private_key.key),
        );
        let y_vec: Vec<GE> = self
            .params
            .pubkey_list
            .iter()
            .map(|public_key| {
                let bytes: Vec<u8> = public_key.key.serialize_uncompressed().to_vec();
                GE::from_bytes(&bytes[1..]).unwrap()
            })
            .collect::<Vec<GE>>();
        let _y_sum = sum_point(&y_vec);
        let parties = (0..params.share_count)
            .map(|i| i + 1)
            .collect::<Vec<usize>>();

        let (vss_scheme, secret_shares) = VerifiableSS::share_at_indices(
            params.threshold,
            params.share_count,
            &key.u_i,
            &parties,
        );

        log::info!("Sending VSS to each other signers");

        for i in 0..self.params.pubkey_list.len() {
            self.connection_manager.send_message(Message {
                message_type: MessageType::KeyGenerationMessage(KeyGenerationMessageType::Nodevss(
                    vss_scheme.clone(),
                    secret_shares[i],
                )),
                sender_id: self.params.signer_id,
                receiver_id: Some(SignerID {
                    pubkey: self.params.pubkey_list[i],
                }),
            });
        }
    }
}

pub fn master_index<T>(state: &NodeState, params: &NodeParameters<T>) -> Option<usize>
where
    T: TapyrusApi,
{
    match state {
        NodeState::Master { .. } => Some(params.self_node_index),
        NodeState::Member { master_index, .. } => Some(*master_index),
        NodeState::RoundComplete { master_index, .. } => Some(*master_index),
        _ => None,
    }
}

pub fn next_master_index<T>(state: &NodeState, params: &NodeParameters<T>) -> usize
where
    T: TapyrusApi,
{
    let next = match state {
        NodeState::Joining => 0,
        NodeState::Master { .. } => params.self_node_index + 1,
        NodeState::Member { master_index, .. } => master_index + 1,
        NodeState::RoundComplete {
            next_master_index, ..
        } => *next_master_index,
    };

    next % params.pubkey_list.len()
}

pub fn is_master<T>(sender_id: &SignerID, state: &NodeState, params: &NodeParameters<T>) -> bool
where
    T: TapyrusApi,
{
    match state {
        NodeState::Master { .. } => params.signer_id == *sender_id,
        NodeState::Member { master_index, .. } => {
            let master_id = params.pubkey_list[*master_index];
            master_id == sender_id.pubkey
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use crate::net::{ConnectionManager, ConnectionManagerError, Message, SignerID};
    use crate::rpc::tests::{safety, safety_error, MockRpc};
    use crate::rpc::TapyrusApi;
    use crate::signer_node::{
        master_index, next_master_index, BidirectionalSharedSecretMap, NodeParameters, NodeState,
        SignerNode,
    };
    use crate::tests::helper::blocks::get_block;
    use crate::tests::helper::keys::TEST_KEYS;
    use crate::tests::helper::{address, enable_log};
    use redis::ControlFlow;
    use std::collections::HashSet;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::Arc;
    use std::thread;
    use std::thread::JoinHandle;
    use std::time::Duration;

    pub type SpyMethod = Box<dyn Fn(Arc<Message>) -> () + Send + 'static>;

    /// ConnectionManager for testing.
    pub struct TestConnectionManager {
        /// This is count of messages. TestConnectionManager waits for receiving the number of message.
        pub receive_count: u32,
        /// sender of message
        pub sender: Sender<Message>,
        /// receiver of message
        pub receiver: Receiver<Message>,
        /// A function which is called when the node try to broadcast messages.
        pub broadcast_assert: SpyMethod,
    }

    impl TestConnectionManager {
        pub fn new(receive_count: u32, broadcast_assert: SpyMethod) -> Self {
            let (sender, receiver): (Sender<Message>, Receiver<Message>) = channel();
            TestConnectionManager {
                receive_count,
                sender,
                receiver,
                broadcast_assert,
            }
        }
    }

    impl ConnectionManager for TestConnectionManager {
        type ERROR = crate::errors::Error;
        fn broadcast_message(&self, message: Message) {
            let rc_message = Arc::new(message);
            (self.broadcast_assert)(rc_message.clone());
        }

        fn send_message(&self, message: Message) {
            let rc_message = Arc::new(message);
            (self.broadcast_assert)(rc_message.clone());
        }

        fn start(
            &self,
            mut message_processor: impl FnMut(Message) -> ControlFlow<()> + Send + 'static,
            _id: SignerID,
        ) -> JoinHandle<()> {
            for _count in 0..self.receive_count {
                match self.receiver.recv() {
                    Ok(message) => {
                        log::debug!("Test message receiving!! {:?}", message.message_type);
                        message_processor(message);
                    }
                    Err(e) => log::warn!("happend receiver error: {:?}", e),
                }
            }
            thread::Builder::new()
                .name("TestConnectionManager start Thread".to_string())
                .spawn(|| {
                    thread::sleep(Duration::from_millis(300));
                })
                .unwrap()
        }

        fn error_handler(
            &mut self,
        ) -> Option<Receiver<ConnectionManagerError<crate::errors::Error>>> {
            None::<Receiver<ConnectionManagerError<crate::errors::Error>>>
        }
    }

    fn create_node<T: TapyrusApi>(
        current_state: NodeState,
        rpc: T,
    ) -> SignerNode<T, TestConnectionManager> {
        let closure: SpyMethod = Box::new(move |_message: Arc<Message>| {});
        let (node, _) = create_node_with_closure_and_publish_count(current_state, rpc, closure, 1);
        node
    }

    fn create_node_with_closure_and_publish_count<T: TapyrusApi>(
        current_state: NodeState,
        rpc: T,
        spy: SpyMethod,
        publish_count: u32,
    ) -> (SignerNode<T, TestConnectionManager>, Sender<Message>) {
        let pubkey_list = TEST_KEYS.pubkeys();
        let threshold = 3;
        let private_key = TEST_KEYS.key[0];
        let to_address = address(&private_key);

        let mut params = NodeParameters::new(
            to_address,
            pubkey_list,
            private_key,
            threshold,
            rpc,
            0,
            true,
        );
        params.round_duration = 0;
        let con = TestConnectionManager::new(publish_count, spy);
        let broadcaster = con.sender.clone();
        let mut node = SignerNode::new(con, params);
        node.current_state = current_state;
        (node, broadcaster)
    }

    #[test]
    fn test_timeout_roundrobin() {
        enable_log(None);
        let closure: SpyMethod = Box::new(move |_message: Arc<Message>| {});
        let initial_state = NodeState::Joining;
        let arc_block = safety(get_block(0));
        let rpc = MockRpc {
            return_block: arc_block.clone(),
        };
        let (mut node, _broadcaster) =
            create_node_with_closure_and_publish_count(initial_state, rpc, closure, 0);

        let (stop_signal, stop_handler): (Sender<u32>, Receiver<u32>) = channel();
        node.stop_handler(stop_handler);

        let ss = stop_signal.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(16)); // 16s = 1 round (10s) + idle time(5s) + 1s
            ss.send(1).unwrap();
        });
        node.start();

        assert_eq!(master_index(&node.current_state, &node.params).unwrap(), 1);
    }

    #[test]
    fn test_start_next_round() {
        let arc_block = safety(get_block(0));
        let rpc = MockRpc {
            return_block: arc_block.clone(),
        };
        let mut node = create_node(
            NodeState::Member {
                block_key: None,
                block_shared_keys: None,
                shared_block_secrets: BidirectionalSharedSecretMap::new(),
                candidate_block: None,
                participants: HashSet::new(),
                master_index: 0,
            },
            rpc,
        );

        assert_eq!(master_index(&node.current_state, &node.params).unwrap(), 0);

        node.start_next_round(next_master_index(&node.current_state, &node.params));
        assert_eq!(master_index(&node.current_state, &node.params).unwrap(), 1);

        // When the state is Joining, next round should be started as first round, so that,
        // the master index is 0.
        node.current_state = NodeState::Joining;
        node.start_next_round(next_master_index(&node.current_state, &node.params));
        assert_eq!(master_index(&node.current_state, &node.params).unwrap(), 0);
    }

    mod test_for_waiting_ibd_finish {
        use crate::blockdata::Block;
        use crate::errors::Error;
        use crate::rpc::{GetBlockchainInfoResult, TapyrusApi};
        use crate::signer_node::tests::create_node;
        use crate::signer_node::{BidirectionalSharedSecretMap, NodeState};
        use bitcoin::Address;
        use std::cell::Cell;
        use std::collections::HashSet;

        struct MockRpc {
            pub results: [GetBlockchainInfoResult; 2],
            pub call_count: Cell<usize>,
        }

        impl TapyrusApi for MockRpc {
            fn getnewblock(&self, _address: &Address) -> Result<Block, Error> {
                unimplemented!()
            }
            fn testproposedblock(&self, _block: &Block) -> Result<bool, Error> {
                unimplemented!()
            }

            fn submitblock(&self, _block: &Block) -> Result<(), Error> {
                unimplemented!()
            }

            fn getblockchaininfo(&self) -> Result<GetBlockchainInfoResult, Error> {
                let result = self.results[self.call_count.get()].clone();

                self.call_count.set(self.call_count.get() + 1);

                Ok(result)
            }
        }

        #[test]
        fn test_wait_for_ibd_finish() {
            let json = serde_json::from_str("{\"chain\": \"test\", \"blocks\": 26826, \"headers\": 26826, \"bestblockhash\": \"7303687fb5d80781bd9fece466e76d97a94613d409d127030ff7f34081a899f7\", \"mediantime\": 1568103315, \"verificationprogress\": 1, \"initialblockdownload\": false, \"size_on_disk\": 11669126,  \"pruned\": false,  \"bip9_softforks\": {    \"csv\": {      \"status\": \"failed\",      \"startTime\": 1456790400, \"timeout\": 1493596800, \"since\": 2016 }, \"segwit\": { \"status\": \"failed\", \"startTime\": 1462060800, \"timeout\": 1493596800, \"since\": 2016 }},  \"warnings\": \"\"}").unwrap();
            let mut result1 = serde_json::from_value::<GetBlockchainInfoResult>(json).unwrap();
            result1.initialblockdownload = true;
            let mut result2 = result1.clone();
            result2.initialblockdownload = false;

            let rpc = MockRpc {
                results: [result1, result2],
                call_count: Cell::new(0),
            };

            let node = create_node(
                NodeState::Member {
                    block_key: None,
                    block_shared_keys: None,
                    shared_block_secrets: BidirectionalSharedSecretMap::new(),
                    candidate_block: None,
                    participants: HashSet::new(),
                    master_index: 0,
                },
                rpc,
            );

            node.wait_for_ibd_finish(std::time::Duration::from_millis(1));

            let rpc = node.params.rpc.clone();
            assert_eq!(rpc.call_count.get(), 2);
        }
    }
}
