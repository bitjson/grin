// Copyright 2016 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use rand::{thread_rng, Rng};

use core::core;
use core::core::hash::{Hash, Hashed};
use core::core::target::Difficulty;
use util::LOGGER;
use time;

use peer::Peer;
use store::{PeerData, PeerStore, State};
use types::*;

#[derive(Clone)]
pub struct Peers {
	pub adapter: Arc<ChainAdapter>,
	store: Arc<PeerStore>,
	peers: Arc<RwLock<HashMap<SocketAddr, Arc<RwLock<Peer>>>>>,
	config: P2PConfig,
}

unsafe impl Send for Peers {}
unsafe impl Sync for Peers {}

impl Peers {
	pub fn new(store: PeerStore, adapter: Arc<ChainAdapter>, config: P2PConfig) -> Peers {
		Peers {
			adapter,
			store: Arc::new(store),
			peers: Arc::new(RwLock::new(HashMap::new())),
			config,
		}
	}

	/// Adds the peer to our internal peer mapping. Note that the peer is still
	/// returned so the server can run it.
	pub fn add_connected(&self, p: Peer) -> Arc<RwLock<Peer>> {
		debug!(LOGGER, "Saving newly connected peer {}.", p.info.addr);
		let peer_data = PeerData {
			addr: p.info.addr,
			capabilities: p.info.capabilities,
			user_agent: p.info.user_agent.clone(),
			flags: State::Healthy,
			last_banned: 0,
		};
		if let Err(e) = self.save_peer(&peer_data) {
			error!(LOGGER, "Could not save connected peer: {:?}", e);
		}

		let addr = p.info.addr.clone();
		let apeer = Arc::new(RwLock::new(p));
		{
			let mut peers = self.peers.write().unwrap();
			peers.insert(addr, apeer.clone());
		}
		apeer.clone()
	}

	pub fn is_known(&self, addr: &SocketAddr) -> bool {
		self.get_connected_peer(addr).is_some()
	}

	/// Get vec of peers we are currently connected to.
	pub fn connected_peers(&self) -> Vec<Arc<RwLock<Peer>>> {
		let mut res = self.peers
			.read()
			.unwrap()
			.values()
			.cloned()
			.collect::<Vec<_>>();
		thread_rng().shuffle(&mut res);
		res
	}

	/// Get a peer we're connected to by address.
	pub fn get_connected_peer(&self, addr: &SocketAddr) -> Option<Arc<RwLock<Peer>>> {
		self.peers.read().unwrap().get(addr).map(|p| p.clone())
	}

	/// Number of peers we're currently connected to.
	pub fn peer_count(&self) -> u32 {
		self.connected_peers().len() as u32
	}

	// Return vec of connected peers that currently advertise more work
	// (total_difficulty) than we do.
	pub fn more_work_peers(&self) -> Vec<Arc<RwLock<Peer>>> {
		let peers = self.connected_peers();
		if peers.len() == 0 {
			return vec![];
		}

		let total_difficulty = self.total_difficulty();

		let mut max_peers = peers
			.iter()
			.filter(|x| match x.try_read() {
				Ok(peer) => peer.info.total_difficulty > total_difficulty,
				Err(_) => false,
			})
			.cloned()
			.collect::<Vec<_>>();

		thread_rng().shuffle(&mut max_peers);
		max_peers
	}

	/// Returns single random peer with more work than us.
	pub fn more_work_peer(&self) -> Option<Arc<RwLock<Peer>>> {
		match self.more_work_peers().first() {
			Some(x) => Some(x.clone()),
			None => None,
		}
	}

	/// Return vec of connected peers that currently have the most worked branch,
	/// showing the highest total difficulty.
	pub fn most_work_peers(&self) -> Vec<Arc<RwLock<Peer>>> {
		let peers = self.connected_peers();
		if peers.len() == 0 {
			return vec![];
		}

		let max_total_difficulty = peers
			.iter()
			.map(|x| match x.try_read() {
				Ok(peer) => peer.info.total_difficulty.clone(),
				Err(_) => Difficulty::zero(),
			})
			.max()
			.unwrap();

		let mut max_peers = peers
			.iter()
			.filter(|x| match x.try_read() {
				Ok(peer) => peer.info.total_difficulty == max_total_difficulty,
				Err(_) => false,
			})
			.cloned()
			.collect::<Vec<_>>();

		thread_rng().shuffle(&mut max_peers);
		max_peers
	}

