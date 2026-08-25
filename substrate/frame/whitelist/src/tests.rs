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

// Tests for Whitelist Pallet

use crate::mock::*;
use codec::Encode;
use frame::{
	testing_prelude::*,
	traits::{QueryPreimage, StorePreimage},
};

/// Run the given closure in a fresh test externality and assert that all of the pallet's
/// invariants hold afterwards.
fn test(f: impl FnOnce()) {
	new_test_ext().execute_with(|| {
		f();
		Whitelist::do_try_state().expect("All invariants must hold after a test");
	});
}

#[test]
fn test_whitelist_call_and_remove() {
	test(|| {
		let call = RuntimeCall::System(frame_system::Call::remark { remark: vec![] });
		let encoded_call = call.encode();
		let call_hash = <Test as frame_system::Config>::Hashing::hash(&encoded_call[..]);

		assert_noop!(
			Whitelist::remove_whitelisted_call(RuntimeOrigin::root(), call_hash),
			crate::Error::<Test>::CallIsNotWhitelisted,
		);

		assert_noop!(
			Whitelist::whitelist_call(RuntimeOrigin::signed(1), call_hash),
			DispatchError::BadOrigin,
		);

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));

		assert!(Preimage::is_requested(&call_hash));

		assert_noop!(
			Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash),
			crate::Error::<Test>::CallAlreadyWhitelisted,
		);

		assert_noop!(
			Whitelist::remove_whitelisted_call(RuntimeOrigin::signed(1), call_hash),
			DispatchError::BadOrigin,
		);

		assert_ok!(Whitelist::remove_whitelisted_call(RuntimeOrigin::root(), call_hash));

		assert!(!Preimage::is_requested(&call_hash));

		assert_noop!(
			Whitelist::remove_whitelisted_call(RuntimeOrigin::root(), call_hash),
			crate::Error::<Test>::CallIsNotWhitelisted,
		);
	});
}

#[test]
fn test_whitelist_call_and_execute() {
	test(|| {
		let call = RuntimeCall::System(frame_system::Call::remark_with_event { remark: vec![1] });
		let call_weight = call.get_dispatch_info().call_weight;
		let encoded_call = call.encode();
		let call_encoded_len = encoded_call.len() as u32;
		let call_hash = <Test as frame_system::Config>::Hashing::hash(&encoded_call[..]);

		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::root(),
				call_hash,
				call_encoded_len,
				call_weight
			),
			crate::Error::<Test>::CallIsNotWhitelisted,
		);

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));

		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::signed(1),
				call_hash,
				call_encoded_len,
				call_weight
			),
			DispatchError::BadOrigin,
		);

		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::root(),
				call_hash,
				call_encoded_len,
				call_weight
			),
			crate::Error::<Test>::UnavailablePreImage,
		);

		assert_ok!(Preimage::note(encoded_call.into()));

		assert!(Preimage::is_requested(&call_hash));

		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::root(),
				call_hash,
				call_encoded_len,
				call_weight - Weight::from_parts(1, 0)
			),
			crate::Error::<Test>::InvalidCallWeightWitness,
		);

		assert_ok!(Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::root(),
			call_hash,
			call_encoded_len,
			call_weight
		));

		assert!(!Preimage::is_requested(&call_hash));

		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::root(),
				call_hash,
				call_encoded_len,
				call_weight
			),
			crate::Error::<Test>::CallIsNotWhitelisted,
		);
	});
}

#[test]
fn test_whitelist_call_and_execute_failing_call() {
	test(|| {
		let call = RuntimeCall::Whitelist(crate::Call::dispatch_whitelisted_call {
			call_hash: Default::default(),
			call_encoded_len: Default::default(),
			call_weight_witness: Weight::zero(),
		});
		let call_weight = call.get_dispatch_info().call_weight;
		let encoded_call = call.encode();
		let call_encoded_len = encoded_call.len() as u32;
		let call_hash = <Test as frame_system::Config>::Hashing::hash(&encoded_call[..]);

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));
		assert_ok!(Preimage::note(encoded_call.into()));
		assert!(Preimage::is_requested(&call_hash));
		assert_ok!(Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::root(),
			call_hash,
			call_encoded_len,
			call_weight
		));
		assert!(!Preimage::is_requested(&call_hash));
	});
}

#[test]
fn test_whitelist_call_and_execute_without_note_preimage() {
	test(|| {
		let call = Box::new(RuntimeCall::System(frame_system::Call::remark_with_event {
			remark: vec![1],
		}));
		let call_hash = <Test as frame_system::Config>::Hashing::hash_of(&call);

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));
		assert!(Preimage::is_requested(&call_hash));

		assert_ok!(Whitelist::dispatch_whitelisted_call_with_preimage(
			RuntimeOrigin::root(),
			call.clone()
		));

		assert!(!Preimage::is_requested(&call_hash));

		assert_noop!(
			Whitelist::dispatch_whitelisted_call_with_preimage(RuntimeOrigin::root(), call),
			crate::Error::<Test>::CallIsNotWhitelisted,
		);
	});
}

