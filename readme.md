# Solana Transaction Parser

This Rust module defines a `TransactionParser` for parsing Solana transactions
received via the
[Yellowstone Geyser gRPC plugin](https://github.com/anza-xyz/yellowstone-grpc).
It transforms raw transaction data into a more readable and structured format,
making it easier to analyze transaction contents including instructions, account
interactions, token balance changes, logs, and success status.

---

## 🧩 Features

- Parses base58-encoded Solana transactions from `SubscribeUpdateTransaction`.
- Extracts:
  - Signature, slot, fee, success status
  - Message account keys
  - Instructions and inner instructions
  - Token balance changes (pre- and post-transaction)
  - Log messages

---

## 📦 Structs Overview

### `ParsedTransaction`

The core structured result containing the full transaction details.

- `signature`: Transaction signature (base58)
- `is_vote`: Whether the transaction is a vote transaction
- `account_keys`: All involved account public keys
- `recent_blockhash`: Blockhash of the transaction
- `instructions`: List of parsed instructions
- `inner_instructions`: Inner CPI instructions
- `pre_token_balances`, `post_token_balances`: Token state before/after
  execution
- `logs`: Program log messages
- `slot`: Slot when transaction was recorded
- `fee`: Transaction fee in lamports
- `success`: Whether the transaction was successful

### `ParsedInstruction`

- `program_id`: Executed program ID
- `program_id_index`: Index of the program in the account keys
- `accounts`: List of (index, pubkey) pairs used in this instruction
- `data`: Raw instruction data

### `ParsedInnerInstruction`

- `instruction_index`: Index of the outer instruction this belongs to
- `instructions`: Same structure as `ParsedInstruction`

### `ParsedTokenBalance`

- `account_index`: Index in the account keys
- `mint`: Mint address of the token
- `owner`: Token account owner
- `amount`: Stringified UI token amount

---

## 🧪 Usage

Call `TransactionParser::parse_transaction()` with a
`SubscribeUpdateTransaction` from Yellowstone to receive a fully structured
`ParsedTransaction`:

```rust
use transaction_parser::TransactionParser;

let parsed = TransactionParser::parse_transaction(&tx_update)?;
println!("{}", parsed);
```

## 📜 License

MIT or Apache-2.0 — you choose.

## 🙋‍♂️ Author

Built with ❤️ for Solana, inspired from QUickNode example for learning purposes.