	/// Returns single random peer with the most worked branch, showing the highest total
	/// difficulty.
	pub fn most_work_peer(&self) -> Option<Arc<RwLock<Peer>>> {
		match self.most_work_peers().first() {
			Some(x) => Some(x.clone()),
			None => None,
		}
	}

	pub fn is_banned(&self, peer_addr: SocketAddr) -> bool {
		if let Ok(peer_data) = self.store.get_peer(peer_addr) {
			if peer_data.flags == State::Banned {
				return true;
			}
		}
		false
	}

	/// Bans a peer, disconnecting it if we're currently connected
	pub fn ban_peer(&self, peer_addr: &SocketAddr) {
		if let Err(e) = self.update_state(peer_addr.clone(), State::Banned) {
			error!(LOGGER, "Couldn't ban {}: {:?}", peer_addr, e);
		}

		if let Err(e) =
			self.update_last_banned(peer_addr.clone(), time::now_utc().to_timespec().sec)
		{
			error!(
				LOGGER,
				"Couldn't update last_banned time {}: {:?}", peer_addr, e
			);
		}

		if let Some(peer) = self.get_connected_peer(peer_addr) {
			debug!(LOGGER, "Banning peer {}", peer_addr);
			// setting peer status will get it removed at the next clean_peer
			let peer = peer.write().unwrap();
			peer.set_banned();
			peer.stop();
		}
	}

	/// Unbans a peer, checks if it exists and banned then unban
	pub fn unban_peer(&self, peer_addr: &SocketAddr) {
		match self.get_peer(peer_addr.clone()) {
			Ok(_) => {
				if self.is_banned(peer_addr.clone()) {
					if let Err(e) = self.update_state(peer_addr.clone(), State::Healthy) {
						error!(LOGGER, "Couldn't unban {}: {:?}", peer_addr, e)
					}
				} else {
					error!(LOGGER, "Couldn't unban {}: peer is not banned", peer_addr)
				}
			}
			Err(e) => error!(LOGGER, "Couldn't unban {}: {:?}", peer_addr, e),
		};
	}

	/// Broadcasts the provided block to PEER_PREFERRED_COUNT of our peers.
	/// We may be connected to PEER_MAX_COUNT peers so we only
	/// want to broadcast to a random subset of peers.
	/// A peer implementation may drop the broadcast request
	/// if it knows the remote peer already has the block.
	pub fn broadcast_block(&self, b: &core::Block) {
		let peers = self.connected_peers();
		let preferred_peers = 8;
		let mut count = 0;
		for p in peers.iter().take(preferred_peers) {
			let p = p.read().unwrap();
			if p.is_connected() {
				if let Err(e) = p.send_block(b) {
					debug!(LOGGER, "Error sending block to peer: {:?}", e);
				} else {
					count += 1;
				}
			}
		}
		debug!(
			LOGGER,
			"broadcast_block: {}, {} at {}, to {} peers, done.",
			b.hash(),
			b.header.total_difficulty,
			b.header.height,
			count,
		);
	}

	pub fn broadcast_compact_block(&self, b: &core::CompactBlock) {
		let peers = self.connected_peers();
		let preferred_peers = 8;
		let mut count = 0;
		for p in peers.iter().take(preferred_peers) {
			let p = p.read().unwrap();
			if p.is_connected() {
				if let Err(e) = p.send_compact_block(b) {
					debug!(LOGGER, "Error sending compact block to peer: {:?}", e);
				} else {
					count += 1;
				}
			}
		}
		debug!(
			LOGGER,
			"broadcast_compact_block: {}, {} at {}, to {} peers, done.",
			b.hash(),
			b.header.total_difficulty,
			b.header.height,
			count,
		);
	}

