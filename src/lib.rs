/// This is Work in Progress Library for running a BIP157 node
/// Intended to be used as a backend for BDK based wallets.
///
///
///
///

mod address_manager_2;
mod address_manager;
mod peer;
mod peermngr;
mod store;
mod sync;


use thiserror::Error;
use peer::PeerError;


/// An error that can occur during sync with a [`CompactFiltersBlockchain`]
#[derive(Debug, Error)]
pub enum CompactFiltersError {
    #[error("A peer sent an invalid or unexpected response")]
    InvalidResponse,
    #[error("The headers returned are invalid")]
    InvalidHeaders,
    #[error("The compact filter headers returned are invalid")]
    InvalidFilterHeader,
    #[error("The compact filter returned is invalid")]
    InvalidFilter,
    #[error("The peer is missing a block in the valid chain")]
    MissingBlock,
    #[error("Block hash at specified height not found")]
    BlockHashNotFound,
    #[error("The data stored in the block filters storage are corrupted")]
    DataCorruption,

    #[error("A peer is not connected")]
    NotConnected,
    #[error("A peer took too long to reply to one of our messages")]
    Timeout,
    #[error("The peer doesn't advertise the service flag")]
    PeerBloomDisabled,

    #[error("No peers have been specified")]
    NoPeers,

    #[error("Internal database error : {}", .0)]
    Db(#[from]rocksdb::Error),
    #[error("Internal I/O error : {}", .0)]
    Io(#[from]std::io::Error),
    #[error("Invalid BIP158 filter : {}", .0)]
    Bip158(#[from]bitcoin::util::bip158::Error),
    #[error("Internal system time error : {}", .0)]
    Time(#[from]std::time::SystemTimeError),

    #[error("Internal Peer Error")]
    Peer(#[from]PeerError),
}