// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use super::*;
use futures::{channel::oneshot, executor};
use polkadot_node_network_protocol::{self as net_protocol, OurView};
use polkadot_node_subsystem::messages::NetworkBridgeEvent;

use assert_matches::assert_matches;
use async_trait::async_trait;
use parking_lot::Mutex;
use polkadot_overseer::TimeoutExt;
use std::{
	collections::HashSet,
	sync::atomic::{AtomicBool, Ordering},
	time::Duration,
};

use sc_network::{
	service::traits::{Direction, MessageSink, NotificationService},
	IfDisconnected, Multiaddr, ObservedRole as SubstrateObservedRole, ProtocolName,
	ReputationChange, Roles,
};

use polkadot_node_network_protocol::{
	peer_set::PeerSetProtocolNames,
	request_response::{outgoing::Requests, ReqProtocolNames},
	view, CollationProtocols, ObservedRole, ValidationProtocols,
};
use polkadot_node_subsystem::{
	messages::{
		AllMessages, ApprovalVotingParallelMessage, BitfieldDistributionMessage,
		GossipSupportMessage, StatementDistributionMessage,
	},
	ActiveLeavesUpdate, FromOrchestra, OverseerSignal,
};
use polkadot_node_subsystem_test_helpers::{
	mock::new_leaf, SingleItemSink, SingleItemStream, TestSubsystemContextHandle,
};
use polkadot_node_subsystem_util::metered;
use polkadot_primitives::{AuthorityDiscoveryId, Hash};

use sp_keyring::Sr25519Keyring;

use crate::{network::Network, validator_discovery::AuthorityDiscovery};

#[derive(Debug, PartialEq)]
pub enum NetworkAction {
	/// Note a change in reputation for a peer.
	ReputationChange(PeerId, ReputationChange),
	/// Disconnect a peer from the given peer-set.
	DisconnectPeer(PeerId, PeerSet),
	/// Write a notification to a given peer on the given peer-set.
	WriteNotification(PeerId, PeerSet, Vec<u8>),
}

// The subsystem's view of the network.
#[derive(Clone)]
struct TestNetwork {
	action_tx: Arc<Mutex<metered::UnboundedMeteredSender<NetworkAction>>>,
	protocol_names: Arc<PeerSetProtocolNames>,
}

#[derive(Clone, Debug)]
struct TestAuthorityDiscovery;

// The test's view of the network. This receives updates from the subsystem in the form
// of `NetworkAction`s.
struct TestNetworkHandle {
	action_rx: metered::UnboundedMeteredReceiver<NetworkAction>,
	validation_tx: SingleItemSink<NotificationEvent>,
	collation_tx: SingleItemSink<NotificationEvent>,
}

fn new_test_network(
	protocol_names: PeerSetProtocolNames,
) -> (
	TestNetwork,
	TestNetworkHandle,
	TestAuthorityDiscovery,
	Box<dyn NotificationService>,
	Box<dyn NotificationService>,
) {
	let (action_tx, action_rx) = metered::unbounded();
	let (validation_tx, validation_rx) = polkadot_node_subsystem_test_helpers::single_item_sink();
	let (collation_tx, collation_rx) = polkadot_node_subsystem_test_helpers::single_item_sink();
	let action_tx = Arc::new(Mutex::new(action_tx));

	(
		TestNetwork {
			action_tx: action_tx.clone(),
			protocol_names: Arc::new(protocol_names.clone()),
		},
		TestNetworkHandle { action_rx, validation_tx, collation_tx },
		TestAuthorityDiscovery,
		Box::new(TestNotificationService::new(
			PeerSet::Validation,
			action_tx.clone(),
			validation_rx,
		)),
		Box::new(TestNotificationService::new(PeerSet::Collation, action_tx, collation_rx)),
	)
}

#[async_trait]
impl Network for TestNetwork {
	async fn set_reserved_peers(
		&mut self,
		_protocol: ProtocolName,
		_: HashSet<Multiaddr>,
	) -> Result<(), String> {
		Ok(())
	}

	async fn add_peers_to_reserved_set(
		&mut self,
		_protocol: ProtocolName,
		_: HashSet<Multiaddr>,
	) -> Result<(), String> {
		Ok(())
	}

	async fn remove_from_peers_set(
		&mut self,
		_protocol: ProtocolName,
		_: Vec<PeerId>,
	) -> Result<(), String> {
		Ok(())
	}

	async fn start_request<AD: AuthorityDiscovery>(
		&self,
		_: &mut AD,
		_: Requests,
		_: &ReqProtocolNames,
		_: IfDisconnected,
	) {
	}

	fn report_peer(&self, who: PeerId, rep: ReputationChange) {
		self.action_tx
			.lock()
			.unbounded_send(NetworkAction::ReputationChange(who, rep))
			.unwrap();
	}

	fn disconnect_peer(&self, who: PeerId, protocol: ProtocolName) {
		let (peer_set, version) = self.protocol_names.try_get_protocol(&protocol).unwrap();
		assert_eq!(version, peer_set.get_main_version());

		self.action_tx
			.lock()
			.unbounded_send(NetworkAction::DisconnectPeer(who, peer_set))
			.unwrap();
	}

	fn peer_role(&self, _peer_id: PeerId, handshake: Vec<u8>) -> Option<SubstrateObservedRole> {
		Roles::decode_all(&mut &handshake[..])
			.ok()
			.and_then(|role| Some(SubstrateObservedRole::from(role)))
	}
}

#[async_trait]
impl validator_discovery::AuthorityDiscovery for TestAuthorityDiscovery {
	async fn get_addresses_by_authority_id(
		&mut self,
		_authority: AuthorityDiscoveryId,
	) -> Option<HashSet<Multiaddr>> {
		None
	}

	async fn get_authority_ids_by_peer_id(
		&mut self,
		_peer_id: PeerId,
	) -> Option<HashSet<AuthorityDiscoveryId>> {
		None
	}
}

