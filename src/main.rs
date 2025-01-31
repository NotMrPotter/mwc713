#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate prettytable;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_json;
#[macro_use]
extern crate gotham_derive;
#[macro_use]
extern crate clap;
extern crate env_logger;
extern crate blake2_rfc;
extern crate chrono;
extern crate ansi_term;
extern crate colored;
extern crate digest;
extern crate failure;
extern crate futures;
extern crate gotham;
extern crate hmac;
extern crate hyper;
extern crate mime;
extern crate parking_lot;
extern crate rand;
extern crate regex;
extern crate ring;
extern crate ripemd160;
extern crate rpassword;
extern crate rustyline;
extern crate serde;
extern crate sha2;
extern crate tokio;
extern crate url;
extern crate uuid;
extern crate ws;
extern crate semver;
extern crate commands;
extern crate enquote;

extern crate grin_api;
#[macro_use]
extern crate grin_core;
extern crate grin_keychain;
extern crate grin_store;
extern crate grin_util;
extern crate grin_p2p;

use std::borrow::Cow::{self, Borrowed, Owned};
use std::fs::File;
use std::io;
use std::io::{Read, Write};
use std::path::Path;

use clap::{App, Arg, ArgMatches, SubCommand};
use colored::*;
use grin_core::core;
use grin_core::libtx::tx_fee;
use grin_core::global::{set_mining_mode, ChainTypes};
use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::config::OutputStreamType;
use rustyline::error::ReadlineError;
use rustyline::highlight::{Highlighter, MatchingBracketHighlighter};
use rustyline::hint::Hinter;
use rustyline::{CompletionType, Config, EditMode, Editor, Helper};
use url::Url;

#[macro_use]
mod common;
mod api;
mod broker;
mod cli;
mod contacts;
mod wallet;

use api::router::{build_foreign_api_router, build_owner_api_router};
use cli::Parser;
use common::config::Wallet713Config;
use common::{ErrorKind, Result, RuntimeMode, COLORED_PROMPT, PROMPT, post};
use wallet::Wallet;

use crate::wallet::types::{Arc, Mutex, Slate, TxProof};
use crate::common::motd::get_motd;

use contacts::{Address, AddressBook, AddressType, Backend, Contact, GrinboxAddress};

const CLI_HISTORY_PATH: &str = ".history";
static mut RECV_ACCOUNT: Option<String> = None;
static mut RECV_PASS: Option<String> = None;

fn do_config(
    args: &ArgMatches,
    chain: &Option<ChainTypes>,
    silent: bool,
    new_address_index: Option<u32>,
    config_path: Option<&str>,
) -> Result<Wallet713Config> {
    let mut config;
    let mut any_matches = false;
    let exists = Wallet713Config::exists(config_path, &chain)?;
    if exists {
        config = Wallet713Config::from_file(config_path, &chain)?;
    } else {
        config = Wallet713Config::default(&chain)?;
    }

    if let Some(data_path) = args.value_of("data-path") {
        config.wallet713_data_path = data_path.to_string();
        any_matches = true;
    }

    if let Some(domain) = args.value_of("domain") {
        config.mwcmq_domain = domain.to_string();
        any_matches = true;
    }

    if let Some(port) = args.value_of("port") {
        let port = u16::from_str_radix(port, 10).map_err(|_| ErrorKind::NumberParsingError)?;
        config.mwcmq_port = Some(port);
        any_matches = true;
    }

    if let Some(node_uri) = args.value_of("node-uri") {
        config.grin_node_uri = Some(node_uri.to_string());
        any_matches = true;
    }

    if let Some(node_secret) = args.value_of("node-secret") {
        config.grin_node_secret = Some(node_secret.to_string());
        any_matches = true;
    }

    if new_address_index.is_some() {
        config.grinbox_address_index = new_address_index;
        any_matches = true;
    }

    config.to_file(config_path)?;

    if !any_matches && !silent {
        cli_message!("{}", config);
    }

    Ok(config)
}

fn do_contacts(args: &ArgMatches, address_book: Arc<Mutex<AddressBook>>) -> Result<()> {
    let mut address_book = address_book.lock();
    if let Some(add_args) = args.subcommand_matches("add") {
        let name = add_args.value_of("name").expect("missing argument: name");
        let address = add_args
            .value_of("address")
            .expect("missing argument: address");

        // try parse as a general address and fallback to mwcmq address
        let contact_address = Address::parse(address);
        let contact_address: Result<Box<Address>> = match contact_address {
            Ok(address) => Ok(address),
            Err(e) => {
                Ok(Box::new(GrinboxAddress::from_str(address).map_err(|_| e)?) as Box<Address>)
            }
        };

        let contact = Contact::new(name, contact_address?)?;
        address_book.add_contact(&contact)?;
    } else if let Some(add_args) = args.subcommand_matches("remove") {
        let name = add_args.value_of("name").unwrap();
        address_book.remove_contact(name)?;
    } else {
        let contacts: Vec<()> = address_book
            .contacts()
            .map(|contact| {
                cli_message!("@{} = {}", contact.get_name(), contact.get_address());
                ()
            })
            .collect();

        if contacts.len() == 0 {
            cli_message!(
                "your contact list is empty. consider using `contacts add` to add a new contact."
            );
        }
    }
    Ok(())
}

const WELCOME_FOOTER: &str = r#"Use `help` to see available commands
"#;

fn welcome(args: &ArgMatches, runtime_mode: &RuntimeMode) -> Result<Wallet713Config> {
    let chain: Option<ChainTypes> = match args.is_present("floonet") {
        true => Some(ChainTypes::Floonet),
        false => Some(ChainTypes::Mainnet),
    };

    unsafe {
        common::set_runtime_mode(runtime_mode);
    };

    let config = do_config(args, &chain, true, None, args.value_of("config-path"))?;
    set_mining_mode(config.chain.clone().unwrap_or(ChainTypes::Mainnet));

    Ok(config)
}

use broker::{
    CloseReason, GrinboxPublisher, GrinboxSubscriber, KeybasePublisher, KeybaseSubscriber,
    Publisher, Subscriber, SubscriptionHandler,
};
use std::borrow::Borrow;

struct Controller {
    name: String,
    wallet: Arc<Mutex<Wallet>>,
    address_book: Arc<Mutex<AddressBook>>,
    publisher: Box<Publisher + Send>,
}

impl Controller {
    pub fn new(
        name: &str,
        wallet: Arc<Mutex<Wallet>>,
        address_book: Arc<Mutex<AddressBook>>,
        publisher: Box<Publisher + Send>,
    ) -> Result<Self> {
        Ok(Self {
            name: name.to_string(),
            wallet,
            address_book,
            publisher,
        })
    }

