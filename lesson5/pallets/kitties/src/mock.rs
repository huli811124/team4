// Creating mock runtime here

use crate::{Module, Trait};
use sp_core::H256;
use frame_support::{
	impl_outer_origin, parameter_types,
	weights::Weight, impl_outer_event,
};
use sp_io::TestExternalities;
use sp_runtime::{
	traits::{BlakeTwo256, IdentityLookup}, testing::Header, Perbill,
};
use frame_system as system;

impl_outer_origin! {
	pub enum Origin for Test {}
}

// For testing the pallet, we construct most of a mock runtime. This means
// first constructing a configuration type (`Test`) which `impl`s each of the
// configuration traits of pallets we want to use.
#[derive(Clone, Eq, PartialEq)]
pub struct Test;
parameter_types! {
	pub const BlockHashCount: u64 = 250;
	pub const MaximumBlockWeight: Weight = 1024;
	pub const MaximumBlockLength: u32 = 2 * 1024;
	pub const AvailableBlockRatio: Perbill = Perbill::from_percent(75);
}
impl system::Trait for Test {
	type Origin = Origin;
	type Call = ();
	type Index = u64;
	type BlockNumber = u64;
	type Hash = H256;
	type Hashing = BlakeTwo256;
	type AccountId = u64;
	type Lookup = IdentityLookup<Self::AccountId>;
	type Header = Header;
	type Event = TestEvent;
	type BlockHashCount = BlockHashCount;
	type MaximumBlockWeight = MaximumBlockWeight;
	type DbWeight = ();
	type BlockExecutionWeight = ();
	type ExtrinsicBaseWeight = ();
	type MaximumExtrinsicWeight = MaximumBlockWeight;
	type MaximumBlockLength = MaximumBlockLength;
	type AvailableBlockRatio = AvailableBlockRatio;
	type Version = ();
	type ModuleToIndex = ();
	type AccountData = balances::AccountData<u64>;
	type OnNewAccount = ();
	type OnKilledAccount = ();
}

mod kitty_event {
	pub use crate::Event;
}

impl_outer_event! {
	pub enum TestEvent for Test {
		kitty_event<T>,
		balances<T>,
		system<T>,
	}
}

parameter_types! {
	pub const ExistentialDeposit: u64 = 1;
}

impl balances::Trait for Test {
	type Balance = u64;
	type DustRemoval = ();
	type Event = TestEvent;
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
}

impl Trait for Test {
}

pub type System = system::Module<Test>;
pub type _Balances = balances::Module<Test>;
pub type Kitty = Module<Test>;

pub struct ExtBuilder;

impl ExtBuilder {
	pub fn build() -> TestExternalities {
		let mut storage = system::GenesisConfig::default()
			.build_storage::<Test>()
			.unwrap();

		balances::GenesisConfig::<Test> {
			balances: vec![(1, 4000), (2, 4000), (3, 4000), (4, 4000)],
		}
		.assimilate_storage(&mut storage).unwrap();

		let mut ext = TestExternalities::from(storage);
		ext.execute_with(|| System::set_block_number(1));
		ext
	}
}