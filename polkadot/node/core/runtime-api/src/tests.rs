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

use polkadot_node_primitives::{BabeAllowedSlots, BabeEpoch, BabeEpochConfiguration};
use polkadot_node_subsystem::SpawnGlue;
use polkadot_node_subsystem_test_helpers::make_subsystem_context;
use polkadot_primitives::{
	async_backing, slashing, vstaging,
	vstaging::{
		async_backing::Constraints, CandidateEvent,
		CommittedCandidateReceiptV2 as CommittedCandidateReceipt, CoreState, ScrapedOnChainVotes,
	},
	ApprovalVotingParams, AuthorityDiscoveryId, BlockNumber, CandidateCommitments, CandidateHash,
	CoreIndex, DisputeState, ExecutorParams, GroupRotationInfo, Id as ParaId,
	InboundDownwardMessage, InboundHrmpMessage, NodeFeatures, OccupiedCoreAssumption,
	PersistedValidationData, PvfCheckStatement, SessionIndex, SessionInfo, Slot, ValidationCode,
	ValidationCodeHash, ValidatorId, ValidatorIndex, ValidatorSignature,
};
use polkadot_primitives_test_helpers::{
	dummy_committed_candidate_receipt_v2, dummy_validation_code,
};
use sp_api::ApiError;
use sp_core::testing::TaskExecutor;
use std::{
	collections::{BTreeMap, HashMap, VecDeque},
	sync::{Arc, Mutex},
};

#[derive(Default)]
struct MockSubsystemClient {
	submitted_pvf_check_statement: Arc<Mutex<Vec<(PvfCheckStatement, ValidatorSignature)>>>,
	authorities: Vec<AuthorityDiscoveryId>,
	validators: Vec<ValidatorId>,
	validator_groups: Vec<Vec<ValidatorIndex>>,
	availability_cores: Vec<CoreState>,
	validation_data: HashMap<ParaId, PersistedValidationData>,
	validation_code: HashMap<ParaId, ValidationCode>,
	validation_outputs_results: HashMap<ParaId, bool>,
	session_index_for_child: SessionIndex,
	candidate_pending_availability: HashMap<ParaId, CommittedCandidateReceipt>,
	candidates_pending_availability: HashMap<ParaId, Vec<CommittedCandidateReceipt>>,
	dmq_contents: HashMap<ParaId, Vec<InboundDownwardMessage>>,
	hrmp_channels: HashMap<ParaId, BTreeMap<ParaId, Vec<InboundHrmpMessage>>>,
	validation_code_by_hash: HashMap<ValidationCodeHash, ValidationCode>,
	availability_cores_wait: Arc<Mutex<()>>,
	babe_epoch: Option<BabeEpoch>,
	pvfs_require_precheck: Vec<ValidationCodeHash>,
	validation_code_hash: HashMap<ParaId, ValidationCodeHash>,
	session_info: HashMap<SessionIndex, SessionInfo>,
	candidate_events: Vec<CandidateEvent>,
}

#[async_trait::async_trait]
impl RuntimeApiSubsystemClient for MockSubsystemClient {
	async fn api_version_parachain_host(&self, _: Hash) -> Result<Option<u32>, ApiError> {
		Ok(Some(5))
	}

	async fn validators(&self, _: Hash) -> Result<Vec<ValidatorId>, ApiError> {
		Ok(self.validators.clone())
	}

	async fn validator_groups(
		&self,
		_: Hash,
	) -> Result<(Vec<Vec<ValidatorIndex>>, GroupRotationInfo<BlockNumber>), ApiError> {
		Ok((
			self.validator_groups.clone(),
			GroupRotationInfo { session_start_block: 1, group_rotation_frequency: 100, now: 10 },
		))
	}

	async fn availability_cores(
		&self,
		_: Hash,
	) -> Result<Vec<CoreState<Hash, BlockNumber>>, ApiError> {
		let _lock = self.availability_cores_wait.lock().unwrap();
		Ok(self.availability_cores.clone())
	}

