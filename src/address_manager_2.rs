// This is a sketch of proposed address manager

#![allow(unused)]

use bitcoin::network::constants::ServiceFlags;
use bitcoin::Network;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Display;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use bitcoin::network::message::NetworkMessage;
use thiserror::Error;

use super::peer::{Mempool, Peer, PeerError};

const BITCOIN_MAINNET_SEED: [&str; 9] = [
    "seed.bitcoin.sipa.be",
    "dnsseed.bluematt.me",
    "dnsseed.bitcoin.dashjr.org",
    "seed.bitcoinstats.com",
    "seed.bitcoin.jonasschnelli.ch",
    "seed.btc.petertodd.org",
    "seed.bitcoin.sprovoost.nl",
    "dnsseed.emzy.de",
    "seed.bitcoin.wiz.biz",
];

const BITCOIN_TESTNET_SEED: [&str; 4] = [
    "testnet-seed.bitcoin.jonasschnelli.ch",
    "seed.tbtc.petertodd.org",
    "seed.testnet.bitcoin.sprovoost.nl",
    "testnet-seed.bluematt.me",
];

// TODO: Put them into config file
const NUM_WORKER_THREADS: usize = 5; // Higher number of threads can potentially cause deadlocks.
const CBF_ADDRS_TARGET: usize = 10; // Fetching stops after this
const NON_CBF_TARGET: usize = 50; // Fetching stops after this
const CBF_THRESHOLD: usize = 5; // Fetching triggers at this
const NON_CBF_THRESHOLD: usize = 20; // Fetching triggers at this
const NETWORK: Network = Network::Bitcoin; //Default mainnet

// A Hack to trick the workers to stop, without creating another channel
const STOP_ADDR: &str = "0.0.0.0:0000";

// The main address store type, indexed by SocketAddr
pub struct AddrsStore(HashMap<SocketAddr, AddrsEntry>);

impl AddrsStore {
    fn new() -> Self {
        Self(HashMap::new())
    }

    // get an entry for the given address
    fn get_entry(&self, addr: &SocketAddr) -> Option<&AddrsEntry> {
        self.0.get(addr)
    }

    // Iterator over all the keys
    fn iter_keys(&self) -> impl Iterator<Item = &SocketAddr> {
        self.0.keys()
    }

    // Update if existing, or add into the store
    fn put(&mut self, entry: &AddrsEntry) {
        self.0
            .entry(entry.addr)
            .and_modify(|existing| {
                *existing = entry.clone();
            })
            .or_insert(entry.clone());
    }

    fn cbf_count(&self) -> usize {
        self.0
            .values()
            .filter(|entry| entry.service.has(ServiceFlags::COMPACT_FILTERS))
            .count()
    }

    fn non_cbf_count(&self) -> usize {
        self.0
            .values()
            .filter(|entry| !entry.service.has(ServiceFlags::COMPACT_FILTERS))
            .count()
    }
}

struct AddrsManager {
    fetcher: AddrsFetcher,
    store: AddrsStore,
}

impl AddrsManager {
    fn new() -> Result<Self, AddrsMngrError> {
        let store = AddrsStore::new();
        let fetcher = AddrsFetcher::setup(NETWORK)?;
        Ok(Self { fetcher, store })
    }

    fn fill_up(&mut self) -> Result<(), AddrsMngrError> {
        let required_cbf = CBF_ADDRS_TARGET.saturating_sub(self.store.cbf_count());
        let required_non_cbf = NON_CBF_TARGET.saturating_sub(self.store.non_cbf_count());

        let filter_list = Some(self.store.iter_keys().cloned().collect());
        let entries = self
            .fetcher
            .fetch(required_cbf, required_non_cbf, filter_list)?;
        entries.iter().for_each(|entry| self.store.put(entry));
        Ok(())
    }
}

/// A Fetcher who spawns a number of worker and collects good addresses
/// and passes them on to the manager for storing.
struct AddrsFetcher {
    fetch_task: FetchTask,
    network: Network,
    worker_pool: Option<Vec<JoinHandle<Result<(), AddrsMngrError>>>>,
    // Channels to communicate with workers
    //
    // Tell workers to connect to these addresses.
    // One channel per worker.
    peer_conn_channel: Vec<Sender<SocketAddr>>,
    // Read the newly fetched addresses from workers.
    fetched_addr_receiver: Receiver<Vec<SocketAddr>>,
    // Read the full AddrsEntry for the connected peer.
    peer_entry_receiver: Receiver<AddrsEntry>,
}

