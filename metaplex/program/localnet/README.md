# Local-validator tests

`tests/localnet.rs` runs real auctions through this program on a local validator,
using NFTs made by the **current mainnet** Token Metadata program.

Token Metadata's `Resize` converts legacy accounts (679-byte metadata, 282-byte
master edition) to the current sizes (607 and 20 bytes). NFTs minted today are
created at those sizes, so they look the same as resized NFTs, and the unpatched
program rejects both because mpl-token-metadata 1.4 requires exactly 679 and 282
bytes. `Resize` itself can't be run on a fresh mint: Token Metadata answers
"Account has already been resized". So the tests use freshly minted NFTs, and
check that their accounts are below the legacy sizes.

| Test | What it checks |
| --- | --- |
| `token_only_transfer_auction_with_resized_size_nft` | An NFT with resized-size accounts goes through the whole auction: validate, start, bid, end, redeem to the winner, claim the bid, pay the creator. |
| `printing_v2_validation_with_resized_size_master_edition` | Validating a printing auction accepts a 20-byte master edition. |

The tests are `#[ignore]`d, so a normal `cargo test` skips them.

## You need two Solana toolchains

- **Solana 1.14** to build the program (`cargo build-bpf`), matching the locked dependencies.
- **A current Solana/Agave CLI** to run the validator, because today's Token Metadata
  program won't run on a 1.14 validator.

Keep them in separate directories and point `SOLANA_BIN` at the current one.

## Run

```bash
cd metaplex/program

# 1. Build the program with the 1.14 toolchain
cargo build-bpf                                   # -> target/deploy/mpl_metaplex.so

# 2. Start the validator with the current toolchain (leave it running)
SOLANA_BIN=/path/to/current/solana/bin ./localnet/start-validator.sh

# 3. In another terminal, run the tests (the host toolchain the repo builds with)
cargo +1.66.0 test --test localnet -- --ignored --test-threads=1 --nocapture
```

The script clones the Token Metadata, Auction and Token Vault programs from mainnet,
so step 2 needs internet access. When the validator supports it, it also copies
mainnet's feature set, so the old program runs under the same rules it does on mainnet.
The ledger lives in `target/localnet-ledger` and is reset on every start.

If a test fails, the panic message includes the transaction's program logs.
`--nocapture` prints each step as it succeeds, which shows how far it got.

## Not covered, because current Token Metadata breaks them for every NFT

These are separate from resizing and fail with every NFT:

- **FullRightsTransfer** auctions fail at validation. The program moves the update
  authority with `UpdateMetadataAccount` (v1), which Token Metadata has removed.
- **PrintingV2 / participation** prizes can be validated but not redeemed. Redemption
  prints editions with `MintNewEditionFromMasterEditionViaVaultProxy`, which has also
  been removed.
- The V1 (deprecated) instructions depend on other removed Token Metadata instructions.

## Builders the test doesn't use

Several builders in `src/instruction.rs` have account lists that no longer match the
processors. For those instructions the test builds the account list itself, still using
`MetaplexInstruction` for the data:

- `create_init_auction_manager_v2_instruction` and
  `create_validate_safety_deposit_box_v2_instruction` leave out the token tracker account.
- `create_redeem_bid_instruction` leaves out the V2 safety deposit config account.
- There are no builders for `ClaimBid` or `EmptyPaymentAccount`.
