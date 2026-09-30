#![forbid(unsafe_code)]

use std::io;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use obstore::config::load_stack;
use obstore::protocol::{
    bind_socket, fetch_object, import_admission, request_release, request_transfer,
    request_who_has, serve_connection, socket_path, Admission,
};
use obstore::releases::{import_sets, FirmwareSet};
use obstore::store::{parse_verifying_key, Claims, ObjectStore};
use obstore::transfer::{load_object_transfer_address, serve_peer, ObjectTransfer};

#[derive(Debug, Parser)]
#[command(
    name = "object-services",
    about = "Local object store and object transfer"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Listen for store and transfer requests.
    Serve {
        /// Directory that contains the stack config file.
        #[arg(long, value_name = "DIR")]
        config: PathBuf,
    },
    /// Stream a file to the running object store service.
    Import {
        /// Directory that contains the stack config file.
        #[arg(long, value_name = "DIR")]
        config: PathBuf,
        /// File to store. The object id is the SHA-256 of these bytes.
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
        /// LOA envelope claims, as a comma-separated name=value list.
        /// Signed only with --as-owner.
        #[arg(
            long,
            value_name = "NAME=VALUE,...",
            value_delimiter = ',',
            conflicts_with = "envelope"
        )]
        claims: Vec<String>,
        /// Sign the claims and object id with this stack's Local Object Authority key.
        #[arg(long, conflicts_with = "envelope")]
        as_owner: bool,
        /// Envelope already signed by another authority. Stored unchanged after the signature checks.
        #[arg(long, value_name = "PATH", conflicts_with = "as_owner")]
        envelope: Option<PathBuf>,
        /// Ed25519 verifying key (64 hex) or identity public key (128 hex) that signed --envelope.
        #[arg(long, value_name = "HEX", requires = "envelope")]
        authority: Option<String>,
    },
    /// Fetch an object from the running object store service.
    Fetch {
        /// Directory that contains the stack config file.
        #[arg(long, value_name = "DIR")]
        config: PathBuf,
        /// Object id to fetch. This is the SHA-256 of the stored bytes.
        #[arg(long, value_name = "HEX")]
        object_id: String,
        /// File to write.
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
    },
    /// Offer an object to the object-transfer service at a destination address.
    Transfer {
        /// Directory that contains the stack config file.
        #[arg(long, value_name = "DIR")]
        config: PathBuf,
        /// Object id to offer. This is the SHA-256 of the stored bytes.
        #[arg(long, value_name = "HEX")]
        object_id: String,
        /// Object-transfer address of the receiving stack, 32 hex characters.
        #[arg(long, value_name = "HEX")]
        destination: String,
    },
    /// Ask who has a piece, one hop ring at a time. The first send uses 0 hops.
    WhoHas {
        /// Directory that contains the stack config file.
        #[arg(long, value_name = "DIR")]
        config: PathBuf,
        /// Object id from the local transfer manifest.
        #[arg(long, value_name = "HEX")]
        object_id: String,
        /// Piece index in that transfer manifest.
        #[arg(long, value_name = "N")]
        index: u32,
    },
    /// Announce a release to directly connected object-transfer services.
    Release {
        /// Directory that contains the stack config file.
        #[arg(long, value_name = "DIR")]
        config: PathBuf,
        /// Object id to release. This is the SHA-256 of the stored bytes.
        #[arg(long, value_name = "HEX")]
        object_id: String,
        /// Hops remaining for neighbors to propagate. Each hop decrements this.
        #[arg(long, value_name = "N", default_value_t = 8)]
        hops: u8,
    },
    /// Download the preview and stable firmware releases into the local store.
    ///
    /// Each board is stored twice: a USB zip for hopspot-flash, and the exact OTA
    /// file. This stack's Local Object Authority signs both.
    ImportReleases {
        /// Directory that contains the stack config file.
        #[arg(long, value_name = "DIR")]
        config: PathBuf,
        /// Firmware set to import. Repeat the flag to choose both. Defaults to both sets.
        #[arg(long = "set", value_enum, value_name = "SET")]
        sets: Vec<FirmwareSetArg>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum FirmwareSetArg {
    Preview,
    Stable,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Serve { config } => serve(&config),
        Command::Import {
            config,
            file,
            claims,
            as_owner,
            envelope,
            authority,
        } => import(
            &config,
            &file,
            &claims,
            as_owner,
            envelope.as_deref(),
            authority.as_deref(),
        ),
        Command::Fetch {
            config,
            object_id,
            file,
        } => fetch(&config, &object_id, &file),
        Command::Transfer {
            config,
            object_id,
            destination,
        } => transfer(&config, &object_id, &destination),
        Command::WhoHas {
            config,
            object_id,
            index,
        } => who_has(&config, &object_id, index),
        Command::Release {
            config,
            object_id,
            hops,
        } => release(&config, &object_id, hops),
        Command::ImportReleases { config, sets } => import_releases(&config, &sets),
    }
}