impl AddrsFetcher {
    fn setup(network: Network) -> Result<Self, AddrsMngrError> {
        let num_workers = NUM_WORKER_THREADS;
        let fetch_task = FetchTask::new(network, vec![])?;

        // Create all the communication channels
        let mut peer_conn_senders = Vec::new();
        let mut peer_conn_receivers = Vec::new();
        for _ in 0..num_workers {
            let (sender, receiver) = channel::<SocketAddr>();
            peer_conn_senders.push(sender);
            peer_conn_receivers.push(receiver);
        }
        let (fetched_addr_sender, fetched_addr_receiver) = channel::<Vec<SocketAddr>>();
        let (peer_entry_sender, peer_entry_receiver) = channel::<AddrsEntry>();

        // Create workers with correct channels and spawn their threads
        let mut worker_pool = Vec::new();
        for peer_conn_receiver in peer_conn_receivers {
            let worker = AddrsWorker::new(
                peer_conn_receiver,
                fetched_addr_sender.clone(),
                peer_entry_sender.clone(),
                network,
            );
            let handle = thread::spawn(move || worker.work());
            worker_pool.push(handle);
        }

        Ok(Self {
            fetch_task,
            network,
            worker_pool: Some(worker_pool),
            peer_conn_channel: peer_conn_senders,
            fetched_addr_receiver,
            peer_entry_receiver,
        })
    }

    // fetch fortarget number of CBFs and Non-CBFs.
    // Optionally takes in a filter list to exclude addresses.
    fn fetch(
        &mut self,
        cbf_target: usize,
        non_cbf_target: usize,
        filter_list: Option<Vec<SocketAddr>>,
    ) -> Result<Vec<AddrsEntry>, AddrsMngrError> {
        let mut fetch_result = Vec::<AddrsEntry>::new();
        let mut cbf_count = 0;
        let mut non_cbf_count = 0;

        // The main fetching loop
        while cbf_count <= cbf_target || non_cbf_count <= non_cbf_target {
            // Report discovery statistics
            let discovery_data = DiscoveryData::new(
                self.fetch_task.pending.len(),
                self.fetch_task.visited.len(),
                cbf_count,
                non_cbf_count,
            );
            println!("{}", discovery_data);

            // Send connection request to each worker
            for peer_connect_sender in self.peer_conn_channel.iter() {
                if let Some(peer_addr) = self.fetch_task.get_pending() {
                    // Send connection making instruction to the worker
                    peer_connect_sender.send(peer_addr)?;
                }
            }

            // Now try to receive responses from each worker
            for _ in 0..NUM_WORKER_THREADS {
                // Receive fetched addresses from workers, and add them to pending list
                let addrs = self.fetched_addr_receiver.recv()?;
                self.fetch_task.add_pendings(addrs);

                // Received connected peer's address entry
                let entry = self.peer_entry_receiver.recv()?;
                let is_cbf = entry.service.has(ServiceFlags::COMPACT_FILTERS);

                // We filter for target and type together, as we don't wanna fetch more than we need.
                if let Some(list) = &filter_list {
                    if is_cbf && non_cbf_count <= non_cbf_target && list.contains(&entry.addr) {
                        cbf_count += 1;
                        fetch_result.push(entry);
                    } else if cbf_count <= cbf_target && list.contains(&entry.addr) {
                        non_cbf_count += 1;
                        fetch_result.push(entry)
                    }
                }
            }
        }
        Ok(fetch_result)
    }
}

impl Drop for AddrsFetcher {
    fn drop(&mut self) {
        // Fetching target reached. Send stop signal to all workers
        for sender in &self.peer_conn_channel {
            sender.send(SocketAddr::from_str(STOP_ADDR).unwrap());
        }

        let worker_pool = self
            .worker_pool
            .take()
            .expect("Expect running working threads");
        // Join all the worker threads.
        for worker in worker_pool {
            let _ = worker.join();
        }
    }
}

/// A worker crawls the bitcoin network and finds new addresses,
/// and passes them back to the fetcher.
struct AddrsWorker {
    // Channel to recv peer addrs to fetch from
    peer_conn_receiver: Receiver<SocketAddr>,
    // Channel to send fetched Addresses
    fetched_addrs_sender: Sender<Vec<SocketAddr>>,
    // Chennel to send Connected Peer Details
    peer_entry_sender: Sender<AddrsEntry>,
    // Network is needed to construct peer connection
    network: Network,
}

impl AddrsWorker {
    fn new(
        peer_conn_receiver: Receiver<SocketAddr>,
        fetched_addrs_sender: Sender<Vec<SocketAddr>>,
        peer_entry_sender: Sender<AddrsEntry>,
        network: Network,
    ) -> Self {
        Self {
            peer_conn_receiver,
            fetched_addrs_sender,
            peer_entry_sender,
            network,
        }
    }

