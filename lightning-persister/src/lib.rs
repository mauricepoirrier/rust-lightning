//! Utilities that handle persisting Rust-Lightning data to disk via standard filesystem APIs.

#![deny(broken_intra_doc_links)]
#![deny(missing_docs)]

#![cfg_attr(all(test, feature = "_bench_unstable"), feature(test))]
#[cfg(all(test, feature = "_bench_unstable"))] extern crate test;

mod util;

extern crate lightning;
extern crate bitcoin;
extern crate libc;

use bitcoin::hash_types::{BlockHash, Txid};
use bitcoin::hashes::hex::{FromHex, ToHex};
use crate::util::DiskWriteable;
use lightning::chain;
use lightning::chain::chaininterface::{BroadcasterInterface, FeeEstimator};
use lightning::chain::channelmonitor::{ChannelMonitor, ChannelMonitorUpdate};
use lightning::chain::chainmonitor;
use lightning::chain::keysinterface::{Sign, KeysInterface};
use lightning::chain::transaction::OutPoint;
use lightning::ln::channelmanager::ChannelManager;
use lightning::util::logger::Logger;
use lightning::util::ser::{ReadableArgs, Writeable};
use std::fs;
use std::io::{Cursor, Error};
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// FilesystemPersister persists channel data on disk, where each channel's
/// data is stored in a file named after its funding outpoint.
///
/// Warning: this module does the best it can with calls to persist data, but it
/// can only guarantee that the data is passed to the drive. It is up to the
/// drive manufacturers to do the actual persistence properly, which they often
/// don't (especially on consumer-grade hardware). Therefore, it is up to the
/// user to validate their entire storage stack, to ensure the writes are
/// persistent.
/// Corollary: especially when dealing with larger amounts of money, it is best
/// practice to have multiple channel data backups and not rely only on one
/// FilesystemPersister.
pub struct FilesystemPersister {
	path_to_channel_data: String,
}

impl<Signer: Sign> DiskWriteable for ChannelMonitor<Signer> {
	fn write_to_file(&self, writer: &mut fs::File) -> Result<(), Error> {
		self.write(writer)
	}
}

impl<Signer: Sign, M: Deref, T: Deref, K: Deref, F: Deref, L: Deref> DiskWriteable for ChannelManager<Signer, M, T, K, F, L>
where
	M::Target: chain::Watch<Signer>,
	T::Target: BroadcasterInterface,
	K::Target: KeysInterface<Signer=Signer>,
	F::Target: FeeEstimator,
	L::Target: Logger,
{
	fn write_to_file(&self, writer: &mut fs::File) -> Result<(), std::io::Error> {
		self.write(writer)
	}
}

impl FilesystemPersister {
	/// Initialize a new FilesystemPersister and set the path to the individual channels'
	/// files.
	pub fn new(path_to_channel_data: String) -> Self {
		return Self {
			path_to_channel_data,
		}
	}

	/// Get the directory which was provided when this persister was initialized.
	pub fn get_data_dir(&self) -> String {
		self.path_to_channel_data.clone()
	}

	pub(crate) fn path_to_monitor_data(&self) -> PathBuf {
		let mut path = PathBuf::from(self.path_to_channel_data.clone());
		path.push("monitors");
		path
	}

	/// Writes the provided `ChannelManager` to the path provided at `FilesystemPersister`
	/// initialization, within a file called "manager".
	pub fn persist_manager<Signer: Sign, M: Deref, T: Deref, K: Deref, F: Deref, L: Deref>(
		data_dir: String,
		manager: &ChannelManager<Signer, M, T, K, F, L>
	) -> Result<(), std::io::Error>
	where
		M::Target: chain::Watch<Signer>,
		T::Target: BroadcasterInterface,
		K::Target: KeysInterface<Signer=Signer>,
		F::Target: FeeEstimator,
		L::Target: Logger,
	{
		let path = PathBuf::from(data_dir);
		util::write_to_file(path, "manager".to_string(), manager)
	}