impl TestNetworkHandle {
	// Get the next network action.
	async fn next_network_action(&mut self) -> NetworkAction {
		self.action_rx.next().await.expect("subsystem concluded early")
	}

	// Wait for the next N network actions.
	async fn next_network_actions(&mut self, n: usize) -> Vec<NetworkAction> {
		let mut v = Vec::with_capacity(n);
		for _ in 0..n {
			v.push(self.next_network_action().await);
		}

		v
	}

	async fn connect_peer(
		&mut self,
		peer: PeerId,
		protocol_version: ProtocolVersion,
		peer_set: PeerSet,
		role: ObservedRole,
	) {
		fn observed_role_to_handshake(role: &ObservedRole) -> Vec<u8> {
			match role {
				&ObservedRole::Light => Roles::LIGHT.encode(),
				&ObservedRole::Authority => Roles::AUTHORITY.encode(),
				&ObservedRole::Full => Roles::FULL.encode(),
			}
		}

		// because of how protocol negotiation works, if two peers support at least one common
		// protocol, the protocol is negotiated over the main protocol (`ValidationVersion::V3`) but
		// if either one of the peers used a fallback protocol for the negotiation (meaning they
		// don't support the main protocol but some older version of it ), `negotiated_fallback` is
		// set to that protocol.
		let negotiated_fallback = match (protocol_version.into(), peer_set) {
			(1, PeerSet::Collation) => Some(ProtocolName::from("/polkadot/collation/1")),
			(2, PeerSet::Collation) => None,
			(3, PeerSet::Validation) => None,
			_ => unreachable!(),
		};

		match peer_set {
			PeerSet::Validation => {
				self.validation_tx
					.send(NotificationEvent::NotificationStreamOpened {
						peer,
						direction: Direction::Inbound,
						handshake: observed_role_to_handshake(&role),
						negotiated_fallback,
					})
					.await
					.expect("subsystem concluded early");
			},
			PeerSet::Collation => {
				self.collation_tx
					.send(NotificationEvent::NotificationStreamOpened {
						peer,
						direction: Direction::Inbound,
						handshake: observed_role_to_handshake(&role),
						negotiated_fallback,
					})
					.await
					.expect("subsystem concluded early");
			},
		}
	}

	async fn disconnect_peer(&mut self, peer: PeerId, peer_set: PeerSet) {
		match peer_set {
			PeerSet::Validation => self
				.validation_tx
				.send(NotificationEvent::NotificationStreamClosed { peer })
				.await
				.expect("subsystem concluded early"),
			PeerSet::Collation => self
				.collation_tx
				.send(NotificationEvent::NotificationStreamClosed { peer })
				.await
				.expect("subsystem concluded early"),
		}
	}

	async fn peer_message(&mut self, peer: PeerId, peer_set: PeerSet, message: Vec<u8>) {
		match peer_set {
			PeerSet::Validation => self
				.validation_tx
				.send(NotificationEvent::NotificationReceived { peer, notification: message })
				.await
				.expect("subsystem concluded early"),
			PeerSet::Collation => self
				.collation_tx
				.send(NotificationEvent::NotificationReceived { peer, notification: message })
				.await
				.expect("subsystem concluded early"),
		}
	}
}

/// Assert that the given actions contain the given `action`.
fn assert_network_actions_contains(actions: &[NetworkAction], action: &NetworkAction) {
	if !actions.iter().any(|x| x == action) {
		panic!("Could not find `{:?}` in `{:?}`", action, actions);
	}
}

struct TestNotificationService {
	peer_set: PeerSet,
	action_tx: Arc<Mutex<metered::UnboundedMeteredSender<NetworkAction>>>,
	rx: SingleItemStream<NotificationEvent>,
}

impl std::fmt::Debug for TestNotificationService {
	fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		Ok(())
	}
}

impl TestNotificationService {
	pub fn new(
		peer_set: PeerSet,
		action_tx: Arc<Mutex<metered::UnboundedMeteredSender<NetworkAction>>>,
		rx: SingleItemStream<NotificationEvent>,
	) -> Self {
		Self { peer_set, action_tx, rx }
	}
}

struct TestMessageSink {
	peer: PeerId,
	peer_set: PeerSet,
	action_tx: Arc<Mutex<metered::UnboundedMeteredSender<NetworkAction>>>,
}

impl TestMessageSink {
	fn new(
		peer: PeerId,
		peer_set: PeerSet,
		action_tx: Arc<Mutex<metered::UnboundedMeteredSender<NetworkAction>>>,
	) -> TestMessageSink {
		Self { peer, peer_set, action_tx }
	}
}

#[async_trait::async_trait]
impl MessageSink for TestMessageSink {
	fn send_sync_notification(&self, notification: Vec<u8>) {
		self.action_tx
			.lock()
			.unbounded_send(NetworkAction::WriteNotification(
				self.peer,
				self.peer_set,
				notification,
			))
			.unwrap();
	}

	async fn send_async_notification(
		&self,
		_notification: Vec<u8>,
	) -> Result<(), sc_network::error::Error> {
		unimplemented!();
	}
}

#[async_trait::async_trait]
impl NotificationService for TestNotificationService {
	/// Instruct `Notifications` to open a new substream for `peer`.
	async fn open_substream(&mut self, _peer: PeerId) -> Result<(), ()> {
		unimplemented!();
	}

	/// Instruct `Notifications` to close substream for `peer`.
	async fn close_substream(&mut self, _peer: PeerId) -> Result<(), ()> {
		unimplemented!();
	}

	/// Send synchronous `notification` to `peer`.
	fn send_sync_notification(&mut self, _peer: &PeerId, _notification: Vec<u8>) {
		unimplemented!();
	}

	/// Send asynchronous `notification` to `peer`, allowing sender to exercise backpressure.
	async fn send_async_notification(
		&mut self,
		_peer: &PeerId,
		_notification: Vec<u8>,
	) -> Result<(), sc_network::error::Error> {
		unimplemented!();
	}