fn serve(config_dir: &std::path::Path) -> ExitCode {
    let stack = match load_stack(config_dir) {
        Ok(stack) => stack,
        Err(error) => {
            eprintln!("object-services: {error}");
            return ExitCode::FAILURE;
        }
    };
    let store = match ObjectStore::open(&stack.object_store) {
        Ok(store) => Arc::new(store),
        Err(error) => {
            eprintln!("object-services: {error}");
            return ExitCode::FAILURE;
        }
    };
    let address = match load_object_transfer_address(config_dir) {
        Ok(address) => address,
        Err(error) => {
            eprintln!("object-services: {error}");
            return ExitCode::FAILURE;
        }
    };
    let transfer = match ObjectTransfer::open(&stack.object_transfer, address) {
        Ok(mut transfer) => {
            transfer.set_fetch_policy(stack.fetch);
            transfer
        }
        Err(error) => {
            eprintln!("object-services: {error}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "object-services: object-transfer address {}",
        address.as_hex()
    );
    let transfer_socket = config_dir.join("object-transfer.sock");
    let transfer_listener = match bind_listener(&transfer_socket) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("object-services: {error}");
            return ExitCode::FAILURE;
        }
    };
    let socket = socket_path(config_dir);
    let listener = match bind_listener(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("object-services: {error}");
            return ExitCode::FAILURE;
        }
    };
    if store.created_loa_key() {
        eprintln!(
            "object-services: created Local Object Authority key {}",
            store.loa_key_path().display()
        );
    }
    eprintln!(
        "object-services: listening on {} for {}",
        socket.display(),
        stack.object_store.display()
    );
    eprintln!(
        "object-services: object-transfer listening on {}",
        transfer_socket.display()
    );
    let peer_store = Arc::clone(&store);
    let peer_transfer = transfer.clone();
    std::thread::spawn(move || {
        for connection in transfer_listener.incoming() {
            let store = Arc::clone(&peer_store);
            let transfer = peer_transfer.clone();
            std::thread::spawn(move || match connection {
                Ok(mut stream) => {
                    // The transport has not yet named the arrival interface, so this
                    // socket does not lower the who-has rate.
                    if let Err(error) = serve_peer(&store, &transfer, &mut stream, u64::MAX) {
                        eprintln!("object-services: {error}");
                    }
                }
                Err(error) => eprintln!("object-services: {error}"),
            });
        }
    });
    let _socket_guard = SocketGuard(socket);
    let _transfer_guard = SocketGuard(transfer_socket);
    for connection in listener.incoming() {
        match connection {
            Ok(mut stream) => {
                if let Err(error) = serve_connection(&store, Some(&transfer), &mut stream) {
                    eprintln!("object-services: {error}");
                }
            }
            Err(error) => eprintln!("object-services: {error}"),
        }
    }
    ExitCode::SUCCESS
}