	/// Read `ChannelMonitor`s from disk.
	pub fn read_channelmonitors<Signer: Sign, K: Deref> (
		&self, keys_manager: K
	) -> Result<Vec<(BlockHash, ChannelMonitor<Signer>)>, std::io::Error>
		where K::Target: KeysInterface<Signer=Signer> + Sized,
	{
		let path = self.path_to_monitor_data();
		if !Path::new(&path).exists() {
			return Ok(Vec::new());
		}
		let mut res = Vec::new();
		for file_option in fs::read_dir(path).unwrap() {
			let file = file_option.unwrap();
			let owned_file_name = file.file_name();
			let filename = owned_file_name.to_str();
			if !filename.is_some() || !filename.unwrap().is_ascii() || filename.unwrap().len() < 65 {
				return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					"Invalid ChannelMonitor file name",
				));
			}

			let txid = Txid::from_hex(filename.unwrap().split_at(64).0);
			if txid.is_err() {
				return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					"Invalid tx ID in filename",
				));
			}

			let index = filename.unwrap().split_at(65).1.parse();
			if index.is_err() {
				return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					"Invalid tx index in filename",
				));
			}

			let contents = fs::read(&file.path())?;
			let mut buffer = Cursor::new(&contents);
			match <(BlockHash, ChannelMonitor<Signer>)>::read(&mut buffer, &*keys_manager) {
				Ok((blockhash, channel_monitor)) => {
					if channel_monitor.get_funding_txo().0.txid != txid.unwrap() || channel_monitor.get_funding_txo().0.index != index.unwrap() {
						return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "ChannelMonitor was stored in the wrong file"));
					}
					res.push((blockhash, channel_monitor));
				}
				Err(e) => return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					format!("Failed to deserialize ChannelMonitor: {}", e),
				))
			}
		}
		Ok(res)
	}
}

impl<ChannelSigner: Sign> chainmonitor::Persist<ChannelSigner> for FilesystemPersister {
	// TODO: We really need a way for the persister to inform the user that its time to crash/shut
	// down once these start returning failure.
	// A PermanentFailure implies we need to shut down since we're force-closing channels without
	// even broadcasting!

	fn persist_new_channel(&self, funding_txo: OutPoint, monitor: &ChannelMonitor<ChannelSigner>, _update_id: chainmonitor::MonitorUpdateId) -> Result<(), chain::ChannelMonitorUpdateErr> {
		let filename = format!("{}_{}", funding_txo.txid.to_hex(), funding_txo.index);
		util::write_to_file(self.path_to_monitor_data(), filename, monitor)
			.map_err(|_| chain::ChannelMonitorUpdateErr::PermanentFailure)
	}

	fn update_persisted_channel(&self, funding_txo: OutPoint, _update: &Option<ChannelMonitorUpdate>, monitor: &ChannelMonitor<ChannelSigner>, _update_id: chainmonitor::MonitorUpdateId) -> Result<(), chain::ChannelMonitorUpdateErr> {
		let filename = format!("{}_{}", funding_txo.txid.to_hex(), funding_txo.index);
		util::write_to_file(self.path_to_monitor_data(), filename, monitor)
			.map_err(|_| chain::ChannelMonitorUpdateErr::PermanentFailure)
	}
}

#[cfg(test)]
mod tests {
	extern crate lightning;
	extern crate bitcoin;
	use crate::FilesystemPersister;
	use bitcoin::blockdata::block::{Block, BlockHeader};
	use bitcoin::hashes::hex::FromHex;
	use bitcoin::Txid;
	use lightning::chain::ChannelMonitorUpdateErr;
	use lightning::chain::chainmonitor::Persist;
	use lightning::chain::transaction::OutPoint;
	use lightning::{check_closed_broadcast, check_closed_event, check_added_monitors};
	use lightning::ln::features::InitFeatures;
	use lightning::ln::functional_test_utils::*;
	use lightning::util::events::{ClosureReason, MessageSendEventsProvider};
	use lightning::util::test_utils;
	use std::fs;
	#[cfg(target_os = "windows")]
	use {
		lightning::get_event_msg,
		lightning::ln::msgs::ChannelMessageHandler,
	};

	impl Drop for FilesystemPersister {
		fn drop(&mut self) {
			// We test for invalid directory names, so it's OK if directory removal
			// fails.
			match fs::remove_dir_all(&self.path_to_channel_data) {
				Err(e) => println!("Failed to remove test persister directory: {}", e),
				_ => {}
			}
		}
	}