	/// Set handshake for the notification protocol replacing the old handshake.
	async fn set_handshake(&mut self, _handshake: Vec<u8>) -> Result<(), ()> {
		unimplemented!();
	}

	fn try_set_handshake(&mut self, _handshake: Vec<u8>) -> Result<(), ()> {
		unimplemented!();
	}

	/// Get next event from the `Notifications` event stream.
	async fn next_event(&mut self) -> Option<NotificationEvent> {
		self.rx.next().await
	}

	// Clone [`NotificationService`]
	fn clone(&mut self) -> Result<Box<dyn NotificationService>, ()> {
		unimplemented!();
	}

	/// Get protocol name.
	fn protocol(&self) -> &ProtocolName {
		unimplemented!();
	}

	/// Get notification sink of the peer.
	fn message_sink(&self, peer: &PeerId) -> Option<Box<dyn MessageSink>> {
		Some(Box::new(TestMessageSink::new(*peer, self.peer_set, self.action_tx.clone())))
	}
}

#[derive(Clone)]
struct TestSyncOracle {
	is_major_syncing: Arc<AtomicBool>,
	done_syncing_sender: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

struct TestSyncOracleHandle {
	done_syncing_receiver: oneshot::Receiver<()>,
	is_major_syncing: Arc<AtomicBool>,
}

impl TestSyncOracleHandle {
	fn set_done(&self) {
		self.is_major_syncing.store(false, Ordering::SeqCst);
	}

	async fn await_mode_switch(self) {
		let _ = self.done_syncing_receiver.await;
	}
}

impl SyncOracle for TestSyncOracle {
	fn is_major_syncing(&self) -> bool {
		let is_major_syncing = self.is_major_syncing.load(Ordering::SeqCst);

		if !is_major_syncing {
			if let Some(sender) = self.done_syncing_sender.lock().take() {
				let _ = sender.send(());
			}
		}

		is_major_syncing
	}

	fn is_offline(&self) -> bool {
		unimplemented!("not used in network bridge")
	}
}

// val - result of `is_major_syncing`.
fn make_sync_oracle(is_major_syncing: bool) -> (TestSyncOracle, TestSyncOracleHandle) {
	let (tx, rx) = oneshot::channel();
	let is_major_syncing = Arc::new(AtomicBool::new(is_major_syncing));

	(
		TestSyncOracle {
			is_major_syncing: is_major_syncing.clone(),
			done_syncing_sender: Arc::new(Mutex::new(Some(tx))),
		},
		TestSyncOracleHandle { is_major_syncing, done_syncing_receiver: rx },
	)
}

fn done_syncing_oracle() -> Box<dyn SyncOracle + Send> {
	let (oracle, _) = make_sync_oracle(false);
	Box::new(oracle)
}

type VirtualOverseer = TestSubsystemContextHandle<NetworkBridgeRxMessage>;

struct TestHarness {
	network_handle: TestNetworkHandle,
	virtual_overseer: VirtualOverseer,
	shared: Shared,
}

// wait until all needed validation and collation peers have connected.
async fn await_peer_connections(
	shared: &Shared,
	num_validation_peers: usize,
	num_collation_peers: usize,
) {
	loop {
		{
			let shared = shared.0.lock();
			if shared.validation_peers.len() == num_validation_peers &&
				shared.collation_peers.len() == num_collation_peers
			{
				break
			}
		}

		futures_timer::Delay::new(std::time::Duration::from_millis(100)).await;
	}
}

fn test_harness<T: Future<Output = VirtualOverseer>>(
	sync_oracle: Box<dyn SyncOracle + Send>,
	test: impl FnOnce(TestHarness) -> T,
) {
	let genesis_hash = Hash::repeat_byte(0xff);
	let fork_id = None;
	let peerset_protocol_names = PeerSetProtocolNames::new(genesis_hash, fork_id);

	let pool = sp_core::testing::TaskExecutor::new();
	let (network, network_handle, discovery, validation_service, collation_service) =
		new_test_network(peerset_protocol_names.clone());
	let (context, virtual_overseer) =
		polkadot_node_subsystem_test_helpers::make_subsystem_context(pool);
	let notification_sinks = Arc::new(Mutex::new(HashMap::new()));
	let shared = Shared::default();

	let bridge = NetworkBridgeRx {
		network_service: network,
		authority_discovery_service: discovery,
		metrics: Metrics(None),
		sync_oracle,
		shared: shared.clone(),
		peerset_protocol_names,
		validation_service,
		collation_service,
		notification_sinks,
	};

	let network_bridge = run_network_in(bridge, context)
		.map_err(|_| panic!("subsystem execution failed"))
		.map(|_| ());

	let test_fut = test(TestHarness { network_handle, virtual_overseer, shared });

	futures::pin_mut!(test_fut);
	futures::pin_mut!(network_bridge);

	let _ = executor::block_on(future::join(
		async move {
			let mut virtual_overseer = test_fut.await;
			virtual_overseer.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
		},
		network_bridge,
	));
}

async fn assert_sends_validation_event_to_all(
	event: NetworkBridgeEvent<net_protocol::VersionedValidationProtocol>,
	virtual_overseer: &mut TestSubsystemContextHandle<NetworkBridgeRxMessage>,
) {
	// Ordering must be consistent across:
	// `fn dispatch_validation_event_to_all_unbounded`
	// `dispatch_validation_events_to_all`
	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::StatementDistribution(
			StatementDistributionMessage::NetworkBridgeUpdate(e)
		) if e == event.focus().expect("could not focus message")
	);

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::BitfieldDistribution(
			BitfieldDistributionMessage::NetworkBridgeUpdate(e)
		) if e == event.focus().expect("could not focus message")
	);

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::ApprovalVotingParallel(
			ApprovalVotingParallelMessage::NetworkBridgeUpdate(e)
		) if e == event.focus().expect("could not focus message")
	);

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::GossipSupport(
			GossipSupportMessage::NetworkBridgeUpdate(e)
		) if e == event.focus().expect("could not focus message")
	);
}