    fn process_incoming_slate(
        &self,
        address: Option<String>,
        slate: &mut Slate,
        tx_proof: Option<&mut TxProof>,
        config: Option<Wallet713Config>,
    ) -> Result<bool> {
        if slate.num_participants > slate.participant_data.len() {
            //TODO: this needs to be changed to properly figure out if this slate is an invoice or a send
            if slate.tx.inputs().len() == 0 {
                self.wallet.lock().process_receiver_initiated_slate(slate)?;
            } else {
                let mut w = self.wallet.lock();
                let old_account = w.active_account.clone();

                unsafe {
                    if config.is_some() && RECV_ACCOUNT.is_some() {
                        if RECV_PASS.is_some() {
                            w.unlock(&config.clone().unwrap(), &RECV_ACCOUNT.clone().unwrap(), &RECV_PASS.clone().unwrap())?;
                        } 
                        else
                        {
                            w.unlock(&config.clone().unwrap(), &RECV_ACCOUNT.clone().unwrap(), &"".to_string())?;
                        }
                    }
                    w.process_sender_initiated_slate(address, slate)?;

                    if config.is_some() && RECV_ACCOUNT.is_some() {
                        if RECV_PASS.is_some() {
                            w.unlock(&config.unwrap(), &old_account, &RECV_PASS.clone().unwrap())?;
                        }
                        else {
                            w.unlock(&config.unwrap(), &old_account, &"".to_string())?;
                        }
                    }
                }
            }
            Ok(false)
        } else {
            self.wallet.lock().finalize_slate(slate, tx_proof)?;
            Ok(true)
        }
    }
}

impl SubscriptionHandler for Controller {
    fn on_open(&self) {
        cli_message!("listener started for [{}]", self.name.bright_green());
    }

    fn on_slate(&self, from: &Address, slate: &mut Slate, tx_proof: Option<&mut TxProof>, config: Option<Wallet713Config>) {
        let mut display_from = from.stripped();
        if let Ok(contact) = self
            .address_book
            .lock()
            .get_contact_by_address(&from.to_string())
        {
            display_from = contact.get_name().to_string();
        }

        if slate.num_participants > slate.participant_data.len() {
            let message = &slate.participant_data[0].message;
            if message.is_some() {
                cli_message!(
                    "slate [{}] received from [{}] for [{}] MWCs. Message: [\"{}\"]",
                    slate.id.to_string().bright_green(),
                    display_from.bright_green(),
                    core::amount_to_hr_string(slate.amount, false).bright_green(),
                    message.clone().unwrap().bright_green()
                );
            }
            else {
                cli_message!(
                    "slate [{}] received from [{}] for [{}] MWCs.",
                    slate.id.to_string().bright_green(),
                    display_from.bright_green(),
                    core::amount_to_hr_string(slate.amount, false).bright_green()
                );
            }
        } else {
            cli_message!(
                "slate [{}] received back from [{}] for [{}] MWCs",
                slate.id.to_string().bright_green(),
                display_from.bright_green(),
                core::amount_to_hr_string(slate.amount, false).bright_green()
            );
        };

        if from.address_type() == AddressType::Grinbox {
            GrinboxAddress::from_str(&from.to_string()).expect("invalid mwcmq address");
        }

        let result = self
            .process_incoming_slate(Some(from.to_string()), slate, tx_proof, config)
            .and_then(|is_finalized| {
                if !is_finalized {
                    self.publisher
                        .post_slate(slate, from)
                        .map_err(|e| {
                            cli_message!("{}: {}", "ERROR".bright_red(), e);
                            e
                        })
                        .expect("failed posting slate!");
                    cli_message!(
                        "slate [{}] sent back to [{}] successfully",
                        slate.id.to_string().bright_green(),
                        display_from.bright_green()
                    );
                } else {
                    cli_message!(
                        "slate [{}] finalized successfully",
                        slate.id.to_string().bright_green()
                    );
                }
                Ok(())
            });

        match result {
            Ok(()) => {}
            Err(e) => cli_message!("{}", e),
        }
    }

    fn on_close(&self, reason: CloseReason) {
        match reason {
            CloseReason::Normal => cli_message!("listener [{}] stopped", self.name.bright_green()),
            CloseReason::Abnormal(_) => cli_message!(
                "{}: listener [{}] stopped unexpectedly",
                "ERROR".bright_red(),
                self.name.bright_green()
            ),
        }
    }

    fn on_dropped(&self) {
        cli_message!("{}: listener [{}] lost connection. it will keep trying to restore connection in the background.", "WARNING".bright_yellow(), self.name.bright_green())
    }

    fn on_reestablished(&self) {
        cli_message!(
            "{}: listener [{}] reestablished connection.",
            "INFO".bright_blue(),
            self.name.bright_green()
        )
    }
}

fn start_grinbox_listener(
    config: &Wallet713Config,
    wallet: Arc<Mutex<Wallet>>,
    address_book: Arc<Mutex<AddressBook>>,
) -> Result<(GrinboxPublisher, GrinboxSubscriber, std::thread::JoinHandle<()>)> {
    // make sure wallet is not locked, if it is try to unlock with no passphrase
    {
        let mut wallet = wallet.lock();
        if wallet.is_locked() {
            wallet.unlock(config, "default", "")?;
        }
    }

    cli_message!("starting mwcmq listener...");
    let grinbox_address = config.get_grinbox_address()?;
    let grinbox_secret_key = config.get_grinbox_secret_key()?;

    let grinbox_publisher = GrinboxPublisher::new(
        &grinbox_address,
        &grinbox_secret_key,
        config.grinbox_protocol_unsecure(),
        config,
    )?;

    let grinbox_subscriber = GrinboxSubscriber::new(
        &grinbox_publisher
    )?;

    let cloned_publisher = grinbox_publisher.clone();
    let mut cloned_subscriber = grinbox_subscriber.clone();
    let grinbox_listener_handle = std::thread::spawn(move || {
        let controller = Controller::new(
            &grinbox_address.stripped(),
            wallet.clone(),
            address_book.clone(),
            Box::new(cloned_publisher),
        )
        .expect("could not start mwcmq controller!");
        cloned_subscriber
            .start(Box::new(controller))
            .expect("something went wrong!");
    });
    Ok((grinbox_publisher, grinbox_subscriber, grinbox_listener_handle))
}

fn start_keybase_listener(
    config: &Wallet713Config,
    wallet: Arc<Mutex<Wallet>>,
    address_book: Arc<Mutex<AddressBook>>,
) -> Result<(KeybasePublisher, KeybaseSubscriber, std::thread::JoinHandle<()>)> {
    // make sure wallet is not locked, if it is try to unlock with no passphrase
    {
        let mut wallet = wallet.lock();
        if wallet.is_locked() {
            wallet.unlock(config, "default", "")?;
        }
    }

    cli_message!("starting keybase listener...");
    let keybase_subscriber = KeybaseSubscriber::new(config.keybase_binary.clone())?;
    let keybase_publisher = KeybasePublisher::new(config.default_keybase_ttl.clone(),
                                                  config.keybase_binary.clone())?;

    let mut cloned_subscriber = keybase_subscriber.clone();
    let cloned_publisher = keybase_publisher.clone();
    let keybase_listener_handle = std::thread::spawn(move || {
        let controller = Controller::new(
            "keybase",
            wallet.clone(),
            address_book.clone(),
            Box::new(cloned_publisher),
        )
        .expect("could not start keybase controller!");
        cloned_subscriber
            .start(Box::new(controller))
            .expect("something went wrong!");
    });
    Ok((keybase_publisher, keybase_subscriber, keybase_listener_handle))
}