	async fn persisted_validation_data(
		&self,
		_: Hash,
		para_id: ParaId,
		_: OccupiedCoreAssumption,
	) -> Result<Option<PersistedValidationData<Hash, BlockNumber>>, ApiError> {
		Ok(self.validation_data.get(&para_id).cloned())
	}

	async fn assumed_validation_data(
		&self,
		_: Hash,
		para_id: ParaId,
		expected_persisted_validation_data_hash: Hash,
	) -> Result<Option<(PersistedValidationData<Hash, BlockNumber>, ValidationCodeHash)>, ApiError>
	{
		Ok(self
			.validation_data
			.get(&para_id)
			.cloned()
			.filter(|data| data.hash() == expected_persisted_validation_data_hash)
			.zip(self.validation_code.get(&para_id).map(|code| code.hash())))
	}

	async fn check_validation_outputs(
		&self,
		_: Hash,
		para_id: ParaId,
		_: CandidateCommitments,
	) -> Result<bool, ApiError> {
		Ok(self.validation_outputs_results.get(&para_id).copied().unwrap())
	}

	async fn session_index_for_child(&self, _: Hash) -> Result<SessionIndex, ApiError> {
		Ok(self.session_index_for_child)
	}

	async fn validation_code(
		&self,
		_: Hash,
		para_id: ParaId,
		_: OccupiedCoreAssumption,
	) -> Result<Option<ValidationCode>, ApiError> {
		Ok(self.validation_code.get(&para_id).cloned())
	}

	async fn candidate_pending_availability(
		&self,
		_: Hash,
		para_id: ParaId,
	) -> Result<Option<CommittedCandidateReceipt<Hash>>, ApiError> {
		Ok(self.candidate_pending_availability.get(&para_id).cloned())
	}

	async fn candidates_pending_availability(
		&self,
		_: Hash,
		para_id: ParaId,
	) -> Result<Vec<CommittedCandidateReceipt<Hash>>, ApiError> {
		Ok(self.candidates_pending_availability.get(&para_id).cloned().unwrap_or_default())
	}

	async fn candidate_events(&self, _: Hash) -> Result<Vec<CandidateEvent<Hash>>, ApiError> {
		Ok(self.candidate_events.clone())
	}

	async fn dmq_contents(
		&self,
		_: Hash,
		para_id: ParaId,
	) -> Result<Vec<InboundDownwardMessage<BlockNumber>>, ApiError> {
		Ok(self.dmq_contents.get(&para_id).cloned().unwrap())
	}

	async fn inbound_hrmp_channels_contents(
		&self,
		_: Hash,
		para_id: ParaId,
	) -> Result<BTreeMap<ParaId, Vec<InboundHrmpMessage<BlockNumber>>>, ApiError> {
		Ok(self.hrmp_channels.get(&para_id).cloned().unwrap())
	}

	async fn validation_code_by_hash(
		&self,
		_: Hash,
		hash: ValidationCodeHash,
	) -> Result<Option<ValidationCode>, ApiError> {
		Ok(self.validation_code_by_hash.get(&hash).cloned())
	}

	async fn on_chain_votes(&self, _: Hash) -> Result<Option<ScrapedOnChainVotes<Hash>>, ApiError> {
		todo!("Not required for tests")
	}

	async fn session_info(
		&self,
		_: Hash,
		index: SessionIndex,
	) -> Result<Option<SessionInfo>, ApiError> {
		Ok(self.session_info.get(&index).cloned())
	}

	async fn submit_pvf_check_statement(
		&self,
		_: Hash,
		stmt: PvfCheckStatement,
		sig: ValidatorSignature,
	) -> Result<(), ApiError> {
		self.submitted_pvf_check_statement.lock().unwrap().push((stmt, sig));
		Ok(())
	}

	async fn pvfs_require_precheck(&self, _: Hash) -> Result<Vec<ValidationCodeHash>, ApiError> {
		Ok(self.pvfs_require_precheck.clone())
	}