async fn assert_sends_collation_event_to_all(
	event: NetworkBridgeEvent<net_protocol::VersionedCollationProtocol>,
	virtual_overseer: &mut TestSubsystemContextHandle<NetworkBridgeRxMessage>,
) {
	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::CollatorProtocol(
			CollatorProtocolMessage::NetworkBridgeUpdate(e)
		) if e == event.focus().expect("could not focus message")
	)
}

#[test]
fn send_our_view_upon_connection() {
	let (oracle, handle) = make_sync_oracle(false);
	test_harness(Box::new(oracle), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer = PeerId::random();

		let head = Hash::repeat_byte(1);
		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::start_work(new_leaf(head, 1)),
			)))
			.await;

		handle.await_mode_switch().await;

		network_handle
			.connect_peer(
				peer,
				ValidationVersion::V3.into(),
				PeerSet::Validation,
				ObservedRole::Full,
			)
			.await;
		network_handle
			.connect_peer(peer, CollationVersion::V1.into(), PeerSet::Collation, ObservedRole::Full)
			.await;

		await_peer_connections(&shared, 1, 1).await;

		let view = view![head];
		let actions = network_handle.next_network_actions(2).await;
		assert_network_actions_contains(
			&actions,
			&NetworkAction::WriteNotification(
				peer,
				PeerSet::Validation,
				WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(view.clone()).encode(),
			),
		);
		assert_network_actions_contains(
			&actions,
			&NetworkAction::WriteNotification(
				peer,
				PeerSet::Collation,
				WireMessage::<protocol_v1::CollationProtocol>::ViewUpdate(view.clone()).encode(),
			),
		);
		virtual_overseer
	});
}

#[test]
fn sends_view_updates_to_peers() {
	let (oracle, handle) = make_sync_oracle(false);
	test_harness(Box::new(oracle), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer_a = PeerId::random();
		let peer_b = PeerId::random();

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
				activated: Default::default(),
				deactivated: Default::default(),
			})))
			.await;

		handle.await_mode_switch().await;

		network_handle
			.connect_peer(
				peer_a,
				ValidationVersion::V3.into(),
				PeerSet::Validation,
				ObservedRole::Full,
			)
			.await;
		network_handle
			.connect_peer(
				peer_b,
				CollationVersion::V1.into(),
				PeerSet::Collation,
				ObservedRole::Full,
			)
			.await;

		await_peer_connections(&shared, 1, 1).await;

		let actions = network_handle.next_network_actions(2).await;
		let wire_message =
			WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(View::default()).encode();

		assert_network_actions_contains(
			&actions,
			&NetworkAction::WriteNotification(peer_a, PeerSet::Validation, wire_message.clone()),
		);

		assert_network_actions_contains(
			&actions,
			&NetworkAction::WriteNotification(peer_b, PeerSet::Collation, wire_message.clone()),
		);

		let hash_a = Hash::repeat_byte(1);

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::start_work(new_leaf(hash_a, 1)),
			)))
			.await;

		let actions = network_handle.next_network_actions(2).await;
		let wire_message =
			WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(view![hash_a]).encode();

		assert_network_actions_contains(
			&actions,
			&NetworkAction::WriteNotification(peer_a, PeerSet::Validation, wire_message.clone()),
		);

		assert_network_actions_contains(
			&actions,
			&NetworkAction::WriteNotification(peer_b, PeerSet::Collation, wire_message.clone()),
		);
		virtual_overseer
	});
}

#[test]
fn do_not_send_view_update_until_synced() {
	let (oracle, handle) = make_sync_oracle(true);
	test_harness(Box::new(oracle), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer_a = PeerId::random();
		let peer_b = PeerId::random();
		assert_ne!(peer_a, peer_b);

		network_handle
			.connect_peer(
				peer_a,
				ValidationVersion::V3.into(),
				PeerSet::Validation,
				ObservedRole::Full,
			)
			.await;
		network_handle
			.connect_peer(
				peer_b,
				CollationVersion::V1.into(),
				PeerSet::Collation,
				ObservedRole::Full,
			)
			.await;

		await_peer_connections(&shared, 1, 1).await;

		{
			let actions = network_handle.next_network_actions(2).await;
			let wire_message =
				WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(View::default())
					.encode();

			assert_network_actions_contains(
				&actions,
				&NetworkAction::WriteNotification(
					peer_a,
					PeerSet::Validation,
					wire_message.clone(),
				),
			);

			assert_network_actions_contains(
				&actions,
				&NetworkAction::WriteNotification(peer_b, PeerSet::Collation, wire_message.clone()),
			);
		}

		let hash_a = Hash::repeat_byte(1);
		let hash_b = Hash::repeat_byte(1);

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::start_work(new_leaf(hash_a, 1)),
			)))
			.await;

		// delay until the previous update has certainly been processed.
		futures_timer::Delay::new(std::time::Duration::from_millis(100)).await;

		handle.set_done();

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::start_work(new_leaf(hash_b, 1)),
			)))
			.await;

		handle.await_mode_switch().await;

		// There should be a mode switch only for the second view update.
		{
			let actions = network_handle.next_network_actions(2).await;
			let wire_message =
				WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(view![hash_a, hash_b])
					.encode();

			assert_network_actions_contains(
				&actions,
				&NetworkAction::WriteNotification(
					peer_a,
					PeerSet::Validation,
					wire_message.clone(),
				),
			);

			assert_network_actions_contains(
				&actions,
				&NetworkAction::WriteNotification(peer_b, PeerSet::Collation, wire_message.clone()),
			);
		}
		virtual_overseer
	});
}

