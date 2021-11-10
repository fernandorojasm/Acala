// This file is part of Acala.

// Copyright (C) 2020-2021 Acala Foundation.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! # CDP Treasury Module
//!
//! ## Overview
//!
//! CDP Treasury manages the accumulated interest and bad debts generated by
//! CDPs, and handle excessive surplus or debits timely in order to keep the
//! system healthy with low risk. It's the only entry for issuing/burning stable
//! coin for whole system.

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::unused_unit)]

use frame_support::{log, pallet_prelude::*, transactional, PalletId};
use frame_system::pallet_prelude::*;
use orml_traits::{MultiCurrency, MultiCurrencyExtended};
use primitives::{Balance, CurrencyId};
use sp_runtime::{
	traits::{AccountIdConversion, One, Zero},
	ArithmeticError, DispatchError, DispatchResult, FixedPointNumber,
};
use support::{AuctionManager, CDPTreasury, CDPTreasuryExtended, DEXManager, Ratio};

mod mock;
mod tests;
pub mod weights;

pub use module::*;
pub use weights::WeightInfo;

#[frame_support::pallet]
pub mod module {
	use super::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// The origin which may update parameters and handle
		/// surplus/collateral.
		type UpdateOrigin: EnsureOrigin<Self::Origin>;

		/// The Currency for managing assets related to CDP
		type Currency: MultiCurrencyExtended<Self::AccountId, CurrencyId = CurrencyId, Balance = Balance>;

		/// Stablecoin currency id
		#[pallet::constant]
		type GetStableCurrencyId: Get<CurrencyId>;

		/// Auction manager creates auction to handle system surplus and debit
		type AuctionManagerHandler: AuctionManager<Self::AccountId, CurrencyId = CurrencyId, Balance = Balance>;

		/// Dex manager is used to swap confiscated collateral assets to stable
		/// currency
		type DEX: DEXManager<Self::AccountId, CurrencyId, Balance>;

		/// The cap of lots number when create collateral auction on a
		/// liquidation or to create debit/surplus auction on block end.
		/// If set to 0, does not work.
		#[pallet::constant]
		type MaxAuctionsCount: Get<u32>;

		#[pallet::constant]
		type TreasuryAccount: Get<Self::AccountId>;