	async fn validation_code_hash(
		&self,
		_: Hash,
		para_id: ParaId,
		_: OccupiedCoreAssumption,
	) -> Result<Option<ValidationCodeHash>, ApiError> {
		Ok(self.validation_code_hash.get(&para_id).cloned())
	}

	async fn disputes(
		&self,
		_: Hash,
	) -> Result<Vec<(SessionIndex, CandidateHash, DisputeState<BlockNumber>)>, ApiError> {
		todo!("Not required for tests")
	}

	async fn unapplied_slashes(
		&self,
		_: Hash,
	) -> Result<Vec<(SessionIndex, CandidateHash, slashing::PendingSlashes)>, ApiError> {
		todo!("Not required for tests")
	}

	async fn key_ownership_proof(
		&self,
		_: Hash,
		_: ValidatorId,
	) -> Result<Option<slashing::OpaqueKeyOwnershipProof>, ApiError> {
		todo!("Not required for tests")
	}

	async fn submit_report_dispute_lost(
		&self,
		_: Hash,
		_: slashing::DisputeProof,
		_: slashing::OpaqueKeyOwnershipProof,
	) -> Result<Option<()>, ApiError> {
		todo!("Not required for tests")
	}

	async fn session_executor_params(
		&self,
		_: Hash,
		_: SessionIndex,
	) -> Result<Option<ExecutorParams>, ApiError> {
		todo!("Not required for tests")
	}

	/// Approval voting configuration parameters
	async fn approval_voting_params(
		&self,
		_: Hash,
		_: SessionIndex,
	) -> Result<ApprovalVotingParams, ApiError> {
		todo!("Not required for tests")
	}

	async fn current_epoch(&self, _: Hash) -> Result<sp_consensus_babe::Epoch, ApiError> {
		Ok(self.babe_epoch.as_ref().unwrap().clone())
	}

	async fn authorities(&self, _: Hash) -> Result<Vec<AuthorityDiscoveryId>, ApiError> {
		Ok(self.authorities.clone())
	}

	async fn async_backing_params(
		&self,
		_: Hash,
	) -> Result<async_backing::AsyncBackingParams, ApiError> {
		todo!("Not required for tests")
	}

	async fn para_backing_state(
		&self,
		_: Hash,
		_: ParaId,
	) -> Result<Option<vstaging::async_backing::BackingState>, ApiError> {
		todo!("Not required for tests")
	}

	async fn minimum_backing_votes(&self, _: Hash, _: SessionIndex) -> Result<u32, ApiError> {
		todo!("Not required for tests")
	}

	async fn node_features(&self, _: Hash) -> Result<NodeFeatures, ApiError> {
		todo!("Not required for tests")
	}

	async fn disabled_validators(&self, _: Hash) -> Result<Vec<ValidatorIndex>, ApiError> {
		todo!("Not required for tests")
	}

	async fn claim_queue(
		&self,
		_: Hash,
	) -> Result<BTreeMap<CoreIndex, VecDeque<ParaId>>, ApiError> {
		todo!("Not required for tests")
	}

	async fn scheduling_lookahead(&self, _: Hash) -> Result<u32, ApiError> {
		todo!("Not required for tests")
	}

	async fn backing_constraints(
		&self,
		_at: Hash,
		_para_id: ParaId,
	) -> Result<Option<Constraints>, ApiError> {
		todo!("Not required for tests")
	}

	async fn validation_code_bomb_limit(&self, _: Hash) -> Result<u32, ApiError> {
		todo!("Not required for tests")
	}

	async fn para_ids(&self, _: Hash) -> Result<Vec<ParaId>, ApiError> {
		todo!("Not required for tests")
	}
}

