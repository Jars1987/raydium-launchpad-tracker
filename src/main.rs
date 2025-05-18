use {
    bs58,
    // "sink" is something you can send values into (like a channel).
    futures::{sink::SinkExt, stream::StreamExt},
    log::{error, info, warn},
    std::{collections::HashMap, env, fmt},
    tokio,
    // `tonic` is a library for talking to gRPC servers. It's how we connect to Yellowstone.
    tonic::{service::Interceptor, transport::ClientTlsConfig, Status},
    yellowstone_grpc_client::GeyserGrpcClient,
    // The actual protobuf definitions used for messages we send/receive via gRPC
    yellowstone_grpc_proto::{
        geyser::{SubscribeUpdate, SubscribeUpdatePing},
        prelude::{
            subscribe_update::UpdateOneof, // Used to pattern match the kind of update we receive
            CommitmentLevel,
            SubscribeRequest,
            SubscribeRequestFilterTransactions, // Filters to only get relevant transactions
        },
    },
};

// The logging level (e.g., "info" will log info, warn, and error messages)
const RUST_LOG_LEVEL: &str = "info";
// Raydium Program Id
const RAYDIUM_LAUNCHPAD_PROGRAM: &str = "LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj";
// An array of instruction types to filter for. You can monitor all program transactions by leaving this empty.
// You can uncomment the specific instruction types you want to monitor (or add other ones).
// For other programs this array would have to be uprated accordingly
const TARGET_IX_TYPES: &[RaydiumInstructionType] = &[
    // 👇 Uncomment to filter for specific instruction types
    // RaydiumInstructionType::Initialize,
    // RaydiumInstructionType::MigrateToAmm,
];
// Replace with your QuickNode Yellowstone gRPC endpoint
const ENDPOINT: &str = "https://your-custom-endpoint.grpc.com";
const AUTH_TOKEN: &str = "your-auth-token";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_logging();
    info!("Starting to monitor account: {}", RAYDIUM_LAUNCHPAD_PROGRAM);

    //Sertup the client
    let mut client = setup_client().await?;
    info!("Connected to gRPC endpoint");

    // Start a subscription: get channels to send and receive messages
    let (subscribe_tx, subscribe_rx) = client.subscribe().await?;

    // Tell Yellowstone what accounts we want to monitor
    send_subscription_request(subscribe_tx).await?;
    info!("Subscription request sent. Listening for updates...");

    // Start processing updates (like listening to a stream of events)
    process_updates(subscribe_rx).await?;

    info!("Stream closed");
    Ok(())
}

// Sets the logging level and initializes the logger
fn setup_logging() {
    unsafe {
        env::set_var("RUST_LOG", RUST_LOG_LEVEL);
    }
    env_logger::init();
}

// Connect to the Yellowstone gRPC service
async fn setup_client() -> Result<GeyserGrpcClient<impl Interceptor>, Box<dyn std::error::Error>> {
    info!("Connecting to gRPC endpoint: {}", ENDPOINT);

    // Build a secure client connection (TLS) to the endpoint, with optional auth
    let client = GeyserGrpcClient::build_from_shared(ENDPOINT.to_string())?
        .x_token(Some(AUTH_TOKEN.to_string()))?
        .tls_config(ClientTlsConfig::new().with_native_roots())? // Use default trusted certs
        .connect()
        .await?;

    Ok(client)
}

// Send a subscription request to Yellowstone to start receiving transaction updates
async fn send_subscription_request<T>(mut tx: T) -> Result<(), Box<dyn std::error::Error>>
where
    // `tx` is a "Sink" where we send requests (like writing into a pipe)
    T: SinkExt<SubscribeRequest> + Unpin,
    <T as futures::Sink<SubscribeRequest>>::Error: std::error::Error + 'static,
{
    // Create account filter with the target accounts
    let mut accounts_filter = HashMap::new();
    accounts_filter.insert(
        "account_monitor".to_string(),
        SubscribeRequestFilterTransactions {
            account_include: vec![],
            account_exclude: vec![],
            account_required: vec![
                // Replace this or add additional accounts to monitor as needed
                RAYDIUM_LAUNCHPAD_PROGRAM.to_string(),
            ],
            vote: Some(false),   // Don’t include validator vote transactions
            failed: Some(false), // Don’t include failed transactions
            signature: None,
        },
    );

    // Send the subscription request to the server
    tx.send(SubscribeRequest {
        transactions: accounts_filter,
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    })
    .await?;

    Ok(())
}