	/// Broadcasts the provided block to PEER_PREFERRED_COUNT of our peers.
	/// We may be connected to PEER_MAX_COUNT peers so we only
	/// want to broadcast to a random subset of peers.
	/// A peer implementation may drop the broadcast request
	/// if it knows the remote peer already has the block.
	pub fn broadcast_header(&self, bh: &core::BlockHeader) {
		let peers = self.connected_peers();
		let preferred_peers = 8;
		let mut count = 0;
		for p in peers.iter().take(preferred_peers) {
			let p = p.read().unwrap();
			if p.is_connected() {
				if let Err(e) = p.send_header(bh) {
					debug!(LOGGER, "Error sending header to peer: {:?}", e);
				} else {
					count += 1;
				}
			}
		}
		debug!(
			LOGGER,
			"broadcast_header: {}, {} at {}, to {} peers, done.",
			bh.hash(),
			bh.total_difficulty,
			bh.height,
			count,
		);
	}

	/// Broadcasts the provided transaction to PEER_PREFERRED_COUNT of our peers.
	/// We may be connected to PEER_MAX_COUNT peers so we only
	/// want to broadcast to a random subset of peers.
	/// A peer implementation may drop the broadcast request
	/// if it knows the remote peer already has the transaction.
	pub fn broadcast_transaction(&self, tx: &core::Transaction) {
		let peers = self.connected_peers();
		for p in peers.iter().take(8) {
			let p = p.read().unwrap();
			if p.is_connected() {
				if let Err(e) = p.send_transaction(tx) {
					debug!(LOGGER, "Error sending block to peer: {:?}", e);
				}
			}
		}
	}

	/// Ping all our connected peers. Always automatically expects a pong back or
	/// disconnects. This acts as a liveness test.
	pub fn check_all(&self, total_difficulty: Difficulty, height: u64) {
		let peers_map = self.peers.read().unwrap();
		for p in peers_map.values() {
			let p = p.read().unwrap();
			if p.is_connected() {
				let _ = p.send_ping(total_difficulty.clone(), height);
			}
		}
	}

	/// All peer information we have in storage
	pub fn all_peers(&self) -> Vec<PeerData> {
		self.store.all_peers()
	}

	/// Find peers in store (not necessarily connected) and return their data
	pub fn find_peers(&self, state: State, cap: Capabilities, count: usize) -> Vec<PeerData> {
		self.store.find_peers(state, cap, count)
	}

	/// Get peer in store by address
	pub fn get_peer(&self, peer_addr: SocketAddr) -> Result<PeerData, Error> {
		self.store.get_peer(peer_addr).map_err(From::from)
	}

	/// Whether we've already seen a peer with the provided address
	pub fn exists_peer(&self, peer_addr: SocketAddr) -> Result<bool, Error> {
		self.store.exists_peer(peer_addr).map_err(From::from)
	}

	/// Saves updated information about a peer
	pub fn save_peer(&self, p: &PeerData) -> Result<(), Error> {
		self.store.save_peer(p).map_err(From::from)
	}

	/// Updates the state of a peer in store
	pub fn update_state(&self, peer_addr: SocketAddr, new_state: State) -> Result<(), Error> {
		self.store
			.update_state(peer_addr, new_state)
			.map_err(From::from)
	}

	/// Updates the last banned time of a peer in store
	pub fn update_last_banned(&self, peer_addr: SocketAddr, last_banned: i64) -> Result<(), Error> {
		self.store
			.update_last_banned(peer_addr, last_banned)
			.map_err(From::from)
	}

	/// Iterate over the peer list and prune all peers we have
	/// lost connection to or have been deemed problematic.
	/// Also avoid connected peer count getting too high.
	pub fn clean_peers(&self, max_count: usize) {
		let mut rm = vec![];

		// build a list of peers to be cleaned up
		for peer in self.connected_peers() {
			let peer_inner = peer.read().unwrap();
			if peer_inner.is_banned() {
				debug!(LOGGER, "cleaning {:?}, peer banned", peer_inner.info.addr);
				rm.push(peer.clone());
			} else if !peer_inner.is_connected() {
				debug!(LOGGER, "cleaning {:?}, not connected", peer_inner.info.addr);
				rm.push(peer.clone());
			}
		}

		// now clean up peer map based on the list to remove
		{
			let mut peers = self.peers.write().unwrap();
			for p in rm.clone() {
				let p = p.read().unwrap();
				peers.remove(&p.info.addr);
			}
		}

		// ensure we do not have too many connected peers
		let excess_count = {
			let peer_count = self.peer_count().clone() as usize;
			if peer_count > max_count {
				peer_count - max_count
			} else {
				0
			}
		};

		// map peers to addrs in a block to bound how long we keep the read lock for
		let addrs = {
			self.connected_peers()
				.iter()
				.map(|x| {
					let p = x.read().unwrap();
					p.info.addr.clone()
				})
				.collect::<Vec<_>>()
		};

		// now remove them taking a short-lived write lock each time
		// maybe better to take write lock once and remove them all?
		for x in addrs.iter().take(excess_count) {
			let mut peers = self.peers.write().unwrap();
			peers.remove(x);
		}
	}

