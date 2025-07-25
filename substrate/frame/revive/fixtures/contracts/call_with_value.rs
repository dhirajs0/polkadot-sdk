// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This calls another contract as passed as its account id.
#![no_std]
#![no_main]
include!("../panic_handler.rs");

use uapi::{input, HostFn, HostFnImpl as api};

#[no_mangle]
#[polkavm_derive::polkavm_export]
pub extern "C" fn deploy() {}

#[no_mangle]
#[polkavm_derive::polkavm_export]
pub extern "C" fn call() {
	input!(
		value: &[u8; 32],
		callee_addr: &[u8; 20],
	);

	// Call the callee
	api::call(
		uapi::CallFlags::empty(),
		callee_addr,
		u64::MAX,       // How much ref_time to devote for the execution. u64::MAX = use all.
		u64::MAX,       // How much proof_size to devote for the execution. u64::MAX = use all.
		&[u8::MAX; 32], // No deposit limit.
		value,          // Value transferred to the contract.
		&[0u8; 0],      // input
		None,
	)
	.unwrap();
}