// This function receives a stream of updates from the Yellowstone gRPC service.
// Each update may contain information like a new transaction or a heartbeat ping.
// The function loops forever (until the stream ends or an error occurs) and processes each incoming update.
async fn process_updates<S>(mut stream: S) -> Result<(), Box<dyn std::error::Error>>
where
    // `stream` is like an infinite iterator that yields new `SubscribeUpdate` events from the Solana blockchain
    // `Result<SubscribeUpdate, Status>` means each message may either be a valid update or a gRPC error
    // `Unpin` means this stream can be used safely across `.await` calls (a technical requirement of `tokio`)
    S: StreamExt<Item = Result<SubscribeUpdate, Status>> + Unpin,
{
    // Keep listening for updates until the stream ends or there's an error
    while let Some(message) = stream.next().await {
        match message {
            Ok(msg) => handle_message(msg)?,
            // If there's a gRPC error, log it and break the loop (stops listening)
            Err(e) => {
                error!("Error receiving message: {:?}", e);
                break;
            }
        }
    }

    Ok(())
}

// This function analyzes one update message from Yellowstone
// It specifically looks for transaction data and checks if it contains any Raydium Launchpad instructions
fn handle_message(msg: SubscribeUpdate) -> Result<(), Box<dyn std::error::Error>> {
    match msg.update_oneof {
        // Yellowstone sends multiple types of updates. This matches on actual transactions (not pings or unknowns)
        Some(UpdateOneof::Transaction(transaction_update)) => {
            // Try to parse the transaction update into readable instructions
            match TransactionParser::parse_transaction(&transaction_update) {
                // If the stream successfully gives us a new update, process it
                Ok(parsed_tx) => {
                    let mut has_raydium_ix = false; // Was any Raydium instruction found?
                    let mut found_target_ix = false; // Did we find one of the *filtered* instructions we care about?
                    let mut found_ix_types = Vec::new(); // Stores all Raydium instruction types found

                    // If TARGET_IX_TYPES is empty, we treat *any* Raydium instruction as relevant
                    if TARGET_IX_TYPES.is_empty() {
                        found_target_ix = true;
                    }

                    // Iterate through top-level instructions in the transaction
                    for (i, ix) in parsed_tx.instructions.iter().enumerate() {
                        if ix.program_id == RAYDIUM_LAUNCHPAD_PROGRAM {
                            has_raydium_ix = true;

                            // Parse the actual instruction type (e.g., Initialize, Migrate, Unknown, etc.)
                            let raydium_ix_type = parse_raydium_instruction_type(&ix.data);
                            found_ix_types.push(raydium_ix_type.clone());

                            // If we’re filtering for specific types, check if this one matches
                            if TARGET_IX_TYPES.contains(&raydium_ix_type) {
                                found_target_ix = true;
                            }
                            // If we care about this instruction, log its presence
                            if found_target_ix {
                                info!(
                                    "Found target instruction: {} at index {}",
                                    raydium_ix_type, i
                                );
                            }
                        }
                    }

                    // Solana supports inner instructions — these are program calls made inside other programs (CPI)
                    // Here we also check those inner instructions for Raydium interactions
                    for inner_ix_group in &parsed_tx.inner_instructions {
                        // Each `inner_ix_group` is a group of instructions for one main instruction
                        for (i, inner_ix) in inner_ix_group.instructions.iter().enumerate() {
                            if inner_ix.program_id == RAYDIUM_LAUNCHPAD_PROGRAM {
                                has_raydium_ix = true;
                                let raydium_ix_type =
                                    parse_raydium_instruction_type(&inner_ix.data);
                                found_ix_types.push(raydium_ix_type.clone());

                                if TARGET_IX_TYPES.contains(&raydium_ix_type) {
                                    found_target_ix = true;
                                }
                                // Only log if it is a target instruction and if not RaydiumInstructionType::Unknown
                                if found_target_ix
                                    && !matches!(
                                        raydium_ix_type,
                                        RaydiumInstructionType::Unknown(_)
                                    )
                                {
                                    info!(
                                        "Found target instruction: {} at inner index {}.{}",
                                        raydium_ix_type,
                                        inner_ix_group.instruction_index, // Which instruction this is nested inside
                                        i // Index of this specific inner instruction
                                    );
                                }
                            }
                        }
                    }

                    // If we found *any* Raydium instruction AND one of them matches our filter
                    // then log the full parsed transaction (you could alert, store it, etc.)
                    if found_target_ix && has_raydium_ix {
                        info!("Found Raydium Launchpad transaction!");
                        info!("Parsed Transaction:\n{}", parsed_tx);
                    }
                }
                Err(e) => {
                    error!("Failed to parse transaction: {:?}", e);
                }
            }
        }
        Some(UpdateOneof::Ping(SubscribeUpdatePing {})) => {
            // Ignore pings
        }
        Some(other) => {
            info!("Unexpected update received. Type of update: {:?}", other);
        }
        None => {
            warn!("Empty update received");
        }
    }

    Ok(())
}
#[derive(Debug, Clone, PartialEq)]
pub enum RaydiumInstructionType {
    Initialize,
    BuyExactIn,
    BuyExactOut,
    SellExactIn,
    SellExactOut,
    ClaimPlatformFee,
    ClaimVestedToken,
    CollectFee,
    CollectMigrateFee,
    CreateConfig,
    CreatePlatformConfig,
    CreateVestingAccount,
    MigrateToAmm,
    MigrateToCpswap,
    UpdateConfig,
    UpdatePlatformConfig,
    Unknown([u8; 8]),
}