#[test]
fn requests_authorities() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let subsystem_client = Arc::new(MockSubsystemClient::default());
	let relay_parent = [1; 32].into();
	let spawner = sp_core::testing::TaskExecutor::new();

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(relay_parent, Request::Authorities(tx)),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), subsystem_client.authorities);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_validators() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let subsystem_client = Arc::new(MockSubsystemClient::default());
	let relay_parent = [1; 32].into();
	let spawner = sp_core::testing::TaskExecutor::new();

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(relay_parent, Request::Validators(tx)),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), subsystem_client.validators);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_validator_groups() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let subsystem_client = Arc::new(MockSubsystemClient::default());
	let relay_parent = [1; 32].into();
	let spawner = sp_core::testing::TaskExecutor::new();

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(relay_parent, Request::ValidatorGroups(tx)),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap().0, subsystem_client.validator_groups);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_availability_cores() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let subsystem_client = Arc::new(MockSubsystemClient::default());
	let relay_parent = [1; 32].into();
	let spawner = sp_core::testing::TaskExecutor::new();

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(relay_parent, Request::AvailabilityCores(tx)),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), subsystem_client.availability_cores);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_persisted_validation_data() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let relay_parent = [1; 32].into();
	let para_a = ParaId::from(5_u32);
	let para_b = ParaId::from(6_u32);
	let spawner = sp_core::testing::TaskExecutor::new();

	let mut subsystem_client = MockSubsystemClient::default();
	subsystem_client.validation_data.insert(para_a, Default::default());
	let subsystem_client = Arc::new(subsystem_client);

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::PersistedValidationData(para_a, OccupiedCoreAssumption::Included, tx),
				),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), Some(Default::default()));

		let (tx, rx) = oneshot::channel();
		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::PersistedValidationData(para_b, OccupiedCoreAssumption::Included, tx),
				),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), None);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_assumed_validation_data() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let relay_parent = [1; 32].into();
	let para_a = ParaId::from(5_u32);
	let para_b = ParaId::from(6_u32);
	let spawner = sp_core::testing::TaskExecutor::new();

	let validation_code = ValidationCode(vec![1, 2, 3]);
	let expected_data_hash = <PersistedValidationData as Default>::default().hash();
	let expected_code_hash = validation_code.hash();

	let mut subsystem_client = MockSubsystemClient::default();
	subsystem_client.validation_data.insert(para_a, Default::default());
	subsystem_client.validation_code.insert(para_a, validation_code);
	subsystem_client.validation_data.insert(para_b, Default::default());
	let subsystem_client = Arc::new(subsystem_client);

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::AssumedValidationData(para_a, expected_data_hash, tx),
				),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), Some((Default::default(), expected_code_hash)));

		let (tx, rx) = oneshot::channel();
		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::AssumedValidationData(para_a, Hash::zero(), tx),
				),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), None);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_check_validation_outputs() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let mut subsystem_client = MockSubsystemClient::default();
	let relay_parent = [1; 32].into();
	let para_a = ParaId::from(5_u32);
	let para_b = ParaId::from(6_u32);
	let commitments = polkadot_primitives::CandidateCommitments::default();
	let spawner = sp_core::testing::TaskExecutor::new();

	subsystem_client.validation_outputs_results.insert(para_a, false);
	subsystem_client.validation_outputs_results.insert(para_b, true);

	let subsystem_client = Arc::new(subsystem_client);

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::CheckValidationOutputs(para_a, commitments.clone(), tx),
				),
			})
			.await;
		assert_eq!(
			rx.await.unwrap().unwrap(),
			subsystem_client.validation_outputs_results[&para_a]
		);

		let (tx, rx) = oneshot::channel();
		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::CheckValidationOutputs(para_b, commitments, tx),
				),
			})
			.await;
		assert_eq!(
			rx.await.unwrap().unwrap(),
			subsystem_client.validation_outputs_results[&para_b]
		);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_session_index_for_child() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let subsystem_client = Arc::new(MockSubsystemClient::default());
	let relay_parent = [1; 32].into();
	let spawner = sp_core::testing::TaskExecutor::new();

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(relay_parent, Request::SessionIndexForChild(tx)),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), subsystem_client.session_index_for_child);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

