// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::sync::{Arc, Weak};
use std::net::SocketAddr;
use std::collections::BTreeMap;
use futures::{future, Future, IntoFuture};
use parking_lot::Mutex;
use ethcore::filter::Filter;
use ethcore::client::{Client, BlockChainClient, BlockId, ChainNotify};
use ethkey::public_to_address;
use ethsync::SyncProvider;
use native_contracts::KeyServerSet as KeyServerSetContract;
use hash::keccak;
use bigint::hash::H256;
use util::Address;
use bytes::Bytes;
use types::all::{Error, Public, NodeAddress, NodeId};
use {NodeKeyPair};

type BoxFuture<A, B> = Box<Future<Item = A, Error = B> + Send>;

const KEY_SERVER_SET_CONTRACT_REGISTRY_NAME: &'static str = "secretstore_server_set";

/// Key server has been added to the set.
const ADDED_EVENT_NAME: &'static [u8] = &*b"KeyServerAdded(address)";
/// Key server has been removed from the set.
const REMOVED_EVENT_NAME: &'static [u8] = &*b"KeyServerRemoved(address)";
/// Migration has started.
const MIGRATION_STARTED_EVENT_NAME: &'static [u8] = &*b"MigrationStarted()";
/// Migration has completed.
const MIGRATION_COMPLETED_EVENT_NAME: &'static [u8] = &*b"MigrationCompleted()";

lazy_static! {
	static ref ADDED_EVENT_NAME_HASH: H256 = keccak(ADDED_EVENT_NAME);
	static ref REMOVED_EVENT_NAME_HASH: H256 = keccak(REMOVED_EVENT_NAME);
	static ref MIGRATION_STARTED_EVENT_NAME_HASH: H256 = keccak(MIGRATION_STARTED_EVENT_NAME);
	static ref MIGRATION_COMPLETED_EVENT_NAME_HASH: H256 = keccak(MIGRATION_COMPLETED_EVENT_NAME);
}

#[derive(Default, Debug, Clone)]
/// Key Server Set state.
pub struct KeyServerSetState {
	/// Old set of key servers.
	pub old_set: BTreeMap<NodeId, SocketAddr>,
	/// New set of key servers.
	pub new_set: BTreeMap<NodeId, SocketAddr>,
	/// Migration set of key servers.
	pub migration_set: Option<BTreeMap<NodeId, SocketAddr>>,
	/// Migration master node.
	pub migration_master: Option<NodeId>,
	/// Is migration confirmed by this node?
	pub is_migration_confirmed: bool,
}

#[derive(Debug, Clone, Copy)]
/// Key Server Set state type.
pub enum KeyServerSetStateType {
	/// No actions required.
	Idle,
	/// Migration is required.
	MigrationRequired,
	/// Migration has started.
	MigrationStarted,
}

/// Key Server Set
pub trait KeyServerSet: Send + Sync {
	/// Get server set state.
	fn state(&self) -> KeyServerSetState;
	/// Start migration.
	fn start_migration(&self);
	/// Confirm migration.
	fn confirm_migration(&self);
}

/// On-chain Key Server set implementation.
pub struct OnChainKeyServerSet {
	/// Cached on-chain contract.
	contract: Mutex<CachedContract>,
}

/// Cached on-chain Key Server set contract.
struct CachedContract {
	/// Blockchain client.
	client: Weak<Client>,
	/// Sync provider.
	sync: Weak<SyncProvider>,
	/// Contract address.
	contract_addr: Option<Address>,
	/// Current contract state.
	state: KeyServerSetState,
	/// This node key pair.
	self_key_pair: Arc<NodeKeyPair>,
}

impl KeyServerSetState {
	/// Get state type.
	pub fn state(&self) -> KeyServerSetStateType {
		if self.old_set == self.new_set {
			return KeyServerSetStateType::Idle;
		}

		if self.migration_set.is_none() {
			return KeyServerSetStateType::MigrationRequired;
		}

		KeyServerSetStateType::MigrationStarted
	}

	/// Is migration required?
	pub fn is_migration_required(&self) -> bool {
		self.old_set != self.new_set && !self.migration_set.is_some()
	}

	/// Is migration scheduled?
	pub fn is_migration_scheduled(&self) -> bool {
		self.migration_set.is_some() && self.migration_master.is_none()
	}

	/// Is migration started?
	pub fn is_migration_started(&self) -> bool {
		self.migration_set.is_some() && self.migration_master.is_some()
	}
}

