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

use crate::{error::InternalValidationError, ArtifactChecksum};
use codec::{Decode, Encode};
use polkadot_node_primitives::PoV;
use polkadot_parachain_primitives::primitives::ValidationResult;
use polkadot_primitives::{ExecutorParams, PersistedValidationData};
use std::time::Duration;

/// The payload of the one-time handshake that is done when a worker process is created. Carries
/// data from the host to the worker.
#[derive(Encode, Decode)]
pub struct Handshake {
	/// The executor parameters.
	pub executor_params: ExecutorParams,
}

/// A request to execute a PVF
#[derive(Encode, Decode)]
pub struct ExecuteRequest {
	/// Persisted validation data.
	pub pvd: PersistedValidationData,
	/// Proof-of-validity.
	pub pov: PoV,
	/// Execution timeout.
	pub execution_timeout: Duration,
	/// Checksum of the artifact to execute.
	pub artifact_checksum: ArtifactChecksum,
}

/// The response from the execution worker.
#[derive(Debug, Encode, Decode)]
pub struct WorkerResponse {
	/// The response from the execute job process.
	pub job_response: JobResponse,
	/// The amount of CPU time taken by the job.
	pub duration: Duration,
	/// The uncompressed PoV size.
	pub pov_size: u32,
}

/// An error occurred in the worker process.
#[derive(thiserror::Error, Debug, Clone, Encode, Decode)]
pub enum WorkerError {
	/// The job timed out.
	#[error("The job timed out")]
	JobTimedOut,
	/// The job process has died. We must kill the worker just in case.
	///
	/// We cannot treat this as an internal error because malicious code may have killed the job.
	/// We still retry it, because in the non-malicious case it is likely spurious.
	#[error("The job process (pid {job_pid}) has died: {err}")]
	JobDied { err: String, job_pid: i32 },
	/// An unexpected error occurred in the job process, e.g. failing to spawn a thread, panic,
	/// etc.
	///
	/// Because malicious code can cause a job error, we must not treat it as an internal error. We
	/// still retry it, because in the non-malicious case it is likely spurious.
	#[error("An unexpected error occurred in the job process: {0}")]
	JobError(#[from] JobError),

	/// Some internal error occurred.
	#[error("An internal error occurred: {0}")]
	InternalError(#[from] InternalValidationError),
}

/// The result of a job on the execution worker.
pub type JobResult = Result<JobResponse, JobError>;

/// The successful response from a job on the execution worker.
#[derive(Debug, Encode, Decode)]
pub enum JobResponse {
	Ok {
		/// The result of parachain validation.
		result_descriptor: ValidationResult,
	},
	/// A possibly transient runtime instantiation error happened during the execution; may be
	/// retried with re-preparation
	RuntimeConstruction(String),
	/// The candidate is invalid.
	InvalidCandidate(String),
	/// PoV decompression failed
	PoVDecompressionFailure,
	/// The artifact is corrupted, re-prepare the artifact and try again.
	CorruptedArtifact,
}

impl JobResponse {
	/// Creates an invalid response from a context `ctx` and a message `msg` (which can be empty).
	pub fn format_invalid(ctx: &'static str, msg: &str) -> Self {
		if msg.is_empty() {
			Self::InvalidCandidate(ctx.to_string())
		} else {
			Self::InvalidCandidate(format!("{}: {}", ctx, msg))
		}
	}

	/// Creates a may retry response from a context `ctx` and a message `msg` (which can be empty).
	pub fn runtime_construction(ctx: &'static str, msg: &str) -> Self {
		if msg.is_empty() {
			Self::RuntimeConstruction(ctx.to_string())
		} else {
			Self::RuntimeConstruction(format!("{}: {}", ctx, msg))
		}
	}
}

/// An unexpected error occurred in the execution job process. Because this comes from the job,
/// which executes untrusted code, this error must likewise be treated as untrusted. That is, we
/// cannot raise an internal error based on this.
#[derive(thiserror::Error, Clone, Debug, Encode, Decode)]
pub enum JobError {
	#[error("The job timed out")]
	TimedOut,
	#[error("An unexpected panic has occurred in the execution job: {0}")]
	Panic(String),
	/// Some error occurred when interfacing with the kernel.
	#[error("Error interfacing with the kernel: {0}")]
	Kernel(String),
	#[error("Could not spawn the requested thread: {0}")]
	CouldNotSpawnThread(String),
	#[error("An error occurred in the CPU time monitor thread: {0}")]
	CpuTimeMonitorThread(String),
	/// Since the job can return any exit status it wants, we have to treat this as untrusted.
	#[error("Unexpected exit status: {0}")]
	UnexpectedExitStatus(i32),
}