	pub fn stop(self) {
		let peers = self.connected_peers();
		for peer in peers {
			let peer = peer.read().unwrap();
			peer.stop();
		}
	}
}

impl ChainAdapter for Peers {
	fn total_difficulty(&self) -> Difficulty {
		self.adapter.total_difficulty()
	}
	fn total_height(&self) -> u64 {
		self.adapter.total_height()
	}
	fn transaction_received(&self, tx: core::Transaction) {
		self.adapter.transaction_received(tx)
	}
	fn block_received(&self, b: core::Block, peer_addr: SocketAddr) -> bool {
		if !self.adapter.block_received(b, peer_addr) {
			// if the peer sent us a block that's intrinsically bad
			// they are either mistaken or manevolent, both of which require a ban
			self.ban_peer(&peer_addr);
			false
		} else {
			true
		}
	}
	fn compact_block_received(&self, cb: core::CompactBlock, peer_addr: SocketAddr) -> bool {
		if !self.adapter.compact_block_received(cb, peer_addr) {
			// if the peer sent us a block that's intrinsically bad
			// they are either mistaken or manevolent, both of which require a ban
			self.ban_peer(&peer_addr);
			false
		} else {
			true
		}
	}
	fn header_received(&self, bh: core::BlockHeader, peer_addr: SocketAddr) -> bool {
		if !self.adapter.header_received(bh, peer_addr) {
			// if the peer sent us a block header that's intrinsically bad
			// they are either mistaken or manevolent, both of which require a ban
			self.ban_peer(&peer_addr);
			false
		} else {
			true
		}
	}
	fn headers_received(&self, headers: Vec<core::BlockHeader>, peer_addr: SocketAddr) {
		self.adapter.headers_received(headers, peer_addr)
	}
	fn locate_headers(&self, hs: Vec<Hash>) -> Vec<core::BlockHeader> {
		self.adapter.locate_headers(hs)
	}
	fn get_block(&self, h: Hash) -> Option<core::Block> {
		self.adapter.get_block(h)
	}
}

impl NetAdapter for Peers {
	/// Find good peers we know with the provided capability and return their
	/// addresses.
	fn find_peer_addrs(&self, capab: Capabilities) -> Vec<SocketAddr> {
		let peers = self.find_peers(State::Healthy, capab, MAX_PEER_ADDRS as usize);
		debug!(LOGGER, "Got {} peer addrs to send.", peers.len());
		map_vec!(peers, |p| p.addr)
	}

	/// A list of peers has been received from one of our peers.
	fn peer_addrs_received(&self, peer_addrs: Vec<SocketAddr>) {
		debug!(LOGGER, "Received {} peer addrs, saving.", peer_addrs.len());
		for pa in peer_addrs {
			if let Ok(e) = self.exists_peer(pa) {
				if e {
					continue;
				}
			}
			let peer = PeerData {
				addr: pa,
				capabilities: Capabilities::UNKNOWN,
				user_agent: "".to_string(),
				flags: State::Healthy,
				last_banned: 0,
			};
			if let Err(e) = self.save_peer(&peer) {
				error!(LOGGER, "Could not save received peer address: {:?}", e);
			}
		}
	}

	fn peer_difficulty(&self, addr: SocketAddr, diff: Difficulty, height: u64) {
		debug!(
			LOGGER,
			"peer total_diff @ height (ping/pong): {}: {} @ {} \
			 vs us: {} @ {}",
			addr,
			diff,
			height,
			self.total_difficulty(),
			self.total_height()
		);

		if diff.into_num() > 0 {
			if let Some(peer) = self.get_connected_peer(&addr) {
				let mut peer = peer.write().unwrap();
				peer.info.total_difficulty = diff;
			}
		}
	}
}