#[test]
fn do_not_send_view_update_when_only_finalized_block_changed() {
	test_harness(done_syncing_oracle(), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer_a = PeerId::random();
		let peer_b = PeerId::random();

		network_handle
			.connect_peer(
				peer_a,
				ValidationVersion::V3.into(),
				PeerSet::Validation,
				ObservedRole::Full,
			)
			.await;
		network_handle
			.connect_peer(
				peer_b,
				ValidationVersion::V3.into(),
				PeerSet::Validation,
				ObservedRole::Full,
			)
			.await;

		await_peer_connections(&shared, 2, 0).await;

		let hash_a = Hash::repeat_byte(1);

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::BlockFinalized(Hash::random(), 5)))
			.await;

		// Send some empty active leaves update
		//
		// This should not trigger a view update to our peers.
		virtual_overseer
			.send(FromOrchestra::Signal(
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::default()),
			))
			.await;

		// This should trigger the view update to our peers.
		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::start_work(new_leaf(hash_a, 1)),
			)))
			.await;

		let actions = network_handle.next_network_actions(4).await;
		let wire_message =
			WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(View::new(vec![hash_a], 5))
				.encode();

		assert_network_actions_contains(
			&actions,
			&NetworkAction::WriteNotification(peer_a, PeerSet::Validation, wire_message.clone()),
		);

		assert_network_actions_contains(
			&actions,
			&NetworkAction::WriteNotification(peer_b, PeerSet::Validation, wire_message.clone()),
		);
		virtual_overseer
	});
}

#[test]
fn peer_view_updates_sent_via_overseer() {
	test_harness(done_syncing_oracle(), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer = PeerId::random();

		network_handle
			.connect_peer(
				peer,
				ValidationVersion::V3.into(),
				PeerSet::Validation,
				ObservedRole::Full,
			)
			.await;

		await_peer_connections(&shared, 1, 0).await;

		let view = view![Hash::repeat_byte(1)];

		// bridge will inform about all connected peers.
		{
			assert_sends_validation_event_to_all(
				NetworkBridgeEvent::PeerConnected(
					peer,
					ObservedRole::Full,
					ValidationVersion::V3.into(),
					None,
				),
				&mut virtual_overseer,
			)
			.await;

			assert_sends_validation_event_to_all(
				NetworkBridgeEvent::PeerViewChange(peer, View::default()),
				&mut virtual_overseer,
			)
			.await;

			assert_eq!(virtual_overseer.message_counter.with_high_priority(), 8);
		}

		network_handle
			.peer_message(
				peer,
				PeerSet::Validation,
				WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(view.clone()).encode(),
			)
			.await;

		assert_sends_validation_event_to_all(
			NetworkBridgeEvent::PeerViewChange(peer, view),
			&mut virtual_overseer,
		)
		.await;
		assert_eq!(virtual_overseer.message_counter.with_high_priority(), 12);
		virtual_overseer
	});
}

#[test]
fn peer_messages_sent_via_overseer() {
	test_harness(done_syncing_oracle(), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer = PeerId::random();

		network_handle
			.connect_peer(
				peer,
				ValidationVersion::V3.into(),
				PeerSet::Validation,
				ObservedRole::Full,
			)
			.await;

		await_peer_connections(&shared, 1, 0).await;

		// bridge will inform about all connected peers.
		{
			assert_sends_validation_event_to_all(
				NetworkBridgeEvent::PeerConnected(
					peer,
					ObservedRole::Full,
					ValidationVersion::V3.into(),
					None,
				),
				&mut virtual_overseer,
			)
			.await;

			assert_sends_validation_event_to_all(
				NetworkBridgeEvent::PeerViewChange(peer, View::default()),
				&mut virtual_overseer,
			)
			.await;

			assert_eq!(virtual_overseer.message_counter.with_high_priority(), 8);
		}

		let approval_distribution_message =
			protocol_v3::ApprovalDistributionMessage::Approvals(Vec::new());

		let message_v1 = protocol_v3::ValidationProtocol::ApprovalDistribution(
			approval_distribution_message.clone(),
		);

		network_handle
			.peer_message(
				peer,
				PeerSet::Validation,
				WireMessage::ProtocolMessage(message_v1.clone()).encode(),
			)
			.await;

		network_handle.disconnect_peer(peer, PeerSet::Validation).await;

		// Approval distribution message comes first, and the message is only sent to that
		// subsystem. then a disconnection event arises that is sent to all validation networking
		// subsystems.

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ApprovalVotingParallel(
				ApprovalVotingParallelMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::PeerMessage(p, ValidationProtocols::V3(m))
				)
			) => {
				assert_eq!(p, peer);
				assert_eq!(m, approval_distribution_message);
			}
		);

		assert_sends_validation_event_to_all(
			NetworkBridgeEvent::PeerDisconnected(peer),
			&mut virtual_overseer,
		)
		.await;
		assert_eq!(virtual_overseer.message_counter.with_high_priority(), 12);
		virtual_overseer
	});
}