fn dummy_session_info() -> SessionInfo {
	SessionInfo {
		validators: Default::default(),
		discovery_keys: vec![],
		assignment_keys: vec![],
		validator_groups: Default::default(),
		n_cores: 4u32,
		zeroth_delay_tranche_width: 0u32,
		relay_vrf_modulo_samples: 0u32,
		n_delay_tranches: 2u32,
		no_show_slots: 0u32,
		needed_approvals: 1u32,
		active_validator_indices: vec![],
		dispute_period: 6,
		random_seed: [0u8; 32],
	}
}
#[test]
fn requests_session_info() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let mut subsystem_client = MockSubsystemClient::default();
	let session_index = 1;
	subsystem_client.session_info.insert(session_index, dummy_session_info());
	let subsystem_client = Arc::new(subsystem_client);
	let spawner = sp_core::testing::TaskExecutor::new();

	let relay_parent = [1; 32].into();

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::SessionInfo(session_index, tx),
				),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), Some(dummy_session_info()));

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_validation_code() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());

	let relay_parent = [1; 32].into();
	let para_a = ParaId::from(5_u32);
	let para_b = ParaId::from(6_u32);
	let spawner = sp_core::testing::TaskExecutor::new();
	let validation_code = dummy_validation_code();

	let mut subsystem_client = MockSubsystemClient::default();
	subsystem_client.validation_code.insert(para_a, validation_code.clone());
	let subsystem_client = Arc::new(subsystem_client);

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::ValidationCode(para_a, OccupiedCoreAssumption::Included, tx),
				),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), Some(validation_code));

		let (tx, rx) = oneshot::channel();
		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::ValidationCode(para_b, OccupiedCoreAssumption::Included, tx),
				),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), None);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_candidate_pending_availability() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let relay_parent = [1; 32].into();
	let para_a = ParaId::from(5_u32);
	let para_b = ParaId::from(6_u32);
	let spawner = sp_core::testing::TaskExecutor::new();
	let candidate_receipt = dummy_committed_candidate_receipt_v2(relay_parent);

	let mut subsystem_client = MockSubsystemClient::default();
	subsystem_client
		.candidate_pending_availability
		.insert(para_a, candidate_receipt.clone());
	let subsystem_client = Arc::new(subsystem_client);

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::CandidatePendingAvailability(para_a, tx),
				),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), Some(candidate_receipt));

		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::CandidatePendingAvailability(para_b, tx),
				),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), None);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_candidate_events() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let subsystem_client = Arc::new(MockSubsystemClient::default());
	let relay_parent = [1; 32].into();
	let spawner = sp_core::testing::TaskExecutor::new();

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(relay_parent, Request::CandidateEvents(tx)),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), subsystem_client.candidate_events);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_dmq_contents() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());

	let relay_parent = [1; 32].into();
	let para_a = ParaId::from(5_u32);
	let para_b = ParaId::from(6_u32);
	let spawner = sp_core::testing::TaskExecutor::new();

	let subsystem_client = Arc::new({
		let mut subsystem_client = MockSubsystemClient::default();

		subsystem_client.dmq_contents.insert(para_a, vec![]);
		subsystem_client.dmq_contents.insert(
			para_b,
			vec![InboundDownwardMessage { sent_at: 228, msg: b"Novus Ordo Seclorum".to_vec() }],
		);

		subsystem_client
	});

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();
		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(relay_parent, Request::DmqContents(para_a, tx)),
			})
			.await;
		assert_eq!(rx.await.unwrap().unwrap(), vec![]);

		let (tx, rx) = oneshot::channel();
		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(relay_parent, Request::DmqContents(para_b, tx)),
			})
			.await;
		assert_eq!(
			rx.await.unwrap().unwrap(),
			vec![InboundDownwardMessage { sent_at: 228, msg: b"Novus Ordo Seclorum".to_vec() }]
		);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};
	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_inbound_hrmp_channels_contents() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());

	let relay_parent = [1; 32].into();
	let para_a = ParaId::from(99_u32);
	let para_b = ParaId::from(66_u32);
	let para_c = ParaId::from(33_u32);
	let spawner = sp_core::testing::TaskExecutor::new();

	let para_b_inbound_channels = [
		(para_a, vec![]),
		(para_c, vec![InboundHrmpMessage { sent_at: 1, data: "𝙀=𝙈𝘾²".as_bytes().to_owned() }]),
	]
	.into_iter()
	.collect::<BTreeMap<_, _>>();

	let subsystem_client = Arc::new({
		let mut subsystem_client = MockSubsystemClient::default();

		subsystem_client.hrmp_channels.insert(para_a, BTreeMap::new());
		subsystem_client.hrmp_channels.insert(para_b, para_b_inbound_channels.clone());

		subsystem_client
	});

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();
		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::InboundHrmpChannelsContents(para_a, tx),
				),
			})
			.await;
		assert_eq!(rx.await.unwrap().unwrap(), BTreeMap::new());

		let (tx, rx) = oneshot::channel();
		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::InboundHrmpChannelsContents(para_b, tx),
				),
			})
			.await;
		assert_eq!(rx.await.unwrap().unwrap(), para_b_inbound_channels);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};
	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_validation_code_by_hash() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let spawner = sp_core::testing::TaskExecutor::new();

	let (subsystem_client, validation_code) = {
		let mut subsystem_client = MockSubsystemClient::default();
		let mut validation_code = Vec::new();

		for n in 0..5 {
			let code = ValidationCode::from(vec![n; 32]);
			subsystem_client.validation_code_by_hash.insert(code.hash(), code.clone());
			validation_code.push(code);
		}

		(Arc::new(subsystem_client), validation_code)
	};

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());

	let relay_parent = [1; 32].into();
	let test_task = async move {
		for code in validation_code {
			let (tx, rx) = oneshot::channel();
			ctx_handle
				.send(FromOrchestra::Communication {
					msg: RuntimeApiMessage::Request(
						relay_parent,
						Request::ValidationCodeByHash(code.hash(), tx),
					),
				})
				.await;

			assert_eq!(rx.await.unwrap().unwrap(), Some(code));
		}

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn multiple_requests_in_parallel_are_working() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let subsystem_client = Arc::new(MockSubsystemClient::default());
	let relay_parent = [1; 32].into();
	let spawner = sp_core::testing::TaskExecutor::new();
	let mutex = subsystem_client.availability_cores_wait.clone();

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		// Make all requests block until we release this mutex.
		let lock = mutex.lock().unwrap();

		let mut receivers = Vec::new();
		for _ in 0..MAX_PARALLEL_REQUESTS {
			let (tx, rx) = oneshot::channel();

			ctx_handle
				.send(FromOrchestra::Communication {
					msg: RuntimeApiMessage::Request(relay_parent, Request::AvailabilityCores(tx)),
				})
				.await;
			receivers.push(rx);
		}

		// The backpressure from reaching `MAX_PARALLEL_REQUESTS` will make the test block, we need
		// to drop the lock.
		drop(lock);

		for _ in 0..MAX_PARALLEL_REQUESTS * 100 {
			let (tx, rx) = oneshot::channel();

			ctx_handle
				.send(FromOrchestra::Communication {
					msg: RuntimeApiMessage::Request(relay_parent, Request::AvailabilityCores(tx)),
				})
				.await;
			receivers.push(rx);
		}

		let join = future::join_all(receivers);

		join.await
			.into_iter()
			.for_each(|r| assert_eq!(r.unwrap().unwrap(), subsystem_client.availability_cores));

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_babe_epoch() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let mut subsystem_client = MockSubsystemClient::default();
	let epoch = BabeEpoch {
		epoch_index: 100,
		start_slot: Slot::from(1000),
		duration: 10,
		authorities: Vec::new(),
		randomness: [1u8; 32],
		config: BabeEpochConfiguration { c: (1, 4), allowed_slots: BabeAllowedSlots::PrimarySlots },
	};
	subsystem_client.babe_epoch = Some(epoch.clone());
	let subsystem_client = Arc::new(subsystem_client);
	let relay_parent = [1; 32].into();
	let spawner = sp_core::testing::TaskExecutor::new();

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(relay_parent, Request::CurrentBabeEpoch(tx)),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), epoch);
		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_submit_pvf_check_statement() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let spawner = sp_core::testing::TaskExecutor::new();
	let subsystem_client = Arc::new(MockSubsystemClient::default());

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());

	let relay_parent = [1; 32].into();
	let test_task = async move {
		let (stmt, sig) = fake_statement();

		// Send the same statement twice.
		//
		// Here we just want to ensure that those requests do not go through the cache.
		let (tx, rx) = oneshot::channel();
		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::SubmitPvfCheckStatement(stmt.clone(), sig.clone(), tx),
				),
			})
			.await;
		let _ = rx.await.unwrap().unwrap();
		let (tx, rx) = oneshot::channel();
		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::SubmitPvfCheckStatement(stmt.clone(), sig.clone(), tx),
				),
			})
			.await;
		let _ = rx.await.unwrap().unwrap();

		assert_eq!(
			&*subsystem_client.submitted_pvf_check_statement.lock().expect("poisoned mutex"),
			&[(stmt.clone(), sig.clone()), (stmt.clone(), sig.clone())]
		);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));

	fn fake_statement() -> (PvfCheckStatement, ValidatorSignature) {
		let stmt = PvfCheckStatement {
			accept: true,
			subject: [1; 32].into(),
			session_index: 1,
			validator_index: 1.into(),
		};
		let sig = sp_keyring::Sr25519Keyring::Alice.sign(&stmt.signing_payload()).into();
		(stmt, sig)
	}
}

