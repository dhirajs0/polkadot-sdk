// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
//! Benchmarking setup for pallet-template
use super::*;

#[allow(unused)]
use crate::Pallet as SnowbridgeControl;
use frame_benchmarking::v2::*;
use frame_system::RawOrigin;
use xcm::prelude::*;

#[benchmarks]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn register_token() -> Result<(), BenchmarkError> {
		let origin_location = Location::new(1, [Parachain(1000), PalletInstance(36)]);
		let origin = <T as Config>::Helper::make_xcm_origin(origin_location.clone());
		let creator = Box::new(VersionedLocation::from(origin_location.clone()));
		let relay_token_asset_id: Location = Location::parent();
		let asset = Box::new(VersionedLocation::from(relay_token_asset_id));
		let asset_metadata = AssetMetadata {
			name: "wnd".as_bytes().to_vec().try_into().unwrap(),
			symbol: "wnd".as_bytes().to_vec().try_into().unwrap(),
			decimals: 12,
		};

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, creator, asset, asset_metadata, 1);

		Ok(())
	}

	#[benchmark]
	fn upgrade() -> Result<(), BenchmarkError> {
		let impl_address = H160::repeat_byte(1);
		let impl_code_hash = H256::repeat_byte(1);

		// Assume 256 bytes passed to initializer
		let params: Vec<u8> = (0..256).map(|_| 1u8).collect();

		#[extrinsic_call]
		_(
			RawOrigin::Root,
			impl_address,
			impl_code_hash,
			Initializer { params, maximum_required_gas: 100000 },
		);

		Ok(())
	}

	#[benchmark]
	fn set_operating_mode() -> Result<(), BenchmarkError> {
		#[extrinsic_call]
		_(RawOrigin::Root, OperatingMode::RejectingOutboundMessages);

		Ok(())
	}

	#[benchmark]
	fn add_tip() -> Result<(), BenchmarkError> {
		let origin_location = Location::new(1, [Parachain(1000), PalletInstance(36)]);
		let origin = <T as Config>::Helper::make_xcm_origin(origin_location.clone());
		let sender: T::AccountId = whitelisted_caller();
		let message_id = MessageId::Inbound(1);

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, sender, message_id, 1_000_000_000u128);

		Ok(())
	}

	impl_benchmark_test_suite!(
		SnowbridgeControl,
		crate::mock::new_test_ext(true),
		crate::mock::Test
	);
}