pub fn parse_raydium_instruction_type(data: &[u8]) -> RaydiumInstructionType {
    if data.len() < 8 {
        return RaydiumInstructionType::Unknown([0; 8]);
    }

    let mut discriminator = [0u8; 8];
    discriminator.copy_from_slice(&data[..8]);

    match discriminator {
        [175, 175, 109, 31, 13, 152, 155, 237] => RaydiumInstructionType::Initialize,
        [250, 234, 13, 123, 213, 156, 19, 236] => RaydiumInstructionType::BuyExactIn,
        [24, 211, 116, 40, 105, 3, 153, 56] => RaydiumInstructionType::BuyExactOut,
        [149, 39, 222, 155, 211, 124, 152, 26] => RaydiumInstructionType::SellExactIn,
        [95, 200, 71, 34, 8, 9, 11, 166] => RaydiumInstructionType::SellExactOut,
        [156, 39, 208, 135, 76, 237, 61, 72] => RaydiumInstructionType::ClaimPlatformFee,
        [49, 33, 104, 30, 189, 157, 79, 35] => RaydiumInstructionType::ClaimVestedToken,
        [60, 173, 247, 103, 4, 93, 130, 48] => RaydiumInstructionType::CollectFee,
        [255, 186, 150, 223, 235, 118, 201, 186] => RaydiumInstructionType::CollectMigrateFee,
        [201, 207, 243, 114, 75, 111, 47, 189] => RaydiumInstructionType::CreateConfig,
        [176, 90, 196, 175, 253, 113, 220, 20] => RaydiumInstructionType::CreatePlatformConfig,
        [129, 178, 2, 13, 217, 172, 230, 218] => RaydiumInstructionType::CreateVestingAccount,
        [207, 82, 192, 145, 254, 207, 145, 223] => RaydiumInstructionType::MigrateToAmm,
        [136, 92, 200, 103, 28, 218, 144, 140] => RaydiumInstructionType::MigrateToCpswap,
        [29, 158, 252, 191, 10, 83, 219, 99] => RaydiumInstructionType::UpdateConfig,
        [195, 60, 76, 129, 146, 45, 67, 143] => RaydiumInstructionType::UpdatePlatformConfig,
        _ => RaydiumInstructionType::Unknown(discriminator),
    }
}