#[test]
fn peer_disconnect_from_just_one_peerset() {
	test_harness(done_syncing_oracle(), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer = PeerId::random();

		network_handle
			.connect_peer(
				peer,
				ValidationVersion::V3.into(),
				PeerSet::Validation,
				ObservedRole::Full,
			)
			.await;
		network_handle
			.connect_peer(peer, CollationVersion::V1.into(), PeerSet::Collation, ObservedRole::Full)
			.await;

		await_peer_connections(&shared, 1, 1).await;

		// bridge will inform about all connected peers.
		{
			assert_sends_validation_event_to_all(
				NetworkBridgeEvent::PeerConnected(
					peer,
					ObservedRole::Full,
					ValidationVersion::V3.into(),
					None,
				),
				&mut virtual_overseer,
			)
			.await;

			assert_sends_validation_event_to_all(
				NetworkBridgeEvent::PeerViewChange(peer, View::default()),
				&mut virtual_overseer,
			)
			.await;

			assert_eq!(virtual_overseer.message_counter.with_high_priority(), 8);
		}

		{
			assert_sends_collation_event_to_all(
				NetworkBridgeEvent::PeerConnected(
					peer,
					ObservedRole::Full,
					CollationVersion::V1.into(),
					None,
				),
				&mut virtual_overseer,
			)
			.await;

			assert_sends_collation_event_to_all(
				NetworkBridgeEvent::PeerViewChange(peer, View::default()),
				&mut virtual_overseer,
			)
			.await;
		}

		network_handle.disconnect_peer(peer, PeerSet::Validation).await;

		assert_sends_validation_event_to_all(
			NetworkBridgeEvent::PeerDisconnected(peer),
			&mut virtual_overseer,
		)
		.await;
		assert_eq!(virtual_overseer.message_counter.with_high_priority(), 12);

		// to show that we're still connected on the collation protocol, send a view update.

		let hash_a = Hash::repeat_byte(1);

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::start_work(new_leaf(hash_a, 1)),
			)))
			.await;

		let actions = network_handle.next_network_actions(3).await;
		let wire_message =
			WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(view![hash_a]).encode();

		assert_network_actions_contains(
			&actions,
			&NetworkAction::WriteNotification(peer, PeerSet::Collation, wire_message.clone()),
		);
		virtual_overseer
	});
}

#[test]
fn relays_collation_protocol_messages() {
	test_harness(done_syncing_oracle(), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer_a = PeerId::random();
		let peer_b = PeerId::random();

		network_handle
			.connect_peer(
				peer_a,
				ValidationVersion::V3.into(),
				PeerSet::Validation,
				ObservedRole::Full,
			)
			.await;
		network_handle
			.connect_peer(
				peer_b,
				CollationVersion::V1.into(),
				PeerSet::Collation,
				ObservedRole::Full,
			)
			.await;

		await_peer_connections(&shared, 1, 1).await;

		// bridge will inform about all connected peers.
		{
			assert_sends_validation_event_to_all(
				NetworkBridgeEvent::PeerConnected(
					peer_a,
					ObservedRole::Full,
					ValidationVersion::V3.into(),
					None,
				),
				&mut virtual_overseer,
			)
			.await;

			assert_sends_validation_event_to_all(
				NetworkBridgeEvent::PeerViewChange(peer_a, View::default()),
				&mut virtual_overseer,
			)
			.await;

			assert_eq!(virtual_overseer.message_counter.with_high_priority(), 8);
		}

		{
			assert_sends_collation_event_to_all(
				NetworkBridgeEvent::PeerConnected(
					peer_b,
					ObservedRole::Full,
					CollationVersion::V1.into(),
					None,
				),
				&mut virtual_overseer,
			)
			.await;

			assert_sends_collation_event_to_all(
				NetworkBridgeEvent::PeerViewChange(peer_b, View::default()),
				&mut virtual_overseer,
			)
			.await;
		}

		let collator_protocol_message = protocol_v1::CollatorProtocolMessage::Declare(
			Sr25519Keyring::Alice.public().into(),
			Default::default(),
			sp_core::crypto::UncheckedFrom::unchecked_from([1u8; 64]),
		);

		let message_v1 =
			protocol_v1::CollationProtocol::CollatorProtocol(collator_protocol_message.clone());

		// peer A gets reported for sending a collation message.
		// NOTE: this is not possible since peer A cannot send
		// a collation message if it has not opened a collation protocol

		// network_handle
		// 	.peer_message(
		// 		peer_a,
		// 		PeerSet::Collation,
		// 		WireMessage::ProtocolMessage(message_v1.clone()).encode(),
		// 	)
		// 	.await;

		// let actions = network_handle.next_network_actions(3).await;
		// assert_network_actions_contains(
		// 	&actions,
		// 	&NetworkAction::ReputationChange(peer_a, UNCONNECTED_PEERSET_COST.into()),
		// );

		// peer B has the message relayed.

		network_handle
			.peer_message(
				peer_b,
				PeerSet::Collation,
				WireMessage::ProtocolMessage(message_v1.clone()).encode(),
			)
			.await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(
				CollatorProtocolMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::PeerMessage(p, CollationProtocols::V1(m))
				)
			) => {
				assert_eq!(p, peer_b);
				assert_eq!(m, collator_protocol_message);
			}
		);
		virtual_overseer
	});
}

#[test]
fn different_views_on_different_peer_sets() {
	test_harness(done_syncing_oracle(), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer = PeerId::random();

		network_handle
			.connect_peer(
				peer,
				ValidationVersion::V3.into(),
				PeerSet::Validation,
				ObservedRole::Full,
			)
			.await;
		network_handle
			.connect_peer(peer, CollationVersion::V1.into(), PeerSet::Collation, ObservedRole::Full)
			.await;

		await_peer_connections(&shared, 1, 1).await;

		// bridge will inform about all connected peers.
		{
			assert_sends_validation_event_to_all(
				NetworkBridgeEvent::PeerConnected(
					peer,
					ObservedRole::Full,
					ValidationVersion::V3.into(),
					None,
				),
				&mut virtual_overseer,
			)
			.await;

			assert_sends_validation_event_to_all(
				NetworkBridgeEvent::PeerViewChange(peer, View::default()),
				&mut virtual_overseer,
			)
			.await;

			assert_eq!(virtual_overseer.message_counter.with_high_priority(), 8);
		}

		{
			assert_sends_collation_event_to_all(
				NetworkBridgeEvent::PeerConnected(
					peer,
					ObservedRole::Full,
					CollationVersion::V1.into(),
					None,
				),
				&mut virtual_overseer,
			)
			.await;

			assert_sends_collation_event_to_all(
				NetworkBridgeEvent::PeerViewChange(peer, View::default()),
				&mut virtual_overseer,
			)
			.await;
		}

		let view_a = view![Hash::repeat_byte(1)];
		let view_b = view![Hash::repeat_byte(2)];

		network_handle
			.peer_message(
				peer,
				PeerSet::Validation,
				WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(view_a.clone()).encode(),
			)
			.await;

		network_handle
			.peer_message(
				peer,
				PeerSet::Collation,
				WireMessage::<protocol_v1::CollationProtocol>::ViewUpdate(view_b.clone()).encode(),
			)
			.await;

		assert_sends_validation_event_to_all(
			NetworkBridgeEvent::PeerViewChange(peer, view_a.clone()),
			&mut virtual_overseer,
		)
		.await;

		assert_eq!(virtual_overseer.message_counter.with_high_priority(), 12);

		assert_sends_collation_event_to_all(
			NetworkBridgeEvent::PeerViewChange(peer, view_b.clone()),
			&mut virtual_overseer,
		)
		.await;
		virtual_overseer
	});
}