    fn work(&self) -> Result<(), AddrsMngrError> {
        loop {
            // Get The peer connection request
            let peer_to_connect = self.peer_conn_receiver.recv()?;

            // Stop if sto signal is received
            if peer_to_connect.to_string() == STOP_ADDR {
                break;
            }

            // Connect with the peer
            if let Ok(peer) = Peer::connect_with_timeout(
                peer_to_connect,
                Duration::from_secs(1),
                Arc::new(Mempool::default()),
                self.network,
            ) {
                // Request new address
                peer.send(NetworkMessage::GetAddr)?;
                // Wait for some response
                if let Ok(Some(NetworkMessage::Addr(list))) =
                    peer.recv("addr", Some(Duration::from_secs(1)))
                {
                    // Collect and send fetched addresses back to fetcher
                    let list = list
                        .iter()
                        .filter_map(|(timestamp, addrs)| {
                            if timestamp > &0 {
                                Some(addrs.socket_addr())
                            } else {
                                None
                            }
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    self.fetched_addrs_sender.send(list)?;

                    // Send this peer's entry data back to fetcher
                    let this_peer_entry = AddrsEntry {
                        addr: peer_to_connect,
                        attempts: 1,
                        last_attempts: SystemTime::now(),
                        last_successful: SystemTime::now(),
                        tried: true,
                        service: peer.get_version().services,
                        rank: 0,
                    };
                    self.peer_entry_sender.send(this_peer_entry)?;
                    peer.close()?;
                }
            }
        }
        Ok(())
    }
}

trait Worker {
    fn work(&self);
}

/// A running list of pending and visited addresses.
/// Workers use this to fetch their next peer to connect.
struct FetchTask {
    pending: VecDeque<SocketAddr>,
    visited: HashSet<SocketAddr>,
    // seed address list depends on network
    network: Network,
}

impl FetchTask {
    // Init a fetch task with seed addresses
    fn new(network: Network, mut seeds: Vec<SocketAddr>) -> Result<Self, AddrsMngrError> {
        let mut network_seeds = Self::seeds(network)?;
        network_seeds.append(&mut seeds);
        Ok(Self {
            pending: network_seeds.into(),
            visited: HashSet::new(),
            network,
        })
    }

    fn seeds(network: Network) -> Result<Vec<SocketAddr>, AddrsMngrError> {
        let port: u16 = match network {
            Network::Bitcoin => 8333,
            Network::Testnet => 18333,
            Network::Regtest => 18444,
            Network::Signet => 38333,
        };

        let seedhosts: &[&str] = match network {
            Network::Bitcoin => &BITCOIN_MAINNET_SEED,
            Network::Testnet => &BITCOIN_TESTNET_SEED,
            Network::Regtest => &[],
            Network::Signet => &[],
        };

        Ok(seedhosts
            .iter()
            .map(|seed_addr| (*seed_addr, port).to_socket_addrs())
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .map(|iter| iter.clone().collect::<Vec<_>>())
            .flatten()
            .collect())
    }

    // Add in the pending list. Ensure its not already in pending or visited
    fn add_pendings(&mut self, addresses: Vec<SocketAddr>) {
        for addr in addresses {
            if !self.pending.contains(&addr) && !self.visited.contains(&addr) {
                self.pending.push_back(addr);
            }
        }
    }

    // Get next pending. Add it to the visited list
    fn get_pending(&mut self) -> Option<SocketAddr> {
        self.pending.pop_front().and_then(|addrs| {
            self.visited.insert(addrs);
            Some(addrs)
        })
    }
}

/// AddrsEntry contains the fully fetched information about an address
#[derive(Clone)]
struct AddrsEntry {
    addr: SocketAddr,
    attempts: u32,
    last_attempts: std::time::SystemTime,
    last_successful: std::time::SystemTime,
    tried: bool,
    service: bitcoin::network::constants::ServiceFlags,
    rank: u32,
}

/// Discovery statistics, useful for logging
#[derive(Clone, Copy)]
pub struct DiscoveryData {
    queued: usize,
    visited: usize,
    non_cbf_count: usize,
    cbf_count: usize,
}

impl DiscoveryData {
    fn new(queued: usize, visited: usize, cbf_count: usize, non_cbf_count: usize) -> Self {
        Self {
            queued,
            visited,
            non_cbf_count,
            cbf_count,
        }
    }
}

impl Display for DiscoveryData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pending: {}, visited: {}, cbf_count: {}, non_cbf_count: {}",
            self.queued, self.visited, self.cbf_count, self.non_cbf_count
        )
    }
}

#[derive(Debug, Error)]
enum AddrsMngrError {
    #[error("std IO Error")]
    IO(#[from] std::io::Error),
    #[error("P2P Network Error")]
    Peer(#[from] PeerError),
    #[error("Internal MPSC Error: {}", .0)]
    Mpsc(String),
}

impl From<std::sync::mpsc::RecvError> for AddrsMngrError {
    fn from(sync_err: std::sync::mpsc::RecvError) -> Self {
        Self::Mpsc(sync_err.to_string())
    }
}

impl<T> From<std::sync::mpsc::SendError<T>> for AddrsMngrError {
    fn from(sync_err: std::sync::mpsc::SendError<T>) -> Self {
        Self::Mpsc(sync_err.to_string())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn basic() {
        let mut manager = AddrsManager::new().unwrap();
        manager.fill_up();

        println!("CBF COUNT: {}", manager.store.cbf_count());
    }
}