struct EditorHelper(FilenameCompleter, MatchingBracketHighlighter);

impl Completer for EditorHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
    ) -> std::result::Result<(usize, Vec<Pair>), ReadlineError> {
        self.0.complete(line, pos)
    }
}

impl Hinter for EditorHelper {
    fn hint(&self, _line: &str, _pos: usize) -> Option<String> {
        None
    }
}

impl Highlighter for EditorHelper {
    fn highlight_prompt<'p>(&self, prompt: &'p str) -> Cow<'p, str> {
        if prompt == PROMPT {
            Borrowed(COLORED_PROMPT)
        } else {
            Borrowed(prompt)
        }
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Owned("\x1b[1m".to_owned() + hint + "\x1b[m")
    }

    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        self.1.highlight(line, pos)
    }

    fn highlight_char(&self, line: &str, pos: usize) -> bool {
        self.1.highlight_char(line, pos)
    }
}

impl Helper for EditorHelper {}

fn main() {
    enable_ansi_support();

    let matches = App::new("mwc713")
        .version(crate_version!())
        .arg(Arg::from_usage("[config-path] -c, --config=<config-path> 'the path to the config file'"))
        .arg(Arg::from_usage("[log-config-path] -l, --log-config-path=<log-config-path> 'the path to the log config file'"))
        .arg(Arg::from_usage("[account] -a, --account=<account> 'the account to use'"))
        .arg(Arg::from_usage("[passphrase] -p, --passphrase=<passphrase> 'the passphrase to use'"))
        .arg(Arg::from_usage("[daemon] -d, --daemon 'run daemon'"))
        .arg(Arg::from_usage("[floonet] -f, --floonet 'use floonet'"))
        .arg(Arg::from_usage("[ready-phrase] -r, --ready-phrase=<phrase> 'use additional ready phrase printed when wallet ready to read input'"))
        .subcommand(SubCommand::with_name("init").about("initializes the wallet"))
        .subcommand(
            SubCommand::with_name("recover")
                .about("recover wallet from mnemonic or displays the current mnemonic")
                .arg(Arg::from_usage("[words] -m, --mnemonic=<words>... 'the seed mnemonic'"))
        )
        .subcommand(SubCommand::with_name("state").about("print wallet initialization state and exit"))
        .get_matches();

    let runtime_mode = match matches.is_present("daemon") {
        true => RuntimeMode::Daemon,
        false => RuntimeMode::Cli,
    };

    let mut config: Wallet713Config = welcome(&matches, &runtime_mode).unwrap_or_else(|e| {
        panic!(
            "{}: could not read or create config! {}",
            "ERROR".bright_red(),
            e
        );
    });

    if runtime_mode == RuntimeMode::Daemon {
        env_logger::init();
    }

    let data_path_buf = config.get_data_path().unwrap();
    let data_path = data_path_buf.to_str().unwrap();

    let address_book_backend =
        Backend::new(data_path).expect("could not create address book backend!");
    let address_book = AddressBook::new(Box::new(address_book_backend))
        .expect("could not create an address book!");
    let address_book = Arc::new(Mutex::new(address_book));

    println!("{}", format!("\nWelcome to wallet713 for MWC v{}\n", crate_version!()).bright_yellow().bold());

    if config.check_updates() {
        get_motd().unwrap_or(());
    }

    let wallet = Wallet::new(config.max_auto_accept_invoice);
    let wallet = Arc::new(Mutex::new(wallet));

    let mut grinbox_broker: Option<(GrinboxPublisher, GrinboxSubscriber)> = None;
    let mut keybase_broker: Option<(KeybasePublisher, KeybaseSubscriber)> = None;

    let has_seed = Wallet::seed_exists(&config);

    // TODO: print something nicer for the user
    if matches.subcommand_matches("state").is_some() {
        match has_seed {
            true => println!("Initialized"),
            false => println!("Uninitialized")
        };
        std::process::exit();
    }

    if !has_seed {
        let mut line = String::new();

        if matches.subcommand_matches("init").is_some() {
            line = "init".to_string();
        }
        if matches.subcommand_matches("recover").is_some() {
            line = "recover".to_string();
        }
        if line == String::new() {
            println!("{}", "Please choose an option".bright_green().bold());
            println!(" 1) {} a new wallet", "init".bold());
            println!(" 2) {} from mnemonic", "recover".bold());
            println!(" 3) {}", "exit".bold());
            println!();
            print!("{}", "> ".cyan());
            io::stdout().flush().unwrap();

            if io::stdin().read_line(&mut line).unwrap() == 0 {
                println!("{}: invalid option", "ERROR".bright_red());
                std::process::exit(1);
            }

            println!();
        }

        let passphrase = if matches.is_present("passphrase") {
            matches.value_of("passphrase").unwrap()
        } else {
            ""
        };

        let line = line.trim();
        let mut out_is_safe = false;
        match line {
            "1" | "init" | "" => {
                println!("{}", "Initialising a new wallet".bold());
                println!();
                println!("Set an optional password to secure your wallet with. Leave blank for no password.");
                println!();
                let cmd = format!("init -p {}", &passphrase);
                if let Err(err) = do_command(&cmd, &mut config, wallet.clone(), address_book.clone(), &mut keybase_broker, &mut grinbox_broker, &mut out_is_safe) {
                    println!("{}: {}", "ERROR".bright_red(), err);
                    std::process::exit(1);
                }
            },
            "2" | "recover" | "restore" => {
                println!("{}", "Recovering from mnemonic".bold());
                print!("Mnemonic: ");
                io::stdout().flush().unwrap();
                let mut mnemonic = String::new();

                if let Some(recover) = matches.subcommand_matches("recover") {
                    if recover.is_present("words") {
                        mnemonic = matches.subcommand_matches("recover").unwrap().value_of("words").unwrap().to_string();
                    }
                } else {
                    if io::stdin().read_line(&mut mnemonic).unwrap() == 0 {
                        println!("{}: invalid mnemonic", "ERROR".bright_red());
                        std::process::exit(1);
                    }
                    mnemonic = mnemonic.trim().to_string();
                };

                println!();
                println!("Set an optional password to secure your wallet with. Leave blank for no password.");
                println!();
                // TODO: refactor this
                let cmd = format!("recover -m {} -p {}", mnemonic, &passphrase);
                if let Err(err) = do_command(&cmd, &mut config, wallet.clone(), address_book.clone(), &mut keybase_broker, &mut grinbox_broker, &mut out_is_safe) {
                    println!("{}: {}", "ERROR".bright_red(), err);
                    std::process::exit(1);
                }
            },
            "3" | "exit" => {
                return;
            },
            _ => {
                println!("{}: invalid option", "ERROR".bright_red());
                std::process::exit(1);
            },
        }

        println!();
    } else {
        if matches.subcommand_matches("init").is_some() {
            println!("Seed file already exists! Not initializing");
            std::process::exit(1);
        }
        if matches.subcommand_matches("recover").is_some() {
            println!("Seed file already exists! Not recovering");
            std::process::exit(1);
        }
    }

    if wallet.lock().is_locked() {
        let account = matches.value_of("account").unwrap_or("default").to_string();
        let has_wallet = if matches.is_present("passphrase") {
            let passphrase = password_prompt(matches.value_of("passphrase"));
            let result = wallet.lock().unlock(&config, &account, &passphrase);
            if let Err(ref err) = result {
                println!("{}: {}", "ERROR".bright_red(), err);
                std::process::exit(1);
            }
            result.is_ok()
        }
        else {
            wallet.lock().unlock(&config, &account, "").is_ok()
        };

        if has_wallet {
            let der = derive_address_key(&mut config, wallet.clone(), &mut grinbox_broker);
            if der.is_err() {
                cli_message!("{}: {}", "ERROR".bright_red(), der.unwrap_err());
            }
        }
        else {
            println!(
                "{}",
                "Unlock your existing wallet or type `init` to initiate a new one"
                    .bright_blue()
                    .bold()
            );
        }
    }

    cli_message!("{}", WELCOME_FOOTER.bright_blue());

    let mut grinbox_listener_handle: Option<std::thread::JoinHandle<()>> = None;
    let mut keybase_listener_handle: Option<std::thread::JoinHandle<()>> = None;
    let mut owner_api_handle: Option<std::thread::JoinHandle<()>> = None;
    let mut foreign_api_handle: Option<std::thread::JoinHandle<()>> = None;

    if config.grinbox_listener_auto_start() {
        let result = start_grinbox_listener(&config, wallet.clone(), address_book.clone());
        match result {
            Err(e) => cli_message!("{}: {}", "ERROR".bright_red(), e),
            Ok((publisher, subscriber, handle)) => {
                grinbox_broker = Some((publisher, subscriber));
                grinbox_listener_handle = Some(handle);
            },
        }
    }

    if config.keybase_listener_auto_start() {
        let result = start_keybase_listener(&config, wallet.clone(), address_book.clone());
        match result {
            Err(e) => cli_message!("{}: {}", "ERROR".bright_red(), e),
            Ok((publisher, subscriber, handle)) => {
                keybase_broker = Some((publisher, subscriber));
                keybase_listener_handle = Some(handle);
            },
        }
    }

    if config.owner_api() || config.foreign_api() {
        owner_api_handle = match config.owner_api {
            Some(true) => {
                cli_message!(
                    "starting listener for owner api on [{}]",
                    config.owner_api_address().bright_green()
                );
                if config.owner_api_secret.is_none() {
                    cli_message!(
                        "{}: no api secret for owner api, it is recommended to set one.",
                        "WARNING".bright_yellow()
                    );
                }
                let router = build_owner_api_router(
                    wallet.clone(),
                    grinbox_broker.clone(),
                    keybase_broker.clone(),
                    config.owner_api_secret.clone(),
                    config.owner_api_include_foreign,
                );
                let address = config.owner_api_address();
                Some(std::thread::spawn(move || {
                    gotham::start(address, router);
                }))
            }
            _ => None,
        };

        foreign_api_handle = match config.foreign_api {
            Some(true) => {
                cli_message!(
                    "starting listener for foreign api on [{}]",
                    config.foreign_api_address().bright_green()
                );
                if config.foreign_api_secret.is_none() {
                    cli_message!(
                        "{}: no api secret for foreign api, it is recommended to set one.",
                        "WARNING".bright_yellow()
                    );
                }
                let router = build_foreign_api_router(
                    wallet.clone(),
                    grinbox_broker.clone(),
                    keybase_broker.clone(),
                    config.foreign_api_secret.clone(),
                );
                let address = config.foreign_api_address();
                Some(std::thread::spawn(move || {
                    gotham::start(address, router);
                }))
            }
            _ => None,
        };
    };

    if runtime_mode == RuntimeMode::Daemon {
        let mut listening = false;
        if let Some(handle) = grinbox_listener_handle {
            handle.join().unwrap();
            listening = true;
        }

        if let Some(handle) = keybase_listener_handle {
            handle.join().unwrap();
            listening = true;
        }

        if let Some(handle) = owner_api_handle {
            handle.join().unwrap();
            listening = true;
        }

        if let Some(handle) = foreign_api_handle {
            handle.join().unwrap();
            listening = true;
        }

        if !listening {
            warn!("no listener configured, exiting");
        }

        return;
    }

    let editor_config = Config::builder()
        .history_ignore_space(true)
        .completion_type(CompletionType::List)
        .edit_mode(EditMode::Emacs)
        .output_stream(OutputStreamType::Stdout)
        .build();
    let mut rl = Editor::with_config(editor_config);
    rl.set_helper(Some(EditorHelper(
        FilenameCompleter::new(),
        MatchingBracketHighlighter::new(),
    )));

    let wallet713_home_path_buf = Wallet713Config::default_home_path(&config.chain).unwrap();
    let wallet713_home_path = wallet713_home_path_buf.to_str().unwrap();

    if let Some(path) = Path::new(wallet713_home_path)
        .join(CLI_HISTORY_PATH)
        .to_str()
    {
        rl.load_history(path).is_ok();
    }

    let prompt_plus = matches.value_of("ready-phrase").unwrap_or("").to_string();

    loop {
        if ! prompt_plus.is_empty() {
            println!("{}", prompt_plus);
        }
        let command = rl.readline(PROMPT);
        match command {
            Ok(command) => {
                let command = command.trim();

                if command == "exit" {
                    break;
                }

                let mut out_is_safe = false;
                let result = do_command(
                    &command,
                    &mut config,
                    wallet.clone(),
                    address_book.clone(),
                    &mut keybase_broker,
                    &mut grinbox_broker,
                    &mut out_is_safe,
                );

                if let Err(err) = result {
                    cli_message!("{}", err);
                }

                if out_is_safe {
                    rl.add_history_entry(command);
                }
            }
            Err(_) => {
                break;
            }
        }
    }

    if let Some(path) = Path::new(wallet713_home_path)
        .join(CLI_HISTORY_PATH)
        .to_str()
    {
        rl.save_history(path).is_ok();
    }
}