#[test]
fn sent_views_include_finalized_number_update() {
	test_harness(done_syncing_oracle(), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer_a = PeerId::random();

		network_handle
			.connect_peer(
				peer_a,
				ValidationVersion::V3.into(),
				PeerSet::Validation,
				ObservedRole::Full,
			)
			.await;

		await_peer_connections(&shared, 1, 0).await;

		let hash_a = Hash::repeat_byte(1);
		let hash_b = Hash::repeat_byte(2);

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::BlockFinalized(hash_a, 1)))
			.await;
		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::start_work(new_leaf(hash_b, 1)),
			)))
			.await;

		let actions = network_handle.next_network_actions(2).await;
		let wire_message =
			WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(View::new(vec![hash_b], 1))
				.encode();

		assert_network_actions_contains(
			&actions,
			&NetworkAction::WriteNotification(peer_a, PeerSet::Validation, wire_message.clone()),
		);
		virtual_overseer
	});
}

#[test]
fn view_finalized_number_can_not_go_down() {
	test_harness(done_syncing_oracle(), |test_harness| async move {
		let TestHarness { mut network_handle, virtual_overseer, shared } = test_harness;

		let peer_a = PeerId::random();

		network_handle
			.connect_peer(
				peer_a,
				ValidationVersion::V3.into(),
				PeerSet::Validation,
				ObservedRole::Full,
			)
			.await;

		await_peer_connections(&shared, 1, 0).await;

		network_handle
			.peer_message(
				peer_a,
				PeerSet::Validation,
				WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(View::new(
					vec![Hash::repeat_byte(0x01)],
					1,
				))
				.encode(),
			)
			.await;

		network_handle
			.peer_message(
				peer_a,
				PeerSet::Validation,
				WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(View::new(vec![], 0))
					.encode(),
			)
			.await;

		let actions = network_handle.next_network_actions(2).await;
		assert_network_actions_contains(
			&actions,
			&NetworkAction::ReputationChange(peer_a, MALFORMED_VIEW_COST.into()),
		);
		virtual_overseer
	});
}

#[test]
fn our_view_updates_decreasing_order_and_limited_to_max() {
	test_harness(done_syncing_oracle(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		// to show that we're still connected on the collation protocol, send a view update.

		let hashes = (0..MAX_VIEW_HEADS + 1).map(|i| Hash::repeat_byte(i as u8));

		for (i, hash) in hashes.enumerate().rev() {
			// These are in reverse order, so the subsystem must sort internally to
			// get the correct view.
			virtual_overseer
				.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
					ActiveLeavesUpdate::start_work(new_leaf(hash, i as _)),
				)))
				.await;
		}

		let our_views = (1..=MAX_VIEW_HEADS).rev().map(|start| {
			OurView::new((start..=MAX_VIEW_HEADS).rev().map(|i| Hash::repeat_byte(i as u8)), 0)
		});

		for our_view in our_views {
			assert_sends_validation_event_to_all(
				NetworkBridgeEvent::OurViewChange(our_view.clone()),
				&mut virtual_overseer,
			)
			.await;

			assert_sends_collation_event_to_all(
				NetworkBridgeEvent::OurViewChange(our_view),
				&mut virtual_overseer,
			)
			.await;
		}

		virtual_overseer
	});
}

#[test]
fn network_protocol_versioning_view_update() {
	let (oracle, handle) = make_sync_oracle(false);
	test_harness(Box::new(oracle), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer_ids: Vec<_> = (0..4).map(|_| PeerId::random()).collect();
		let peers = [
			(peer_ids[0], PeerSet::Validation, ValidationVersion::V3.into()),
			(peer_ids[1], PeerSet::Collation, CollationVersion::V1.into()),
			(peer_ids[2], PeerSet::Validation, ValidationVersion::V3.into()),
			(peer_ids[3], PeerSet::Collation, CollationVersion::V2.into()),
		];

		let head = Hash::repeat_byte(1);
		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::start_work(new_leaf(head, 1)),
			)))
			.await;

		handle.await_mode_switch().await;

		let mut total_validation_peers = 0;
		let mut total_collation_peers = 0;

		for &(peer_id, peer_set, version) in &peers {
			network_handle
				.connect_peer(peer_id, version, peer_set, ObservedRole::Full)
				.await;

			match peer_set {
				PeerSet::Validation => total_validation_peers += 1,
				PeerSet::Collation => total_collation_peers += 1,
			}
		}

		await_peer_connections(&shared, total_validation_peers, total_collation_peers).await;

		let view = view![head];
		let actions = network_handle.next_network_actions(4).await;

		for &(peer_id, peer_set, version) in &peers {
			let wire_msg = match (version.into(), peer_set) {
				(1, PeerSet::Collation) =>
					WireMessage::<protocol_v1::CollationProtocol>::ViewUpdate(view.clone()).encode(),
				(2, PeerSet::Collation) =>
					WireMessage::<protocol_v2::CollationProtocol>::ViewUpdate(view.clone()).encode(),
				(3, PeerSet::Validation) =>
					WireMessage::<protocol_v3::ValidationProtocol>::ViewUpdate(view.clone())
						.encode(),
				_ => unreachable!(),
			};
			assert_network_actions_contains(
				&actions,
				&NetworkAction::WriteNotification(peer_id, peer_set, wire_msg),
			);
		}

		virtual_overseer
	});
}