impl OnChainKeyServerSet {
	pub fn new(client: &Arc<Client>, sync: &Arc<SyncProvider>, self_key_pair: Arc<NodeKeyPair>, key_servers: BTreeMap<Public, NodeAddress>) -> Result<Arc<Self>, Error> {
		let mut cached_contract = CachedContract::new(client, sync, self_key_pair, key_servers)?;
		let key_server_contract_address = client.registry_address(KEY_SERVER_SET_CONTRACT_REGISTRY_NAME.to_owned());
		// only initialize from contract if it is installed. otherwise - use default nodes
		// once the contract is installed, all default nodes are lost (if not in the contract' set)
		if key_server_contract_address.is_some() {
			cached_contract.read_from_registry(&*client, key_server_contract_address);
		}

		let key_server_set = Arc::new(OnChainKeyServerSet {
			contract: Mutex::new(cached_contract),
		});
		client.add_notify(key_server_set.clone());
		Ok(key_server_set)
	}
}

impl KeyServerSet for OnChainKeyServerSet {
	fn state(&self) -> KeyServerSetState {
		self.contract.lock().state()
	}

	fn confirm_migration(&self) {
		self.contract.lock().confirm_migration();
	}
}

impl ChainNotify for OnChainKeyServerSet {
	fn new_blocks(&self, _imported: Vec<H256>, _invalid: Vec<H256>, enacted: Vec<H256>, retracted: Vec<H256>, _sealed: Vec<H256>, _proposed: Vec<Bytes>, _duration: u64) {
		if !enacted.is_empty() || !retracted.is_empty() {
			self.contract.lock().update(enacted, retracted)
		}
	}
}

impl CachedContract {
	pub fn new(client: &Arc<Client>, sync: &Arc<SyncProvider>, self_key_pair: Arc<NodeKeyPair>, key_servers: BTreeMap<Public, NodeAddress>) -> Result<Self, Error> {
		Ok(CachedContract {
			client: Arc::downgrade(client),
			sync: Arc::downgrade(sync),
			contract_addr: None,
			state: KeyServerSetState {
				new_set: key_servers.into_iter()
					.map(|(p, addr)| {
						let addr = format!("{}:{}", addr.address, addr.port).parse()
							.map_err(|err| Error::Internal(format!("error parsing node address: {}", err)))?;
						Ok((p, addr))
					})
					.collect::<Result<BTreeMap<_, _>, Error>>()?,
				..Default::default()
			},
			self_key_pair: self_key_pair,
		})
	}

	pub fn update(&mut self, enacted: Vec<H256>, retracted: Vec<H256>) {
		if let (Some(client), Some(sync)) = (self.client.upgrade(), self.sync.upgrade()) {
			// do not update initial server set until fully synchronized
			if sync.status().is_syncing(client.queue_info()) {
				return;
			}

			let new_contract_addr = client.registry_address(KEY_SERVER_SET_CONTRACT_REGISTRY_NAME.to_owned());

			// new contract installed => read nodes set from the contract
			if self.contract_addr.as_ref() != new_contract_addr.as_ref() {
				self.read_from_registry(&*client, new_contract_addr);
				return;
			}

			// check for contract events
			let is_set_changed = self.contract_addr.is_some() && enacted.iter()
				.chain(retracted.iter())
				.any(|block_hash| !client.logs(Filter {
					from_block: BlockId::Hash(block_hash.clone()),
					to_block: BlockId::Hash(block_hash.clone()),
					address: self.contract_addr.clone().map(|a| vec![a]),
					topics: vec![
						Some(vec![*ADDED_EVENT_NAME_HASH, *REMOVED_EVENT_NAME_HASH,
							*MIGRATION_STARTED_EVENT_NAME_HASH, *MIGRATION_COMPLETED_EVENT_NAME_HASH]),
						None,
						None,
						None,
					],
					limit: Some(1),
				}).is_empty());
			// to simplify processing - just re-read the whole nodes set from the contract
			if is_set_changed {
				self.read_from_registry(&*client, new_contract_addr);
			}
		}
	}

	fn state(&self) -> KeyServerSetState {
		self.state.clone()
	}

	fn start_migration(&self) {
		// TODO
		unimplemented!()
	}

	fn confirm_migration(&self) -> bool {
		// TODO
		unimplemented!()
	}