	// Integration-test the FilesystemPersister. Test relaying a few payments
	// and check that the persisted data is updated the appropriate number of
	// times.
	#[test]
	fn test_filesystem_persister() {
		// Create the nodes, giving them FilesystemPersisters for data persisters.
		let persister_0 = FilesystemPersister::new("test_filesystem_persister_0".to_string());
		let persister_1 = FilesystemPersister::new("test_filesystem_persister_1".to_string());
		let chanmon_cfgs = create_chanmon_cfgs(2);
		let mut node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
		let chain_mon_0 = test_utils::TestChainMonitor::new(Some(&chanmon_cfgs[0].chain_source), &chanmon_cfgs[0].tx_broadcaster, &chanmon_cfgs[0].logger, &chanmon_cfgs[0].fee_estimator, &persister_0, &node_cfgs[0].keys_manager);
		let chain_mon_1 = test_utils::TestChainMonitor::new(Some(&chanmon_cfgs[1].chain_source), &chanmon_cfgs[1].tx_broadcaster, &chanmon_cfgs[1].logger, &chanmon_cfgs[1].fee_estimator, &persister_1, &node_cfgs[1].keys_manager);
		node_cfgs[0].chain_monitor = chain_mon_0;
		node_cfgs[1].chain_monitor = chain_mon_1;
		let node_chanmgrs = create_node_chanmgrs(2, &node_cfgs, &[None, None]);
		let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

		// Check that the persisted channel data is empty before any channels are
		// open.
		let mut persisted_chan_data_0 = persister_0.read_channelmonitors(nodes[0].keys_manager).unwrap();
		assert_eq!(persisted_chan_data_0.len(), 0);
		let mut persisted_chan_data_1 = persister_1.read_channelmonitors(nodes[1].keys_manager).unwrap();
		assert_eq!(persisted_chan_data_1.len(), 0);

		// Helper to make sure the channel is on the expected update ID.
		macro_rules! check_persisted_data {
			($expected_update_id: expr) => {
				persisted_chan_data_0 = persister_0.read_channelmonitors(nodes[0].keys_manager).unwrap();
				assert_eq!(persisted_chan_data_0.len(), 1);
				for (_, mon) in persisted_chan_data_0.iter() {
					assert_eq!(mon.get_latest_update_id(), $expected_update_id);
				}
				persisted_chan_data_1 = persister_1.read_channelmonitors(nodes[1].keys_manager).unwrap();
				assert_eq!(persisted_chan_data_1.len(), 1);
				for (_, mon) in persisted_chan_data_1.iter() {
					assert_eq!(mon.get_latest_update_id(), $expected_update_id);
				}
			}
		}

		// Create some initial channel and check that a channel was persisted.
		let _ = create_announced_chan_between_nodes(&nodes, 0, 1, InitFeatures::known(), InitFeatures::known());
		check_persisted_data!(0);

		// Send a few payments and make sure the monitors are updated to the latest.
		send_payment(&nodes[0], &vec!(&nodes[1])[..], 8000000);
		check_persisted_data!(5);
		send_payment(&nodes[1], &vec!(&nodes[0])[..], 4000000);
		check_persisted_data!(10);

		// Force close because cooperative close doesn't result in any persisted
		// updates.
		nodes[0].node.force_close_channel(&nodes[0].node.list_channels()[0].channel_id).unwrap();
		check_closed_event!(nodes[0], 1, ClosureReason::HolderForceClosed);
		check_closed_broadcast!(nodes[0], true);
		check_added_monitors!(nodes[0], 1);

		let node_txn = nodes[0].tx_broadcaster.txn_broadcasted.lock().unwrap();
		assert_eq!(node_txn.len(), 1);

		let header = BlockHeader { version: 0x20000000, prev_blockhash: nodes[0].best_block_hash(), merkle_root: Default::default(), time: 42, bits: 42, nonce: 42 };
		connect_block(&nodes[1], &Block { header, txdata: vec![node_txn[0].clone(), node_txn[0].clone()]});
		check_closed_broadcast!(nodes[1], true);
		check_closed_event!(nodes[1], 1, ClosureReason::CommitmentTxConfirmed);
		check_added_monitors!(nodes[1], 1);

		// Make sure everything is persisted as expected after close.
		check_persisted_data!(11);
	}