		/// The CDP treasury's module id, keep surplus and collateral assets
		/// from liquidation.
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// Weight information for the extrinsics in this module.
		type WeightInfo: WeightInfo;
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The collateral amount of CDP treasury is not enough
		CollateralNotEnough,
		/// The surplus pool of CDP treasury is not enough
		SurplusPoolNotEnough,
		/// The debit pool of CDP treasury is not enough
		DebitPoolNotEnough,
		/// The swap path is invalid
		InvalidSwapPath,
	}

	#[pallet::event]
	#[pallet::generate_deposit(fn deposit_event)]
	pub enum Event<T: Config> {
		/// The expected amount size for per lot collateral auction of specific
		/// collateral type updated. \[collateral_type, new_size\]
		ExpectedCollateralAuctionSizeUpdated(CurrencyId, Balance),
	}

	/// The expected amount size for per lot collateral auction of specific
	/// collateral type.
	///
	/// ExpectedCollateralAuctionSize: map CurrencyId => Balance
	#[pallet::storage]
	#[pallet::getter(fn expected_collateral_auction_size)]
	pub type ExpectedCollateralAuctionSize<T: Config> = StorageMap<_, Twox64Concat, CurrencyId, Balance, ValueQuery>;

	/// Current total debit value of system. It's not same as debit in CDP
	/// engine, it is the bad debt of the system.
	///
	/// DebitPool: Balance
	#[pallet::storage]
	#[pallet::getter(fn debit_pool)]
	pub type DebitPool<T: Config> = StorageValue<_, Balance, ValueQuery>;

	#[pallet::genesis_config]
	#[cfg_attr(feature = "std", derive(Default))]
	pub struct GenesisConfig {
		pub expected_collateral_auction_size: Vec<(CurrencyId, Balance)>,
	}

	#[pallet::genesis_build]
	impl<T: Config> GenesisBuild<T> for GenesisConfig {
		fn build(&self) {
			self.expected_collateral_auction_size
				.iter()
				.for_each(|(currency_id, size)| {
					ExpectedCollateralAuctionSize::<T>::insert(currency_id, size);
				});
		}
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::hooks]
	impl<T: Config> Hooks<T::BlockNumber> for Pallet<T> {
		/// Handle excessive surplus or debits of system when block end
		fn on_finalize(_now: T::BlockNumber) {
			// offset the same amount between debit pool and surplus pool
			Self::offset_surplus_and_debit();
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::weight(T::WeightInfo::extract_surplus_to_treasury())]
		#[transactional]
		pub fn extract_surplus_to_treasury(origin: OriginFor<T>, #[pallet::compact] amount: Balance) -> DispatchResult {
			T::UpdateOrigin::ensure_origin(origin)?;
			T::Currency::transfer(
				T::GetStableCurrencyId::get(),
				&Self::account_id(),
				&T::TreasuryAccount::get(),
				amount,
			)?;
			Ok(())
		}

		#[pallet::weight(T::WeightInfo::auction_collateral())]
		#[transactional]
		pub fn auction_collateral(
			origin: OriginFor<T>,
			currency_id: CurrencyId,
			#[pallet::compact] amount: Balance,
			#[pallet::compact] target: Balance,
			splited: bool,
		) -> DispatchResult {
			T::UpdateOrigin::ensure_origin(origin)?;
			<Self as CDPTreasuryExtended<T::AccountId>>::create_collateral_auctions(
				currency_id,
				amount,
				target,
				Self::account_id(),
				splited,
			)?;
			Ok(())
		}

		/// Update parameters related to collateral auction under specific
		/// collateral type
		///
		/// The dispatch origin of this call must be `UpdateOrigin`.
		///
		/// - `currency_id`: collateral type
		/// - `amount`: expected size of per lot collateral auction
		#[pallet::weight((T::WeightInfo::set_expected_collateral_auction_size(), DispatchClass::Operational))]
		#[transactional]
		pub fn set_expected_collateral_auction_size(
			origin: OriginFor<T>,
			currency_id: CurrencyId,
			#[pallet::compact] size: Balance,
		) -> DispatchResult {
			T::UpdateOrigin::ensure_origin(origin)?;
			ExpectedCollateralAuctionSize::<T>::insert(currency_id, size);
			Self::deposit_event(Event::ExpectedCollateralAuctionSizeUpdated(currency_id, size));
			Ok(())
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Get account of cdp treasury module.
	pub fn account_id() -> T::AccountId {
		T::PalletId::get().into_account()
	}

	/// Get current total surplus of system.
	pub fn surplus_pool() -> Balance {
		T::Currency::free_balance(T::GetStableCurrencyId::get(), &Self::account_id())
	}

	/// Get total collateral amount of cdp treasury module.
	pub fn total_collaterals(currency_id: CurrencyId) -> Balance {
		T::Currency::free_balance(currency_id, &Self::account_id())
	}

	/// Get collateral amount not in auction
	pub fn total_collaterals_not_in_auction(currency_id: CurrencyId) -> Balance {
		T::Currency::free_balance(currency_id, &Self::account_id())
			.saturating_sub(T::AuctionManagerHandler::get_total_collateral_in_auction(currency_id))
	}

	fn offset_surplus_and_debit() {
		let offset_amount = sp_std::cmp::min(Self::debit_pool(), Self::surplus_pool());

		// Burn the amount that is equal to offset amount of stable currency.
		if !offset_amount.is_zero() {
			let res = T::Currency::withdraw(T::GetStableCurrencyId::get(), &Self::account_id(), offset_amount);
			match res {
				Ok(_) => {
					DebitPool::<T>::mutate(|debit| {
						*debit = debit
							.checked_sub(offset_amount)
							.expect("offset = min(debit, surplus); qed")
					});
				}
				Err(e) => {
					log::warn!(
						target: "cdp-treasury",
						"get_swap_supply_amount: Attempt to burn surplus {:?} failed: {:?}, this is unexpected but should be safe",
						offset_amount, e
					);
				}
			}
		}
	}
}

impl<T: Config> CDPTreasury<T::AccountId> for Pallet<T> {
	type Balance = Balance;
	type CurrencyId = CurrencyId;

	fn get_surplus_pool() -> Self::Balance {
		Self::surplus_pool()
	}

	fn get_debit_pool() -> Self::Balance {
		Self::debit_pool()
	}

	fn get_total_collaterals(id: Self::CurrencyId) -> Self::Balance {
		Self::total_collaterals(id)
	}

	fn get_debit_proportion(amount: Self::Balance) -> Ratio {
		let stable_total_supply = T::Currency::total_issuance(T::GetStableCurrencyId::get());
		Ratio::checked_from_rational(amount, stable_total_supply).unwrap_or_default()
	}

	fn on_system_debit(amount: Self::Balance) -> DispatchResult {
		DebitPool::<T>::try_mutate(|debit_pool| -> DispatchResult {
			*debit_pool = debit_pool.checked_add(amount).ok_or(ArithmeticError::Overflow)?;
			Ok(())
		})
	}

	fn on_system_surplus(amount: Self::Balance) -> DispatchResult {
		Self::issue_debit(&Self::account_id(), amount, true)
	}

	fn issue_debit(who: &T::AccountId, debit: Self::Balance, backed: bool) -> DispatchResult {
		// increase system debit if the debit is unbacked
		if !backed {
			Self::on_system_debit(debit)?;
		}
		T::Currency::deposit(T::GetStableCurrencyId::get(), who, debit)?;

		Ok(())
	}

	fn burn_debit(who: &T::AccountId, debit: Self::Balance) -> DispatchResult {
		T::Currency::withdraw(T::GetStableCurrencyId::get(), who, debit)
	}

	fn deposit_surplus(from: &T::AccountId, surplus: Self::Balance) -> DispatchResult {
		T::Currency::transfer(T::GetStableCurrencyId::get(), from, &Self::account_id(), surplus)
	}

	fn deposit_collateral(from: &T::AccountId, currency_id: Self::CurrencyId, amount: Self::Balance) -> DispatchResult {
		T::Currency::transfer(currency_id, from, &Self::account_id(), amount)
	}

	fn withdraw_collateral(to: &T::AccountId, currency_id: Self::CurrencyId, amount: Self::Balance) -> DispatchResult {
		T::Currency::transfer(currency_id, &Self::account_id(), to, amount)
	}
}

impl<T: Config> CDPTreasuryExtended<T::AccountId> for Pallet<T> {
	/// Swap exact amount of collateral stable,
	/// return actual target stable amount
	fn swap_exact_collateral_to_stable(
		currency_id: CurrencyId,
		supply_amount: Balance,
		min_target_amount: Balance,
		swap_path: &[CurrencyId],
		collateral_in_auction: bool,
	) -> sp_std::result::Result<Balance, DispatchError> {
		if collateral_in_auction {
			ensure!(
				Self::total_collaterals(currency_id) >= supply_amount
					&& T::AuctionManagerHandler::get_total_collateral_in_auction(currency_id) >= supply_amount,
				Error::<T>::CollateralNotEnough,
			);
		} else {
			ensure!(
				Self::total_collaterals_not_in_auction(currency_id) >= supply_amount,
				Error::<T>::CollateralNotEnough,
			);
		}

		let swap_path_length = swap_path.len();
		ensure!(
			swap_path_length >= 2
				&& swap_path[0] == currency_id
				&& swap_path[swap_path_length - 1] == T::GetStableCurrencyId::get(),
			Error::<T>::InvalidSwapPath
		);

		T::DEX::swap_with_exact_supply(&Self::account_id(), swap_path, supply_amount, min_target_amount)
	}

	/// swap collateral which not in auction to get exact stable,
	/// return actual supply collateral amount
	fn swap_collateral_to_exact_stable(
		currency_id: CurrencyId,
		max_supply_amount: Balance,
		target_amount: Balance,
		swap_path: &[CurrencyId],
		collateral_in_auction: bool,
	) -> sp_std::result::Result<Balance, DispatchError> {
		if collateral_in_auction {
			ensure!(
				Self::total_collaterals(currency_id) >= max_supply_amount
					&& T::AuctionManagerHandler::get_total_collateral_in_auction(currency_id) >= max_supply_amount,
				Error::<T>::CollateralNotEnough,
			);
		} else {
			ensure!(
				Self::total_collaterals_not_in_auction(currency_id) >= max_supply_amount,
				Error::<T>::CollateralNotEnough,
			);
		}

		let swap_path_length = swap_path.len();
		ensure!(
			swap_path_length >= 2
				&& swap_path[0] == currency_id
				&& swap_path[swap_path_length - 1] == T::GetStableCurrencyId::get(),
			Error::<T>::InvalidSwapPath
		);

		T::DEX::swap_with_exact_target(&Self::account_id(), swap_path, target_amount, max_supply_amount)
	}

	fn create_collateral_auctions(
		currency_id: CurrencyId,
		amount: Balance,
		target: Balance,
		refund_receiver: T::AccountId,
		splited: bool,
	) -> DispatchResult {
		ensure!(
			Self::total_collaterals_not_in_auction(currency_id) >= amount,
			Error::<T>::CollateralNotEnough,
		);

		let mut unhandled_collateral_amount = amount;
		let mut unhandled_target = target;
		let expected_collateral_auction_size = Self::expected_collateral_auction_size(currency_id);
		let max_auctions_count: Balance = T::MaxAuctionsCount::get().into();
		let lots_count = if !splited
			|| max_auctions_count.is_zero()
			|| expected_collateral_auction_size.is_zero()
			|| amount <= expected_collateral_auction_size
		{
			One::one()
		} else {
			let mut count = amount
				.checked_div(expected_collateral_auction_size)
				.expect("collateral auction maximum size is not zero; qed");

			let remainder = amount
				.checked_rem(expected_collateral_auction_size)
				.expect("collateral auction maximum size is not zero; qed");
			if !remainder.is_zero() {
				count = count.saturating_add(One::one());
			}
			sp_std::cmp::min(count, max_auctions_count)
		};
		let average_amount_per_lot = amount.checked_div(lots_count).expect("lots count is at least 1; qed");
		let average_target_per_lot = target.checked_div(lots_count).expect("lots count is at least 1; qed");
		let mut created_lots: Balance = Zero::zero();

		while !unhandled_collateral_amount.is_zero() {
			created_lots = created_lots.saturating_add(One::one());
			let (lot_collateral_amount, lot_target) = if created_lots == lots_count {
				// the last lot may be have some remnant than average
				(unhandled_collateral_amount, unhandled_target)
			} else {
				(average_amount_per_lot, average_target_per_lot)
			};

			T::AuctionManagerHandler::new_collateral_auction(
				&refund_receiver,
				currency_id,
				lot_collateral_amount,
				lot_target,
			)?;

			unhandled_collateral_amount = unhandled_collateral_amount.saturating_sub(lot_collateral_amount);
			unhandled_target = unhandled_target.saturating_sub(lot_target);
		}
		Ok(())
	}
}

#[cfg(feature = "std")]
impl GenesisConfig {
	/// Direct implementation of `GenesisBuild::build_storage`.
	///
	/// Kept in order not to break dependency.
	pub fn build_storage<T: Config>(&self) -> Result<sp_runtime::Storage, String> {
		<Self as GenesisBuild<T>>::build_storage(self)
	}

	/// Direct implementation of `GenesisBuild::assimilate_storage`.
	///
	/// Kept in order not to break dependency.
	pub fn assimilate_storage<T: Config>(&self, storage: &mut sp_runtime::Storage) -> Result<(), String> {
		<Self as GenesisBuild<T>>::assimilate_storage(self, storage)
	}
}