fn derive_address_key(
    config: &mut Wallet713Config,
    wallet: Arc<Mutex<Wallet>>,
    grinbox_broker: &mut Option<(GrinboxPublisher, GrinboxSubscriber)>,
) -> Result<()> {
    if grinbox_broker.is_some() {
        return Err(ErrorKind::HasListener.into());
    }
    let index = config.grinbox_address_index();
    let key = wallet.lock().derive_address_key(index)?;
    config.grinbox_address_key = Some(key);
    show_address(config, false)?;
    Ok(())
}

fn show_address(config: &Wallet713Config, include_index: bool) -> Result<()> {
    println!(
        "{}: {}",
        "Your mwcmq address".bright_yellow(),
        config.get_grinbox_address()?.stripped().bright_green()
    );
    if include_index {
        println!(
            "Derived with index [{}]",
            config.grinbox_address_index().to_string().bright_blue()
        );
    }
    Ok(())
}

fn password_prompt(opt: Option<&str>) -> String {
    opt.map(String::from).unwrap_or_else(|| {
        rpassword::prompt_password_stdout("Password: ").unwrap_or(String::from(""))
    })
}

fn proof_ok(
    sender: Option<String>,
    receiver: String,
    amount: u64,
    outputs: Vec<String>,
    kernel: String,
) {
    let sender_message = sender
        .as_ref()
        .map(|s| format!(" from [{}]", s.bright_green()))
        .unwrap_or(String::new());

    println!(
        "this file proves that [{}] MWCs was sent to [{}]{}",
        core::amount_to_hr_string(amount, false).bright_green(),
        receiver.bright_green(),
        sender_message
    );

    if sender.is_none() {
        println!(
            "{}: this proof does not prove which address sent the funds, only which received it",
            "WARNING".bright_yellow()
        );
    }

    println!("\noutputs:");
    for output in outputs {
        println!("   {}: https://explorer.mwc.mw/#o{}", output.bright_magenta(), output);
    }
    println!("kernel:");
    println!("   {}: https://explorer.mwc.mw/#k{}", kernel.bright_magenta(), kernel);
    println!("\n{}: this proof should only be considered valid if the kernel is actually on-chain with sufficient confirmations", "WARNING".bright_yellow());
    println!("please use a mwc block explorer to verify this is the case.");
}