	fn read_from_registry(&mut self, client: &Client, new_contract_address: Option<Address>) {
		let contract = new_contract_address.map(|contract_addr| {
			trace!(target: "secretstore", "Configuring for key server set contract from {}", contract_addr);

			KeyServerSetContract::new(contract_addr)
		});

		let contract = match contract {
			Some(contract) => contract,
			None => return,
		};

		let do_call = |a, d| future::done(client.call_contract(BlockId::Latest, a, d));

		let old_set = Self::read_key_server_set(&contract, &do_call, &KeyServerSetContract::get_old_key_servers,
			&KeyServerSetContract::get_old_key_server_public, &KeyServerSetContract::get_old_key_server_address);
		let migration_set = Self::read_key_server_set(&contract, &do_call, &KeyServerSetContract::get_migration_key_servers,
			&KeyServerSetContract::get_migration_key_server_public, &KeyServerSetContract::get_migration_key_server_address);
		let new_set = Self::read_key_server_set(&contract, &do_call, &KeyServerSetContract::get_new_key_servers,
			&KeyServerSetContract::get_new_key_server_public, &KeyServerSetContract::get_new_key_server_address);

		let migration_master = match migration_set.is_empty() {
			false => contract.get_migration_master(&do_call).wait()
				.map(|address| old_set.keys().chain(migration_set.keys())
					.find(|public| public_to_address(public) == address)
					.cloned())
				.map_err(|err| { trace!(target: "secretstore", "Error {} reading migration master from contract", err); err })
				.unwrap_or_default(),
			true => None,
		};

		let is_migration_confirmed = match migration_set.is_empty() {
			false if old_set.contains_key(self.self_key_pair.public()) || migration_set.contains_key(self.self_key_pair.public()) =>
				contract.is_migration_confirmed(&do_call, self.self_key_pair.address()).wait()
					.map_err(|err| { trace!(target: "secretstore", "Error {} reading migration confirmation from contract", err); err })
					.unwrap_or(true),
			_ => false,
		};

		self.state = KeyServerSetState {
			old_set: old_set,
			new_set: new_set,
			migration_set: if migration_set.is_empty() { None } else { Some(migration_set) },
			migration_master: migration_master,
			is_migration_confirmed: is_migration_confirmed,
		};
		self.contract_addr = new_contract_address;
	}

	fn read_key_server_set<F, U, FL, FP, FA>(contract: &KeyServerSetContract, do_call: F, read_list: FL, read_public: FP, read_address: FA) -> BTreeMap<Public, SocketAddr>
		where
			F: FnOnce(Address, Vec<u8>) -> U + Copy,
			U: IntoFuture<Item=Vec<u8>, Error=String>,
			U::Future: Send + 'static,
			FL: Fn(&KeyServerSetContract, F) -> BoxFuture<Vec<Address>, String>,
			FP: Fn(&KeyServerSetContract, F, Address) -> BoxFuture<Vec<u8>, String>,
			FA: Fn(&KeyServerSetContract, F, Address) -> BoxFuture<String, String> {
		let mut key_servers = BTreeMap::new();
		let key_servers_list = read_list(contract, do_call).wait()
			.map_err(|err| { trace!(target: "secretstore", "Error {} reading list of key servers from contract", err); err })
			.unwrap_or_default();
		for key_server in key_servers_list {
			let key_server_public = read_public(contract, do_call, key_server).wait()
				.and_then(|p| if p.len() == 64 { Ok(Public::from_slice(&p)) } else { Err(format!("Invalid public length {}", p.len())) });
			let key_server_address = read_address(contract, do_call, key_server).wait()
				.and_then(|a| a.parse().map_err(|e| format!("Invalid ip address: {}", e)));

			// only add successfully parsed nodes
			match (key_server_public, key_server_address) {
				(Ok(key_server_public), Ok(key_server_address)) => { key_servers.insert(key_server_public, key_server_address); },
				(Err(public_err), _) => warn!(target: "secretstore_net", "received invalid public from key server set contract: {}", public_err),
				(_, Err(ip_err)) => warn!(target: "secretstore_net", "received invalid IP from key server set contract: {}", ip_err),
			}
		}
		key_servers
	}
}

#[cfg(test)]
pub mod tests {
	use std::collections::BTreeMap;
	use std::net::SocketAddr;
	use ethkey::Public;
	use super::{KeyServerSet, KeyServerSetState};

	#[derive(Default)]
	pub struct MapKeyServerSet {
		nodes: BTreeMap<Public, SocketAddr>,
	}

	impl MapKeyServerSet {
		pub fn new(nodes: BTreeMap<Public, SocketAddr>) -> Self {
			MapKeyServerSet {
				nodes: nodes,
			}
		}
	}

	impl KeyServerSet for MapKeyServerSet {
		fn state(&self) -> KeyServerSetState {
			KeyServerSetState {
				new_set: self.nodes.clone(),
				..Default::default()
			}
		}

		fn start_migration(&self) {
			// TODO
			unimplemented!()
		}

		fn confirm_migration(&self) {
			unimplemented!()
		}
	}
}