impl std::fmt::Display for RaydiumInstructionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RaydiumInstructionType::Initialize => write!(f, "Initialize"),
            RaydiumInstructionType::BuyExactIn => write!(f, "BuyExactIn"),
            RaydiumInstructionType::BuyExactOut => write!(f, "BuyExactOut"),
            RaydiumInstructionType::SellExactIn => write!(f, "SellExactIn"),
            RaydiumInstructionType::SellExactOut => write!(f, "SellExactOut"),
            RaydiumInstructionType::ClaimPlatformFee => write!(f, "ClaimPlatformFee"),
            RaydiumInstructionType::ClaimVestedToken => write!(f, "ClaimVestedToken"),
            RaydiumInstructionType::CollectFee => write!(f, "CollectFee"),
            RaydiumInstructionType::CollectMigrateFee => write!(f, "CollectMigrateFee"),
            RaydiumInstructionType::CreateConfig => write!(f, "CreateConfig"),
            RaydiumInstructionType::CreatePlatformConfig => write!(f, "CreatePlatformConfig"),
            RaydiumInstructionType::CreateVestingAccount => write!(f, "CreateVestingAccount"),
            RaydiumInstructionType::MigrateToAmm => write!(f, "MigrateToAmm"),
            RaydiumInstructionType::MigrateToCpswap => write!(f, "MigrateToCpswap"),
            RaydiumInstructionType::UpdateConfig => write!(f, "UpdateConfig"),
            RaydiumInstructionType::UpdatePlatformConfig => write!(f, "UpdatePlatformConfig"),
            RaydiumInstructionType::Unknown(discriminator) => {
                write!(f, "Unknown(discriminator={:?})", discriminator)
            }
        }
    }
}

#[derive(Debug, Default)]
struct ParsedTransaction {
    signature: String,
    is_vote: bool,
    account_keys: Vec<String>,
    recent_blockhash: String,
    instructions: Vec<ParsedInstruction>,
    success: bool,
    fee: u64,
    pre_token_balances: Vec<ParsedTokenBalance>,
    post_token_balances: Vec<ParsedTokenBalance>,
    logs: Vec<String>,
    inner_instructions: Vec<ParsedInnerInstruction>,
    slot: u64,
}