// Test rx bridge sends the newest gossip topology to all subsystems and old ones only to approval
// distribution.
#[test]
fn network_new_topology_update() {
	let (oracle, handle) = make_sync_oracle(false);
	test_harness(Box::new(oracle), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer_ids: Vec<_> = (0..4).map(|_| PeerId::random()).collect();
		let peers = [
			(peer_ids[0], PeerSet::Validation, ValidationVersion::V3.into()),
			(peer_ids[1], PeerSet::Validation, ValidationVersion::V3.into()),
			(peer_ids[2], PeerSet::Validation, ValidationVersion::V3.into()),
			(peer_ids[3], PeerSet::Collation, CollationVersion::V1.into()),
		];

		let head = Hash::repeat_byte(1);
		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::start_work(new_leaf(head, 1)),
			)))
			.await;

		handle.await_mode_switch().await;

		let mut total_validation_peers = 0;
		let mut total_collation_peers = 0;

		for &(peer_id, peer_set, version) in &peers {
			network_handle
				.connect_peer(peer_id, version, peer_set, ObservedRole::Full)
				.await;

			match peer_set {
				PeerSet::Validation => total_validation_peers += 1,
				PeerSet::Collation => total_collation_peers += 1,
			}
		}

		await_peer_connections(&shared, total_validation_peers, total_collation_peers).await;

		// Drain setup messages.
		while let Some(_) = virtual_overseer.recv().timeout(Duration::from_secs(1)).await {}

		// 1. Send new gossip topology and check is sent to all subsystems.
		virtual_overseer
			.send(polkadot_overseer::FromOrchestra::Communication {
				msg: NetworkBridgeRxMessage::NewGossipTopology {
					session: 2,
					local_index: Some(ValidatorIndex(0)),
					canonical_shuffling: Vec::new(),
					shuffled_indices: Vec::new(),
				},
			})
			.await;

		assert_sends_validation_event_to_all(
			NetworkBridgeEvent::NewGossipTopology(NewGossipTopology {
				session: 2,
				topology: SessionGridTopology::new(Vec::new(), Vec::new()),
				local_index: Some(ValidatorIndex(0)),
			}),
			&mut virtual_overseer,
		)
		.await;

		// 2. Send old gossip topology and check is sent only to approval distribution.
		virtual_overseer
			.send(polkadot_overseer::FromOrchestra::Communication {
				msg: NetworkBridgeRxMessage::NewGossipTopology {
					session: 1,
					local_index: Some(ValidatorIndex(0)),
					canonical_shuffling: Vec::new(),
					shuffled_indices: Vec::new(),
				},
			})
			.await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ApprovalVotingParallel(
				ApprovalVotingParallelMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::NewGossipTopology(NewGossipTopology {
						session: 1,
						topology: _,
						local_index: _,
					})
				)
			)
		);

		// 3. Send new gossip topology and check is sent to all subsystems.
		virtual_overseer
			.send(polkadot_overseer::FromOrchestra::Communication {
				msg: NetworkBridgeRxMessage::NewGossipTopology {
					session: 3,
					local_index: Some(ValidatorIndex(0)),
					canonical_shuffling: Vec::new(),
					shuffled_indices: Vec::new(),
				},
			})
			.await;

		assert_sends_validation_event_to_all(
			NetworkBridgeEvent::NewGossipTopology(NewGossipTopology {
				session: 3,
				topology: SessionGridTopology::new(Vec::new(), Vec::new()),
				local_index: Some(ValidatorIndex(0)),
			}),
			&mut virtual_overseer,
		)
		.await;
		virtual_overseer
	});
}

#[test]
fn network_protocol_versioning_subsystem_msg() {
	use std::task::Poll;

	let (oracle, _handle) = make_sync_oracle(false);
	test_harness(Box::new(oracle), |test_harness| async move {
		let TestHarness { mut network_handle, mut virtual_overseer, shared } = test_harness;

		let peer = PeerId::random();

		network_handle
			.connect_peer(peer, CollationVersion::V1.into(), PeerSet::Collation, ObservedRole::Full)
			.await;
		await_peer_connections(&shared, 0, 1).await;

		// bridge will inform about all connected peers.
		{
			assert_sends_collation_event_to_all(
				NetworkBridgeEvent::PeerConnected(
					peer,
					ObservedRole::Full,
					CollationVersion::V1.into(),
					None,
				),
				&mut virtual_overseer,
			)
			.await;

			assert_sends_collation_event_to_all(
				NetworkBridgeEvent::PeerViewChange(peer, View::default()),
				&mut virtual_overseer,
			)
			.await;

			assert_eq!(virtual_overseer.message_counter.with_high_priority(), 0);
		}

		let collator_protocol_message = protocol_v1::CollatorProtocolMessage::Declare(
			Sr25519Keyring::Alice.public().into(),
			Default::default(),
			sp_core::crypto::UncheckedFrom::unchecked_from([1u8; 64]),
		);

		let msg =
			protocol_v1::CollationProtocol::CollatorProtocol(collator_protocol_message.clone());

		network_handle
			.peer_message(
				peer,
				PeerSet::Collation,
				WireMessage::ProtocolMessage(msg.clone()).encode(),
			)
			.await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(
				CollatorProtocolMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::PeerMessage(p, CollationProtocols::V1(m))
				)
			) => {
				assert_eq!(p, peer);
				assert_eq!(m, collator_protocol_message);
			}
		);

		// No more messages.
		assert_matches!(futures::poll!(virtual_overseer.recv().boxed()), Poll::Pending);

		virtual_overseer
	});
}