fn import(
    config_dir: &std::path::Path,
    file: &std::path::Path,
    claims: &[String],
    as_owner: bool,
    envelope: Option<&std::path::Path>,
    authority: Option<&str>,
) -> ExitCode {
    if !claims.is_empty() && !as_owner {
        eprintln!("object-services: pass --as-owner to sign claims into an LOA envelope");
        return ExitCode::FAILURE;
    }
    if as_owner {
        let claims = match Claims::parse(claims) {
            Ok(claims) => claims,
            Err(error) => {
                eprintln!("object-services: {error}");
                return ExitCode::FAILURE;
            }
        };
        return finish_import(import_admission(
            &socket_path(config_dir),
            file,
            Admission::Owner(&claims),
        ));
    }
    if let Some(envelope_path) = envelope {
        let text = match std::fs::read_to_string(envelope_path) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("object-services: {error}");
                return ExitCode::FAILURE;
            }
        };
        let verifying_key = match authority.map(parse_verifying_key) {
            Some(Ok(key)) => key,
            Some(Err(error)) => {
                eprintln!("object-services: {error}");
                return ExitCode::FAILURE;
            }
            None => {
                eprintln!("object-services: an existing envelope needs --authority");
                return ExitCode::FAILURE;
            }
        };
        return finish_import(import_admission(
            &socket_path(config_dir),
            file,
            Admission::Envelope {
                text: &text,
                verifying_key: &verifying_key,
            },
        ));
    }
    finish_import(import_admission(
        &socket_path(config_dir),
        file,
        Admission::Plain,
    ))
}

fn import_releases(config_dir: &std::path::Path, sets: &[FirmwareSetArg]) -> ExitCode {
    let selected: Vec<FirmwareSet> = if sets.is_empty() {
        vec![FirmwareSet::Preview, FirmwareSet::Stable]
    } else {
        sets.iter()
            .map(|set| match set {
                FirmwareSetArg::Preview => FirmwareSet::Preview,
                FirmwareSetArg::Stable => FirmwareSet::Stable,
            })
            .collect()
    };
    let stack = match load_stack(config_dir) {
        Ok(stack) => stack,
        Err(error) => {
            eprintln!("object-services: {error}");
            return ExitCode::FAILURE;
        }
    };
    let store = match ObjectStore::open(&stack.object_store) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("object-services: {error}");
            return ExitCode::FAILURE;
        }
    };
    if store.created_loa_key() {
        eprintln!(
            "object-services: created Local Object Authority key {}",
            store.loa_key_path().display()
        );
    }
    match import_sets(&store, &selected) {
        Ok(outcome) => {
            for notice in outcome.unpublished {
                eprintln!("object-services: {notice}");
            }
            for firmware in outcome.imported {
                eprintln!(
                    "object-services: {} {} {} {}",
                    firmware.set, firmware.board, firmware.version, firmware.artifact
                );
                println!("{}", firmware.object_id);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("object-services: {error}");
            ExitCode::FAILURE
        }
    }
}

fn finish_import(result: Result<String, io::Error>) -> ExitCode {
    match result {
        Ok(id) => {
            println!("{id}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("object-services: {error}");
            ExitCode::FAILURE
        }
    }
}

fn fetch(config_dir: &std::path::Path, object_id: &str, file: &std::path::Path) -> ExitCode {
    match fetch_object(&socket_path(config_dir), object_id, file) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("object-services: {error}");
            ExitCode::FAILURE
        }
    }
}

fn who_has(config_dir: &std::path::Path, object_id: &str, index: u32) -> ExitCode {
    match request_who_has(&socket_path(config_dir), object_id, index) {
        Ok(holder) => {
            println!("{holder}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("object-services: {error}");
            ExitCode::FAILURE
        }
    }
}

fn release(config_dir: &std::path::Path, object_id: &str, hops: u8) -> ExitCode {
    match request_release(&socket_path(config_dir), object_id, hops) {
        Ok(()) => {
            println!("{object_id}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("object-services: {error}");
            ExitCode::FAILURE
        }
    }
}

fn transfer(config_dir: &std::path::Path, object_id: &str, destination: &str) -> ExitCode {
    match request_transfer(&socket_path(config_dir), object_id, destination) {
        Ok(()) => {
            println!("{object_id}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("object-services: {error}");
            ExitCode::FAILURE
        }
    }
}

fn bind_listener(socket: &std::path::Path) -> io::Result<UnixListener> {
    bind_socket(socket)
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