// Custom display implementation for nice CLI output formatting
impl fmt::Display for ParsedTransaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Transaction: {}", self.signature)?;
        writeln!(
            f,
            "Status: {}",
            if self.success { "Success" } else { "Failed" }
        )?;
        writeln!(f, "Slot: {}", self.slot)?;
        writeln!(f, "Fee: {} lamports", self.fee)?;

        // Display all account keys used in this transaction
        writeln!(f, "\nAccount Keys:")?;
        for (i, key) in self.account_keys.iter().enumerate() {
            writeln!(f, "  [{}] {}", i, key)?;
        }

        // Display all top-level instructions
        writeln!(f, "\nInstructions:")?;
        for (i, ix) in self.instructions.iter().enumerate() {
            writeln!(f, "  Instruction {}:", i)?;
            writeln!(
                f,
                "    Program: {} (index: {})",
                ix.program_id, ix.program_id_index
            )?;
            writeln!(f, "    Accounts:")?;
            for (idx, acc) in &ix.accounts {
                writeln!(f, "      [{}] {}", idx, acc)?;
            }
            writeln!(f, "    Data: {} bytes", ix.data.len())?;
        }

        // Display CPI inner instructions, if any
        if !self.inner_instructions.is_empty() {
            writeln!(f, "\nInner Instructions:")?;
            for inner_ix in &self.inner_instructions {
                writeln!(f, "  Instruction Index: {}", inner_ix.instruction_index)?;
                for (i, ix) in inner_ix.instructions.iter().enumerate() {
                    writeln!(f, "    Inner Instruction {}:", i)?;
                    writeln!(
                        f,
                        "      Program: {} (index: {})",
                        ix.program_id, ix.program_id_index
                    )?;
                    writeln!(f, "      Accounts:")?;
                    for (idx, acc) in &ix.accounts {
                        writeln!(f, "        [{}] {}", idx, acc)?;
                    }
                    writeln!(f, "      Data: {} bytes", ix.data.len())?;
                }
            }
        }

        // Display token balance changes before/after transaction
        if !self.pre_token_balances.is_empty() || !self.post_token_balances.is_empty() {
            writeln!(f, "\nToken Balances:")?;

            let mut balance_changes = HashMap::new();

            // Track pre-transaction balances
            for balance in &self.pre_token_balances {
                let key = (balance.account_index, balance.mint.clone());
                balance_changes.insert(key, (balance.amount.clone(), "".to_string()));
            }

            // Track post-transaction balances
            for balance in &self.post_token_balances {
                let key = (balance.account_index, balance.mint.clone());

                if let Some((_, post)) = balance_changes.get_mut(&key) {
                    *post = balance.amount.clone();
                } else {
                    balance_changes.insert(key, ("".to_string(), balance.amount.clone()));
                }
            }

            // Output balance changes
            for ((account_idx, mint), (pre_amount, post_amount)) in balance_changes {
                let account_key = if (account_idx as usize) < self.account_keys.len() {
                    &self.account_keys[account_idx as usize]
                } else {
                    "unknown"
                };

                if pre_amount.is_empty() {
                    writeln!(
                        f,
                        "  Account {} ({}): new balance {} (mint: {})",
                        account_idx, account_key, post_amount, mint
                    )?;
                } else if post_amount.is_empty() {
                    writeln!(
                        f,
                        "  Account {} ({}): removed balance {} (mint: {})",
                        account_idx, account_key, pre_amount, mint
                    )?;
                } else {
                    writeln!(
                        f,
                        "  Account {} ({}): {} → {} (mint: {})",
                        account_idx, account_key, pre_amount, post_amount, mint
                    )?;
                }
            }
        }

        // Display transaction logs
        if !self.logs.is_empty() {
            writeln!(f, "\nTransaction Logs:")?;
            for (i, log) in self.logs.iter().enumerate() {
                writeln!(f, "  [{}] {}", i, log)?;
            }
        }

        Ok(())
    }
}

// Represents a top-level or inner instruction in a transaction
#[derive(Debug)]
struct ParsedInstruction {
    program_id: String,             // Program being invoked
    program_id_index: u8,           // Index in account_keys
    accounts: Vec<(usize, String)>, // Accounts involved: (index, pubkey)
    data: Vec<u8>,                  // Raw instruction data
}

// Represents a group of inner instructions (CPI) for one parent instruction
#[derive(Debug)]
struct ParsedInnerInstruction {
    instruction_index: u8,                // Index of parent instruction
    instructions: Vec<ParsedInstruction>, // Inner CPI calls
}

// Represents a token balance (before or after transaction)
#[derive(Debug)]
struct ParsedTokenBalance {
    account_index: u32, // Index into account_keys
    mint: String,       // Token mint address
    owner: String,      // Owner of the token account
    amount: String,     // Token amount as string
}

// Parser implementation that converts a raw Geyser transaction update into a ParsedTransaction
struct TransactionParser;