#[test]
fn test_whitelist_call_and_execute_decode_consumes_all() {
	test(|| {
		let call = RuntimeCall::System(frame_system::Call::remark_with_event { remark: vec![1] });
		let call_weight = call.get_dispatch_info().call_weight;
		let mut call = call.encode();
		// Appending something does not make the encoded call invalid.
		// This tests that the decode function consumes all data.
		call.extend(call.clone());
		let call_encoded_len = call.len() as u32;

		let call_hash = <Test as frame_system::Config>::Hashing::hash(&call[..]);

		assert_ok!(Preimage::note(call.into()));
		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));

		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::root(),
				call_hash,
				call_encoded_len,
				call_weight
			),
			crate::Error::<Test>::UndecodableCall,
		);
	});
}

// This test corrupts storage on purpose, so it bypasses the `test` wrapper (whose own
// `do_try_state` would trip) and asserts the check rejects the inconsistent state.
#[test]
fn try_state_catches_unrequested_whitelisted_call() {
	new_test_ext().execute_with(|| {
		let call_hash = <Test as frame_system::Config>::Hashing::hash(&b"never requested"[..]);

		// A whitelisted call without a requested preimage is inconsistent.
		crate::WhitelistedCall::<Test>::insert(call_hash, ());
		assert!(Whitelist::do_try_state().is_err());

		// Requesting the preimage restores consistency.
		Preimage::request(&call_hash);
		assert_ok!(Whitelist::do_try_state());
	});
}

// The invariant is cross-pallet: `pallet-preimage`'s `ManagerOrigin` (root on production
// runtimes) can drop the whitelist's request via the `unrequest_preimage` extrinsic without
// touching `WhitelistedCall`, so `try_state` can start failing after a perfectly valid
// (if ill-advised) governance action, not only after a whitelist-pallet bug.
#[test]
fn try_state_broken_by_external_preimage_unrequest() {
	new_test_ext().execute_with(|| {
		let call = RuntimeCall::System(frame_system::Call::remark { remark: vec![42] });
		let call_hash = <Test as frame_system::Config>::Hashing::hash(&call.encode()[..]);

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));
		assert_ok!(Whitelist::do_try_state());

		// Root (the preimage pallet's `ManagerOrigin`) clears the request out from under the
		// whitelist pallet. The whitelist pallet is never informed.
		assert_ok!(Preimage::unrequest_preimage(RuntimeOrigin::root(), call_hash));

		assert!(!<Preimage as QueryPreimage>::is_requested(&call_hash));
		assert!(crate::WhitelistedCall::<Test>::contains_key(call_hash));
		assert!(Whitelist::do_try_state().is_err());
	});
}

// A *noted* preimage is not enough: the invariant demands *requested*. A whitelisted call whose
// preimage is merely noted is still dispatchable (`fetch` succeeds), but the note is unpinned —
// its owner may `unnote_preimage` it at any time. `is_requested` is therefore the right check.
#[test]
fn try_state_fails_for_noted_but_unrequested_preimage() {
	new_test_ext().execute_with(|| {
		let call = RuntimeCall::System(frame_system::Call::remark { remark: vec![7] });
		let encoded = call.encode();
		let call_hash = <Test as frame_system::Config>::Hashing::hash(&encoded[..]);
		// A *user*-noted preimage (with depositor) is `Unrequested`; `StorePreimage::note`
		// (depositor-less) would instead note *and* request it.
		assert_ok!(Preimage::note_preimage(RuntimeOrigin::signed(1), encoded));

		// Bypass `whitelist_call` so no request is placed.
		crate::WhitelistedCall::<Test>::insert(call_hash, ());

		// The call is fetchable, yet the state is inconsistent: nothing pins the preimage.
		assert!(<Preimage as QueryPreimage>::fetch(&call_hash, None).is_ok());
		assert!(!<Preimage as QueryPreimage>::is_requested(&call_hash));
		assert!(Whitelist::do_try_state().is_err());
	});
}

// Request counting in `pallet-preimage` keeps the invariant safe when several parties request
// the same hash: the whitelist pallet's `unrequest` on removal only decrements the count, so an
// unrelated outstanding request survives and no underflow or false positive occurs.
#[test]
fn try_state_holds_with_shared_request_count() {
	test(|| {
		let call = RuntimeCall::System(frame_system::Call::remark { remark: vec![9] });
		let call_hash = <Test as frame_system::Config>::Hashing::hash(&call.encode()[..]);

		// Someone else (e.g. another pallet) requests the hash first.
		<Preimage as QueryPreimage>::request(&call_hash);

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));
		assert_ok!(Whitelist::do_try_state());

		// Removing the whitelisting decrements the count; the external request remains.
		assert_ok!(Whitelist::remove_whitelisted_call(RuntimeOrigin::root(), call_hash));
		assert!(<Preimage as QueryPreimage>::is_requested(&call_hash));
	});
}
