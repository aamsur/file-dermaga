//! Core types of FileDermaga: precision-aware timestamps, directory entries,
//! server descriptions, and a parser for the many directory listing formats
//! that FTP servers send over the data connection.

pub mod datetime;
pub mod direntry;
pub mod listing_parser;
pub mod server;

pub use datetime::{Accuracy, DateTime, Tm};
pub use direntry::{DirEntry, DirectoryListing, FLAG_DIR, FLAG_LINK, FLAG_UNSURE};
pub use listing_parser::{parse_listing, ListingParser};
pub use server::{PasvMode, Protocol, Server, ServerType};