impl TransactionParser {
    pub fn parse_transaction(
        tx_update: &yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction,
    ) -> Result<ParsedTransaction, Box<dyn std::error::Error>> {
        let mut parsed_tx = ParsedTransaction::default();
        parsed_tx.slot = tx_update.slot;

        // Check if there's a transaction to parse
        if let Some(tx_info) = &tx_update.transaction {
            parsed_tx.is_vote = tx_info.is_vote;
            parsed_tx.signature = bs58::encode(&tx_info.signature).into_string();

            // Parse core transaction data
            if let Some(tx) = &tx_info.transaction {
                if let Some(msg) = &tx.message {
                    // Decode account keys from message
                    for key in &msg.account_keys {
                        parsed_tx.account_keys.push(bs58::encode(key).into_string());
                    }

                    // Add loaded addresses from transaction meta
                    if let Some(meta) = &tx_info.meta {
                        for addr in &meta.loaded_writable_addresses {
                            parsed_tx
                                .account_keys
                                .push(bs58::encode(addr).into_string());
                        }
                        for addr in &meta.loaded_readonly_addresses {
                            parsed_tx
                                .account_keys
                                .push(bs58::encode(addr).into_string());
                        }
                    }

                    // Decode recent blockhash
                    parsed_tx.recent_blockhash = bs58::encode(&msg.recent_blockhash).into_string();

                    // Parse all top-level instructions
                    for ix in &msg.instructions {
                        let program_id_index = ix.program_id_index;
                        let program_id = parsed_tx
                            .account_keys
                            .get(program_id_index as usize)
                            .cloned()
                            .unwrap_or_else(|| "unknown".to_string());

                        let mut accounts = Vec::new();
                        for &acc_idx in &ix.accounts {
                            if let Some(pubkey) = parsed_tx.account_keys.get(acc_idx as usize) {
                                accounts.push((acc_idx as usize, pubkey.clone()));
                            }
                        }

                        parsed_tx.instructions.push(ParsedInstruction {
                            program_id,
                            program_id_index: program_id_index as u8,
                            accounts,
                            data: ix.data.clone(),
                        });
                    }
                }
            }

            // Parse transaction metadata
            if let Some(meta) = &tx_info.meta {
                parsed_tx.success = meta.err.is_none();
                parsed_tx.fee = meta.fee;

                // Parse token balances before transaction
                for balance in &meta.pre_token_balances {
                    if let Some(amount) = &balance.ui_token_amount {
                        parsed_tx.pre_token_balances.push(ParsedTokenBalance {
                            account_index: balance.account_index,
                            mint: balance.mint.clone(),
                            owner: balance.owner.clone(),
                            amount: amount.ui_amount_string.clone(),
                        });
                    }
                }

                // Parse token balances after transaction
                for balance in &meta.post_token_balances {
                    if let Some(amount) = &balance.ui_token_amount {
                        parsed_tx.post_token_balances.push(ParsedTokenBalance {
                            account_index: balance.account_index,
                            mint: balance.mint.clone(),
                            owner: balance.owner.clone(),
                            amount: amount.ui_amount_string.clone(),
                        });
                    }
                }

                // Parse all inner instructions (CPI)
                for inner_ix in &meta.inner_instructions {
                    let mut parsed_inner_ixs = Vec::new();

                    for ix in &inner_ix.instructions {
                        let program_id_index = ix.program_id_index;
                        let program_id = parsed_tx
                            .account_keys
                            .get(program_id_index as usize)
                            .cloned()
                            .unwrap_or_else(|| "unknown".to_string());

                        let mut accounts = Vec::new();
                        for &acc_idx in &ix.accounts {
                            if let Some(pubkey) = parsed_tx.account_keys.get(acc_idx as usize) {
                                accounts.push((acc_idx as usize, pubkey.clone()));
                            }
                        }

                        parsed_inner_ixs.push(ParsedInstruction {
                            program_id,
                            program_id_index: program_id_index as u8,
                            accounts,
                            data: ix.data.clone(),
                        });
                    }

                    parsed_tx.inner_instructions.push(ParsedInnerInstruction {
                        instruction_index: inner_ix.index as u8,
                        instructions: parsed_inner_ixs,
                    });
                }

                // Parse log messages
                parsed_tx.logs = meta.log_messages.clone();
            }
        }

        Ok(parsed_tx)
    }
}