#[test]
fn requests_pvfs_require_precheck() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let spawner = sp_core::testing::TaskExecutor::new();

	let subsystem_client = Arc::new({
		let mut subsystem_client = MockSubsystemClient::default();
		subsystem_client.pvfs_require_precheck = vec![[1; 32].into(), [2; 32].into()];
		subsystem_client
	});

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());

	let relay_parent = [1; 32].into();
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(relay_parent, Request::PvfsRequirePrecheck(tx)),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), vec![[1; 32].into(), [2; 32].into()]);
		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}

#[test]
fn requests_validation_code_hash() {
	let (ctx, mut ctx_handle) = make_subsystem_context(TaskExecutor::new());

	let relay_parent = [1; 32].into();
	let para_a = ParaId::from(5_u32);
	let para_b = ParaId::from(6_u32);
	let spawner = sp_core::testing::TaskExecutor::new();
	let validation_code_hash = dummy_validation_code().hash();

	let mut subsystem_client = MockSubsystemClient::default();
	subsystem_client.validation_code_hash.insert(para_a, validation_code_hash);
	let subsystem_client = Arc::new(subsystem_client);

	let subsystem =
		RuntimeApiSubsystem::new(subsystem_client.clone(), Metrics(None), SpawnGlue(spawner));
	let subsystem_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = async move {
		let (tx, rx) = oneshot::channel();

		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::ValidationCodeHash(para_a, OccupiedCoreAssumption::Included, tx),
				),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), Some(validation_code_hash));

		let (tx, rx) = oneshot::channel();
		ctx_handle
			.send(FromOrchestra::Communication {
				msg: RuntimeApiMessage::Request(
					relay_parent,
					Request::ValidationCodeHash(para_b, OccupiedCoreAssumption::Included, tx),
				),
			})
			.await;

		assert_eq!(rx.await.unwrap().unwrap(), None);

		ctx_handle.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
	};

	futures::executor::block_on(future::join(subsystem_task, test_task));
}