	// Test that if the persister's path to channel data is read-only, writing a
	// monitor to it results in the persister returning a PermanentFailure.
	// Windows ignores the read-only flag for folders, so this test is Unix-only.
	#[cfg(not(target_os = "windows"))]
	#[test]
	fn test_readonly_dir_perm_failure() {
		let persister = FilesystemPersister::new("test_readonly_dir_perm_failure".to_string());
		fs::create_dir_all(&persister.path_to_channel_data).unwrap();

		// Set up a dummy channel and force close. This will produce a monitor
		// that we can then use to test persistence.
		let chanmon_cfgs = create_chanmon_cfgs(2);
		let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
		let node_chanmgrs = create_node_chanmgrs(2, &node_cfgs, &[None, None]);
		let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
		let chan = create_announced_chan_between_nodes(&nodes, 0, 1, InitFeatures::known(), InitFeatures::known());
		nodes[1].node.force_close_channel(&chan.2).unwrap();
		check_closed_event!(nodes[1], 1, ClosureReason::HolderForceClosed);
		let mut added_monitors = nodes[1].chain_monitor.added_monitors.lock().unwrap();
		let update_map = nodes[1].chain_monitor.latest_monitor_update_id.lock().unwrap();
		let update_id = update_map.get(&added_monitors[0].0.to_channel_id()).unwrap();

		// Set the persister's directory to read-only, which should result in
		// returning a permanent failure when we then attempt to persist a
		// channel update.
		let path = &persister.path_to_channel_data;
		let mut perms = fs::metadata(path).unwrap().permissions();
		perms.set_readonly(true);
		fs::set_permissions(path, perms).unwrap();

		let test_txo = OutPoint {
			txid: Txid::from_hex("8984484a580b825b9972d7adb15050b3ab624ccd731946b3eeddb92f4e7ef6be").unwrap(),
			index: 0
		};
		match persister.persist_new_channel(test_txo, &added_monitors[0].1, update_id.2) {
			Err(ChannelMonitorUpdateErr::PermanentFailure) => {},
			_ => panic!("unexpected result from persisting new channel")
		}

		nodes[1].node.get_and_clear_pending_msg_events();
		added_monitors.clear();
	}

	// Test that if a persister's directory name is invalid, monitor persistence
	// will fail.
	#[cfg(target_os = "windows")]
	#[test]
	fn test_fail_on_open() {
		// Set up a dummy channel and force close. This will produce a monitor
		// that we can then use to test persistence.
		let chanmon_cfgs = create_chanmon_cfgs(2);
		let mut node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
		let node_chanmgrs = create_node_chanmgrs(2, &node_cfgs, &[None, None]);
		let nodes = create_network(2, &node_cfgs, &node_chanmgrs);
		let chan = create_announced_chan_between_nodes(&nodes, 0, 1, InitFeatures::known(), InitFeatures::known());
		nodes[1].node.force_close_channel(&chan.2).unwrap();
		check_closed_event!(nodes[1], 1, ClosureReason::HolderForceClosed);
		let mut added_monitors = nodes[1].chain_monitor.added_monitors.lock().unwrap();
		let update_map = nodes[1].chain_monitor.latest_monitor_update_id.lock().unwrap();
		let update_id = update_map.get(&added_monitors[0].0.to_channel_id()).unwrap();

		// Create the persister with an invalid directory name and test that the
		// channel fails to open because the directories fail to be created. There
		// don't seem to be invalid filename characters on Unix that Rust doesn't
		// handle, hence why the test is Windows-only.
		let persister = FilesystemPersister::new(":<>/".to_string());

		let test_txo = OutPoint {
			txid: Txid::from_hex("8984484a580b825b9972d7adb15050b3ab624ccd731946b3eeddb92f4e7ef6be").unwrap(),
			index: 0
		};
		match persister.persist_new_channel(test_txo, &added_monitors[0].1, update_id.2) {
			Err(ChannelMonitorUpdateErr::PermanentFailure) => {},
			_ => panic!("unexpected result from persisting new channel")
		}

		nodes[1].node.get_and_clear_pending_msg_events();
		added_monitors.clear();
	}
}

#[cfg(all(test, feature = "_bench_unstable"))]
pub mod bench {
	use test::Bencher;

	#[bench]
	fn bench_sends(bench: &mut Bencher) {
		let persister_a = super::FilesystemPersister::new("bench_filesystem_persister_a".to_string());
		let persister_b = super::FilesystemPersister::new("bench_filesystem_persister_b".to_string());
		lightning::ln::channelmanager::bench::bench_two_sends(bench, persister_a, persister_b);
	}
}