fn do_command(
    command: &str,
    config: &mut Wallet713Config,
    wallet: Arc<Mutex<Wallet>>,
    address_book: Arc<Mutex<AddressBook>>,
    keybase_broker: &mut Option<(KeybasePublisher, KeybaseSubscriber)>,
    grinbox_broker: &mut Option<(GrinboxPublisher, GrinboxSubscriber)>,
    out_is_safe: &mut bool,
) -> Result<()> {
    *out_is_safe = true;
    let home_dir = dirs::home_dir()
        .map(|p| p.to_str().unwrap().to_string())
        .unwrap_or("~".to_string());

    let matches = Parser::parse(command)?;
    match matches.subcommand_name() {
        Some("config") => {
            let args = matches.subcommand_matches("config").unwrap();

            let new_address_index = match args.is_present("generate-address") {
                false => None,
                true => Some({
                    let index = match args.value_of("generate-address-index") {
                        Some(index) => u32::from_str_radix(index, 10)
                            .map_err(|_| ErrorKind::NumberParsingError)?,
                        None => config.grinbox_address_index() + 1,
                    };
                    config.grinbox_address_index = Some(index);
                    index
                }),
            };

            *config = do_config(
                args,
                &config.chain,
                false,
                new_address_index,
                config.config_home.as_ref().map(|x| &**x),
            )?;

            if new_address_index.is_some() {
                derive_address_key(config, wallet, grinbox_broker)?;
                cli_message!(
                    "Derived with index [{}]",
                    config.grinbox_address_index().to_string().bright_blue()
                );
            }
        }
        Some("address") => {
            show_address(config, true)?;
        }
        Some("init") => {
            *out_is_safe = false;
            if keybase_broker.is_some() || grinbox_broker.is_some() {
                return Err(ErrorKind::HasListener.into());
            }
            let args = matches.subcommand_matches("init").unwrap();
            let passphrase = match args.is_present("passphrase") {
                true => password_prompt(args.value_of("passphrase")),
                false => "".to_string(),
            };
            *out_is_safe = args.value_of("passphrase").is_none();

            if passphrase.is_empty() {
                println!("{}: wallet with no passphrase.", "WARNING".bright_yellow());
            }

            let seed = wallet
                .lock()
                .init(config, passphrase.as_str(), true)?;

            println!("{}", "Press ENTER when you have done so".bright_green().bold());

            let mut line = String::new();
            io::stdout().flush().unwrap();
            io::stdin().read_line(&mut line).unwrap();

            wallet.lock().complete(seed, config, "default", passphrase.as_str(), true)?;

            derive_address_key(config, wallet, grinbox_broker)?;
            return Ok(());
        }
        Some("lock") => {
            if keybase_broker.is_some() || grinbox_broker.is_some() {
                return Err(ErrorKind::HasListener.into());
            }
            wallet.lock().lock();
        }
        Some("unlock") => {
            let args = matches.subcommand_matches("unlock").unwrap();
            let account = args.value_of("account").unwrap_or("default");
            let passphrase = match args.is_present("passphrase") {
                true => password_prompt(args.value_of("passphrase")),
                false => "".to_string(),
            };
            *out_is_safe = args.value_of("passphrase").is_none();

            {
                let mut w = wallet.lock();
                if !w.is_locked() {
                    return Err(ErrorKind::WalletAlreadyUnlocked.into());
                }
                w.unlock(config, account, passphrase.as_str())?;
            }

            derive_address_key(config, wallet, grinbox_broker)?;
            return Ok(());
        }
        Some("accounts") => {
            wallet.lock().list_accounts()?;
        }
        Some("account") => {
            let args = matches.subcommand_matches("account").unwrap();
            *out_is_safe = args.value_of("passphrase").is_none();

            let create_args = args.subcommand_matches("create");
            let switch_args = args.subcommand_matches("switch");
            let rename_args = args.subcommand_matches("rename");
            if let Some(args) = create_args {
                wallet
                    .lock()
                    .create_account(args.value_of("name").unwrap())?;
            } else if let Some(args) = switch_args {
                let account = args.value_of("name").unwrap();
                let passphrase = match args.is_present("passphrase") {
                    true => password_prompt(args.value_of("passphrase")),
                    false => "".to_string(),
                };
                wallet.lock().unlock(config, account, passphrase.as_str())?;
            } else if let Some(args) = rename_args {
                let old_account = args.value_of("old_account").unwrap();
                let new_account = args.value_of("new_account").unwrap();
                wallet.lock().rename_account(old_account, new_account)?;
            }

            return Ok(());
        }
        Some("listen") => {
            let grinbox = matches
                .subcommand_matches("listen")
                .unwrap()
                .is_present("mwcmq");
            let keybase = matches
                .subcommand_matches("listen")
                .unwrap()
                .is_present("keybase");
            if grinbox || !keybase {
                let is_running = match grinbox_broker {
                    Some((_, subscriber)) => subscriber.is_running(),
                    _ => false,
                };
                if is_running {
                    Err(ErrorKind::AlreadyListening("mwcmq".to_string()))?
                } else {
                    let (publisher, subscriber, _) =
                        start_grinbox_listener(config, wallet.clone(), address_book.clone())?;
                    *grinbox_broker = Some((publisher, subscriber));
                }
            }
            if keybase {
                let is_running = match keybase_broker {
                    Some((_, subscriber)) => subscriber.is_running(),
                    _ => false,
                };
                if is_running {
                    Err(ErrorKind::AlreadyListening("keybase".to_string()))?
                } else {
                    let (publisher, subscriber, _) =
                        start_keybase_listener(config, wallet.clone(), address_book.clone())?;
                    *keybase_broker = Some((publisher, subscriber));
                }
            }
        }
        Some("stop") => {
            let grinbox = matches
                .subcommand_matches("stop")
                .unwrap()
                .is_present("mwcmq");
            let keybase = matches
                .subcommand_matches("stop")
                .unwrap()
                .is_present("keybase");
            if grinbox || !keybase {
                let is_running = match grinbox_broker {
                    Some((_, subscriber)) => subscriber.is_running(),
                    _ => false,
                };
                if is_running {
                    cli_message!("stopping mwcmq listener...");
                    if let Some((_, subscriber)) = grinbox_broker {
                        subscriber.stop();
                    };
                    *grinbox_broker = None;
                } else {
                    Err(ErrorKind::ClosedListener("mwcmq".to_string()))?
                }
            }
            if keybase {
                let is_running = match keybase_broker {
                    Some((_, subscriber)) => subscriber.is_running(),
                    _ => false,
                };
                if is_running {
                    cli_message!("stopping keybase listener...");
                    if let Some((_, subscriber)) = keybase_broker {
                        subscriber.stop();
                    };
                    *keybase_broker = None;
                } else {
                    Err(ErrorKind::ClosedListener("keybase".to_string()))?
                }
            }
        }
        Some("info") => {
            let args = matches.subcommand_matches("info").unwrap();

            let confirmations = args.value_of("confirmations").unwrap_or("10");
            let confirmations = u64::from_str_radix(confirmations, 10)
                .map_err(|_| ErrorKind::InvalidMinConfirmations(confirmations.to_string()))?;

            wallet.lock().info(!args.is_present("--no-refresh"), confirmations)?;
        }
        Some("txs_count") => {
            let count = wallet.lock().txs_count()?;
            cli_message!("{:?}", count);
        }
        Some("txs") => {
            let args = matches.subcommand_matches("txs").unwrap();

            // get pagination parameters default is to not do pagination when length == 0.
            let pagination_length = args.value_of("length").unwrap_or("0");
            let pagination_start = args.value_of("offset").unwrap_or("0");

            let pagination_length = u32::from_str_radix(pagination_length, 10)
                .map_err(|_| ErrorKind::InvalidPaginationLength(pagination_length.to_string()))?;

            let pagination_start = u32::from_str_radix(pagination_start, 10)
                .map_err(|_| ErrorKind::InvalidPaginationStart(pagination_length.to_string()))?;

            wallet.lock().txs(Some(address_book.clone()), pagination_start, pagination_length)?;
        }
        Some("contacts") => {
            let arg_matches = matches.subcommand_matches("contacts").unwrap();
            do_contacts(&arg_matches, address_book.clone())?;
        }
        Some("output_count") => {
            let args = matches.subcommand_matches("output_count").unwrap();
            let show_spent = args.is_present("show-spent");
            let all_outputs = wallet.lock().all_output_count(show_spent)?;
            cli_message!("{:?}", all_outputs);
        }
        Some("outputs") => {
            let args = matches.subcommand_matches("outputs").unwrap();

            // get pagination parameters default is to not do pagination when length == 0.
            let pagination_length = args.value_of("length").unwrap_or("0");
            let pagination_start = args.value_of("offset").unwrap_or("0");

            let pagination_length = u32::from_str_radix(pagination_length, 10)
                .map_err(|_| ErrorKind::InvalidPaginationLength(pagination_length.to_string()))?;

            let pagination_start = u32::from_str_radix(pagination_start, 10)
                .map_err(|_| ErrorKind::InvalidPaginationStart(pagination_length.to_string()))?;

            let show_spent = args.is_present("show-spent");
            wallet.lock().outputs(show_spent, pagination_start, pagination_length)?;
        }
        Some("repost") => {
            let args = matches.subcommand_matches("repost").unwrap();
            let id = args.value_of("id").unwrap();
            let id = id
                .parse::<u32>()
                .map_err(|_| ErrorKind::InvalidTxId(id.to_string()))?;
            wallet.lock().repost(id, false)?;
        }
        Some("cancel") => {
            let args = matches.subcommand_matches("cancel").unwrap();
            let id = args.value_of("id").unwrap();
            let id = id
                .parse::<u32>()
                .map_err(|_| ErrorKind::InvalidTxId(id.to_string()))?;
            wallet.lock().cancel(id)?;
        }
        Some("receive") => {
            let args = matches.subcommand_matches("receive").unwrap();
            let input = args.value_of("file").unwrap();
            let mut file = File::open(input.replace("~", &home_dir))?;
            let mut slate = String::new();
            file.read_to_string(&mut slate)?;
            let mut slate = Slate::deserialize_upgrade(&slate)?;
            let mut file = File::create(&format!("{}.response", input.replace("~", &home_dir)))?;
            wallet
                .lock()
                .process_sender_initiated_slate(Some(String::from("file")), &mut slate)?;

            let message = &slate.participant_data[0].message;
            let amount = core::amount_to_hr_string(slate.amount, false);
            if message.is_some() {
                cli_message!("{} received. amount = [{}], message = [{:?}]", input, amount, message.clone().unwrap());
            }
            else {
                cli_message!("{} received. amount = [{}]", input, amount);
            }
            file.write_all(serde_json::to_string(&slate)?.as_bytes())?;
            cli_message!("{}.response created successfully.", input);
        }
        Some("finalize") => {
            let args = matches.subcommand_matches("finalize").unwrap();
            let input = args.value_of("file").unwrap();
            let mut file = File::open(input.replace("~", &home_dir))?;
            let mut slate = String::new();
            file.read_to_string(&mut slate)?;
            let mut slate = Slate::deserialize_upgrade(&slate)?;
            wallet.lock().finalize_slate(&mut slate, None)?;
            cli_message!("{} finalized.", input);
        }
        Some("nodeinfo") => {
            wallet.lock().node_info()?;
        }
        Some("send") => {
            let args = matches.subcommand_matches("send").unwrap();
            let to = args.value_of("to");
            let input = args.value_of("file");
            let message = args.value_of("message").map(|s| s.to_string());

            let strategy = args.value_of("strategy").unwrap_or("smallest");
            if strategy != "smallest" && strategy != "all" && strategy != "custom" {
                return Err(ErrorKind::InvalidStrategy.into());
            }

            let outputs_arg = args.value_of("outputs");

            let output_list = if outputs_arg.is_none() {
                if strategy == "custom" {
                    return Err(ErrorKind::CustomWithNoOutputs.into());
                }
                None
            }
            else
            {
                if strategy != "custom" {
                    return Err(ErrorKind::NonCustomWithOutputs.into());
                }
                let ret: Vec<_> = outputs_arg.unwrap().split(",").collect();
                Some(ret)
            };

            let confirmations = args.value_of("confirmations").unwrap_or("10");
            let confirmations = u64::from_str_radix(confirmations, 10)
                .map_err(|_| ErrorKind::InvalidMinConfirmations(confirmations.to_string()))?;

            if confirmations < 1 {
                return Err(ErrorKind::ZeroConfNotAllowed.into());
            }

            let change_outputs = args.value_of("change-outputs").unwrap_or("1");
            let change_outputs = usize::from_str_radix(change_outputs, 10)
                .map_err(|_| ErrorKind::InvalidNumOutputs(change_outputs.to_string()))?;

            let version = match args.value_of("version") {
                Some(v) => Some(u16::from_str_radix(v, 10)
                    .map_err(|_| ErrorKind::InvalidSlateVersion(v.to_string()))?),
                None => None,
            };

            let amount = args.value_of("amount").unwrap();
            let mut ntotal = 0;
            if amount == "ALL" {
                let max_available = wallet.lock().output_count(confirmations, output_list.clone())?;
                let total_value = wallet.lock().total_value(confirmations, output_list.clone())?;
                let fee = tx_fee(max_available, 1, 1, None);
                ntotal = if total_value >= fee { total_value - fee } else { 0 };
            }
  
            let amount = match amount == "ALL" {
                true => ntotal,
                false => core::amount_from_hr_string(amount).map_err(|_| ErrorKind::InvalidAmount(amount.to_string()))?,
            };

            // Store slate in a file
            if let Some(input) = input {
                let mut file = File::create(input.replace("~", &home_dir))?;
                let slate = wallet.lock().initiate_send_tx(
                    Some(String::from("file")),
                    amount,
                    confirmations,
                    strategy,
                    change_outputs,
                    500,
                    message,
                    output_list,
                    version,
                )?;
                file.write_all(serde_json::to_string(&slate)?.as_bytes())?;
                cli_message!("{} created successfully.", input);
                return Ok(());
            }

            let mut to = to.unwrap().to_string();

            let mut display_to = None;
            if to.starts_with("@") {
                let contact = address_book.lock().get_contact(&to[1..])?;
                to = contact.get_address().to_string();
                display_to = Some(contact.get_name().to_string());
            }

            // try parse as a general address and fallback to mwcmq address
            let address = Address::parse(&to);
            let address: Result<Box<Address>> = match address {
                Ok(address) => Ok(address),
                Err(e) => {
                    Ok(Box::new(GrinboxAddress::from_str(&to).map_err(|_| e)?) as Box<Address>)
                }
            };

            let to = address?;
            if display_to.is_none() {
                display_to = Some(to.stripped());
            }
            let mut slate: Slate = match to.address_type() {
                AddressType::Keybase => {
                    if let Some((publisher, _)) = keybase_broker {
                        let slate = wallet.lock().initiate_send_tx(
                            Some(to.to_string()),
                            amount,
                            confirmations,
                            strategy,
                            change_outputs,
                            500,
                            message,
                            output_list,
                            version,
                        )?;
                        let mut keybase_address =
                            contacts::KeybaseAddress::from_str(&to.to_string())?;
                        keybase_address.topic = Some(broker::TOPIC_SLATE_NEW.to_string());
                        publisher.post_slate(&slate, keybase_address.borrow())?;
                        slate
                    } else {
                        return Err(ErrorKind::ClosedListener("keybase".to_string()).into());
                    }
                }
                AddressType::Grinbox => {
                    if let Some((publisher, _)) = grinbox_broker {
                        let slate = wallet.lock().initiate_send_tx(
                            Some(to.to_string()),
                            amount,
                            confirmations,
                            strategy,
                            change_outputs,
                            500,
                            message,
                            output_list,
                            version,
                        )?;
                        publisher.post_slate(&slate, to.borrow())?;
                        slate
                    } else {
                        return Err(ErrorKind::ClosedListener("mwcmq".to_string()).into());
                    }
                }
                AddressType::Https => {
                    let url =
                        Url::parse(&format!("{}/v1/wallet/foreign/receive_tx", to.to_string()))?;
                    let slate = wallet.lock().initiate_send_tx(
                        Some(to.to_string()),
                        amount,
                        confirmations,
                        strategy,
                        change_outputs,
                        500,
                        message,
                        output_list,
                        version,
                    )?;
                    let slate_res = post(url.as_str(), None, &slate)
                        .map_err(|_| ErrorKind::HttpRequest)?;
                    Slate::deserialize_upgrade(&slate_res)
                        .map_err(|_| ErrorKind::HttpRequest)?
                }
            };

            let ret_id = wallet.lock().get_id(slate.id)?;
            println!("txid={:?}", ret_id);

            cli_message!(
                    "slate [{}] for [{}] MWCs sent successfully to [{}]",
                slate.id.to_string().bright_green(),
                core::amount_to_hr_string(slate.amount, false).bright_green(),
                display_to.unwrap().bright_green()
            );

            if to.address_type() == AddressType::Https {
                wallet.lock().finalize_slate(&mut slate, None)?;
                cli_message!(
                    "slate [{}] finalized successfully",
                    slate.id.to_string().bright_green()
                );
            }
        }
        Some("invoice") => {
            let args = matches.subcommand_matches("invoice").unwrap();
            let to = args.value_of("to").unwrap();
            let outputs = args.value_of("outputs").unwrap_or("1");
            let outputs = usize::from_str_radix(outputs, 10)
                .map_err(|_| ErrorKind::InvalidNumOutputs(outputs.to_string()))?;
            let amount = args.value_of("amount").unwrap();
            let amount = core::amount_from_hr_string(amount)
                .map_err(|_| ErrorKind::InvalidAmount(amount.to_string()))?;

            let mut to = to.to_string();
            let mut display_to = None;
            if to.starts_with("@") {
                let contact = address_book.lock().get_contact(&to[1..])?;
                to = contact.get_address().to_string();
                display_to = Some(contact.get_name().to_string());
            }

            // try parse as a general address
            let address = Address::parse(&to);
            let address: Result<Box<Address>> = match address {
                Ok(address) => Ok(address),
                Err(e) => {
                    Ok(Box::new(GrinboxAddress::from_str(&to).map_err(|_| e)?) as Box<Address>)
                }
            };

            let to = address?;
            if display_to.is_none() {
                display_to = Some(to.stripped());
            }
            let slate: Result<Slate> = match to.address_type() {
                AddressType::Keybase => {
                    if let Some((publisher, _)) = keybase_broker {
                        let slate = wallet.lock().initiate_receive_tx(amount, outputs)?;
                        publisher.post_slate(&slate, to.borrow())?;
                        Ok(slate)
                    } else {
                        Err(ErrorKind::ClosedListener("keybase".to_string()))?
                    }
                }
                AddressType::Grinbox => {
                    if let Some((publisher, _)) = grinbox_broker {
                        let slate = wallet.lock().initiate_receive_tx(amount, outputs)?;
                        publisher.post_slate(&slate, to.borrow())?;
                        Ok(slate)
                    } else {
                        Err(ErrorKind::ClosedListener("mwcmq".to_string()))?
                    }
                }
                _ => Err(ErrorKind::HttpRequest.into()),
            };

            let slate = slate?;
            cli_message!(
                "invoice slate [{}] for [{}] MWCs sent successfully to [{}]",
                slate.id.to_string().bright_green(),
                core::amount_to_hr_string(slate.amount, false).bright_green(),
                display_to.unwrap().bright_green()
            );
        }
        Some("restore") => {
            *out_is_safe = false;
            if keybase_broker.is_some() || grinbox_broker.is_some() {
                return Err(ErrorKind::HasListener.into());
            }
            let args = matches.subcommand_matches("restore").unwrap();
            let passphrase = match args.is_present("passphrase") {
                true => password_prompt(args.value_of("passphrase")),
                false => "".to_string(),
            };
            *out_is_safe = args.value_of("passphrase").is_none();

            println!("restoring... please wait as this could take a few minutes to complete.");

            {
                let mut w = wallet.lock();
                let seed = w.init(config, passphrase.as_str(), false)?;
                w.complete(seed, config, "default", passphrase.as_str(), true)?;
                w.restore_state()?;
            }

            derive_address_key(config, wallet, grinbox_broker)?;
            if passphrase.is_empty() {
                println!("{}: wallet with no passphrase.", "WARNING".bright_yellow());
            }

            println!("wallet restoration done!");
            return Ok(());
        }
        Some("recover") => {
            *out_is_safe = false;
            if keybase_broker.is_some() || grinbox_broker.is_some() {
                return Err(ErrorKind::HasListener.into());
            }
            let args = matches.subcommand_matches("recover").unwrap();
            let passphrase = match args.is_present("passphrase") {
                true => password_prompt(args.value_of("passphrase")),
                false => "".to_string(),
            };
            *out_is_safe = args.value_of("passphrase").is_none();

            if let Some(words) = args.values_of("words") {
                println!("recovering... please wait as this could take a few minutes to complete.");
                let words: Vec<&str> = words.collect();
                {
                    let mut w = wallet.lock();
                    w.restore_seed(config, &words, passphrase.as_str())?;
                    let seed = w.init(config, passphrase.as_str(), false)?;
                    w.complete(seed, config, "default", passphrase.as_str(), true)?;
                    w.restore_state()?;
                }

                derive_address_key(config, wallet, grinbox_broker)?;
                if passphrase.is_empty() {
                    println!("{}: wallet with no passphrase.", "WARNING".bright_yellow());
                }

                println!("wallet restoration done!");
                *out_is_safe = false;
                return Ok(());
            } else if args.is_present("display") {
                let mut w = wallet.lock();
                w.show_mnemonic(config, &passphrase)?;
                return Ok(());
            }
        }
        Some("check") => {
            if keybase_broker.is_some() || grinbox_broker.is_some() {
                return Err(ErrorKind::HasListener.into());
            }
            println!("checking and repairing... please wait as this could take a few minutes to complete.");
            let mut wallet = wallet.lock();
            wallet.check_repair()?;
            cli_message!("check and repair done!");
        }
        Some("set-recv") => {
            let args = matches.subcommand_matches("set-recv").unwrap();
            let account = args.value_of("account").unwrap();
            if wallet.lock().account_exists(account).unwrap() {
            unsafe {
                    RECV_ACCOUNT = Some(account.to_string());
                let pass = args.value_of("password");
                if pass.is_some() {
                    RECV_PASS = Some(pass.unwrap().to_string());
                }
                else
                {
                    RECV_PASS = None;
                }
            }
            unsafe {
                cli_message!("Incoming funds will be received in account: {:?}", RECV_ACCOUNT.clone().unwrap());
            }
        }
            else
            {
                cli_message!("Account {:?} does not exist!", account);
            }
        }
        Some("export-proof") => {
            let args = matches.subcommand_matches("export-proof").unwrap();
            let input = args.value_of("file").unwrap();
            let id = args.value_of("id").unwrap();
            let id = id
                .parse::<u32>()
                .map_err(|_| ErrorKind::InvalidTxId(id.to_string()))?;
            let w = wallet.lock();
            let tx_proof = w.get_tx_proof(id)?;
            match w.verify_tx_proof(&tx_proof) {
                Ok((sender, receiver, amount, outputs, kernel)) => {
                    let mut file = File::create(input.replace("~", &home_dir))?;
                    file.write_all(serde_json::to_string(&tx_proof)?.as_bytes())?;
                    println!("proof written to {}", input);
                    proof_ok(sender, receiver, amount, outputs, kernel);
                }
                Err(_) => {
                    cli_message!("unable to verify proof");
                }
            }
        }
        Some("verify-proof") => {
            let args = matches.subcommand_matches("verify-proof").unwrap();
            let input = args.value_of("file").unwrap();
            let path = Path::new(&input.replace("~", &home_dir)).to_path_buf();
            if !path.exists() {
                return Err(ErrorKind::FileNotFound(input.to_string()).into());
            }
            let mut file = File::open(path)?;
            let mut proof = String::new();
            file.read_to_string(&mut proof)?;
            let mut tx_proof: TxProof = serde_json::from_str(&proof)?;

            let mut wallet = wallet.lock();
            match wallet.verify_tx_proof(&tx_proof) {
                Ok((sender, receiver, amount, outputs, kernel)) => {
                    proof_ok(sender, receiver, amount, outputs, kernel);
                }
                Err(_) => {
                    cli_message!("unable to verify proof");
                }
            }
        }
        Some(subcommand) => {
            cli_message!(
                "{}: subcommand `{}` not implemented!",
                "ERROR".bright_red(),
                subcommand.bright_green()
            );
        }
        None => {}
    };

    Ok(())
}

#[cfg(windows)]
pub fn enable_ansi_support() {
    if !ansi_term::enable_ansi_support().is_ok() {
        colored::control::set_override(false);
    }
}

#[cfg(not(windows))]
pub fn enable_ansi_support() {
}
