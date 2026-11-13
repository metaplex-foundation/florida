//! End-to-end tests against a running local validator.
//!
//! They drive real auctions through this program using NFTs created by the
//! current mainnet Token Metadata program, including NFTs shrunk by its
//! `Resize` instruction. They are `#[ignore]`d so a plain `cargo test` skips
//! them; see `localnet/README.md` for how to start the validator and run them:
//!
//!   cargo test --test localnet -- --ignored --test-threads=1 --nocapture
//!
//! Set `RPC_URL` to point somewhere other than http://127.0.0.1:8899.

use borsh::BorshSerialize;
use mpl_auction::{
    instruction::{create_auction_instruction_v2, place_bid_instruction},
    processor::{CreateAuctionArgsV2, PlaceBidArgs, PriceFloor, WinnerLimit},
    BIDDER_POT_TOKEN, EXTENDED, PREFIX as AUCTION_PREFIX,
};
use mpl_metaplex::{
    instruction::{
        create_end_auction_instruction, create_set_store_instruction,
        create_start_auction_instruction, EmptyPaymentAccountArgs, EndAuctionArgs,
        InitAuctionManagerV2Args, MetaplexInstruction,
    },
    state::{
        AmountRange, Key as MetaplexKey, SafetyDepositConfig, TupleNumericType, WinningConfigType,
        PREFIX as METAPLEX_PREFIX, TOTALS,
    },
};
use mpl_token_metadata::{
    instruction::{create_master_edition_v3, create_metadata_accounts_v3},
    state::{
        Creator, EDITION, MAX_MASTER_EDITION_LEN, MAX_METADATA_LEN, PREFIX as METADATA_PREFIX,
    },
};
use mpl_token_vault::{
    instruction::{
        create_activate_vault_instruction, create_add_token_to_inactive_vault_instruction,
        create_combine_vault_instruction, create_init_vault_instruction,
        create_update_external_price_account_instruction,
    },
    state::{MAX_EXTERNAL_ACCOUNT_SIZE, MAX_VAULT_SIZE, PREFIX as VAULT_PREFIX},
};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    message::Message,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program, sysvar,
    transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
use std::{thread::sleep, time::Duration};

/// Token Metadata's `Resize` instruction (discriminator 56). It is newer than the
/// mpl-token-metadata crate this program pins, so it is built by hand. Account order
/// and the discriminator come from the mpl-token-metadata program/client source.
const RESIZE_DISCRIMINATOR: u8 = 56;

const BID_AMOUNT: u64 = 1_000;

// ---------------------------------------------------------------------------
// RPC plumbing
// ---------------------------------------------------------------------------

struct Ctx {
    client: RpcClient,
    payer: Keypair,
}

impl Ctx {
    fn new() -> Ctx {
        let url = std::env::var("RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8899".into());
        let client = RpcClient::new_with_commitment(url.clone(), CommitmentConfig::confirmed());

        match client.get_version() {
            Ok(v) => println!("validator at {}: solana-core {}", url, v.solana_core),
            Err(e) => panic!(
                "Can't reach a validator at {}: {}. Is localnet/start-validator.sh still \
                 running? (Set RPC_URL if it listens elsewhere.)",
                url, e
            ),
        }

        let loaders = [
            solana_sdk::bpf_loader::id(),
            solana_sdk::bpf_loader_deprecated::id(),
            solana_sdk::bpf_loader_upgradeable::id(),
        ];
        for (name, id) in [
            ("Metaplex (this program)", mpl_metaplex::id()),
            ("Token Metadata", mpl_token_metadata::id()),
            ("Auction", mpl_auction::id()),
            ("Token Vault", mpl_token_vault::id()),
        ] {
            match client.get_account(&id) {
                // Newer validators may not set `executable`, so also accept any
                // account owned by a BPF loader.
                Ok(acct) if acct.executable || loaders.contains(&acct.owner) => {}
                Ok(acct) => panic!(
                    "{} program {} exists on {} but doesn't look like a program \
                     (owner {}, executable {}).",
                    name, id, url, acct.owner, acct.executable
                ),
                Err(e) => panic!(
                    "{} program {} was not found on {}: {}. Check the validator's output: \
                     the program didn't load at startup.",
                    name, id, url, e
                ),
            }
        }

        let ctx = Ctx {
            client,
            payer: Keypair::new(),
        };
        ctx.airdrop(&ctx.payer.pubkey(), 100_000_000_000);
        ctx
    }

    fn airdrop(&self, to: &Pubkey, lamports: u64) {
        let sig = self.client.request_airdrop(to, lamports).expect("airdrop");
        for _ in 0..60 {
            if self.client.confirm_transaction(&sig).unwrap_or(false) {
                return;
            }
            sleep(Duration::from_millis(500));
        }
        panic!("airdrop to {} was not confirmed", to);
    }

    /// Send `ixs` paid by `self.payer`. Any keypair in `signers` that the message
    /// actually needs is used to sign; the rest are ignored.
    fn send(&self, label: &str, ixs: &[Instruction], signers: &[&Keypair]) {
        let message = Message::new(ixs, Some(&self.payer.pubkey()));
        let required = &message.account_keys[..message.header.num_required_signatures as usize];
        let mut keys: Vec<&Keypair> = vec![&self.payer];
        for k in signers {
            if required.contains(&k.pubkey()) && !keys.iter().any(|s| s.pubkey() == k.pubkey()) {
                keys.push(k);
            }
        }
        let blockhash = self.client.get_latest_blockhash().expect("blockhash");
        let tx = Transaction::new(&keys, message, blockhash);
        if let Err(e) = self.client.send_and_confirm_transaction(&tx) {
            panic!("{} failed: {:#?}", label, e);
        }
        println!("  ok: {}", label);
    }

    fn rent(&self, len: usize) -> u64 {
        self.client
            .get_minimum_balance_for_rent_exemption(len)
            .expect("rent")
    }

    fn data_len(&self, key: &Pubkey) -> usize {
        self.client.get_account(key).expect("account").data.len()
    }

    fn token_amount(&self, token_account: &Pubkey) -> u64 {
        let acct = self
            .client
            .get_account(token_account)
            .expect("token account");
        spl_token::state::Account::unpack(&acct.data)
            .unwrap()
            .amount
    }

    fn create_mint_ixs(
        &self,
        mint: &Pubkey,
        authority: &Pubkey,
        freeze: &Pubkey,
    ) -> Vec<Instruction> {
        vec![
            system_instruction::create_account(
                &self.payer.pubkey(),
                mint,
                self.rent(spl_token::state::Mint::LEN),
                spl_token::state::Mint::LEN as u64,
                &spl_token::id(),
            ),
            spl_token::instruction::initialize_mint(
                &spl_token::id(),
                mint,
                authority,
                Some(freeze),
                0,
            )
            .unwrap(),
        ]
    }

    fn create_token_account_ixs(
        &self,
        account: &Pubkey,
        mint: &Pubkey,
        owner: &Pubkey,
    ) -> Vec<Instruction> {
        vec![
            system_instruction::create_account(
                &self.payer.pubkey(),
                account,
                self.rent(spl_token::state::Account::LEN),
                spl_token::state::Account::LEN as u64,
                &spl_token::id(),
            ),
            spl_token::instruction::initialize_account(&spl_token::id(), account, mint, owner)
                .unwrap(),
        ]
    }

    fn create_ata_ix(&self, wallet: &Pubkey, mint: &Pubkey) -> Instruction {
        create_associated_token_account(&self.payer.pubkey(), wallet, mint, &spl_token::id())
    }
}

fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

// ---------------------------------------------------------------------------
// NFTs
// ---------------------------------------------------------------------------

struct Nft {
    mint: Pubkey,
    metadata: Pubkey,
    edition: Pubkey,
    /// Owner's associated token account holding the single token.
    token: Pubkey,
}

/// Mint a 1/1 NFT through the live Token Metadata program, with `owner` as
/// mint authority, update authority and sole verified creator (100% share).
fn mint_nft(ctx: &Ctx, owner: &Keypair, max_supply: Option<u64>) -> Nft {
    let tm = mpl_token_metadata::id();
    let mint = Keypair::new();
    let m = mint.pubkey();
    let nft = Nft {
        mint: m,
        metadata: pda(&[METADATA_PREFIX.as_bytes(), tm.as_ref(), m.as_ref()], &tm),
        edition: pda(
            &[
                METADATA_PREFIX.as_bytes(),
                tm.as_ref(),
                m.as_ref(),
                EDITION.as_bytes(),
            ],
            &tm,
        ),
        token: get_associated_token_address(&owner.pubkey(), &m),
    };

    let mut ixs = ctx.create_mint_ixs(&m, &owner.pubkey(), &owner.pubkey());
    ixs.push(ctx.create_ata_ix(&owner.pubkey(), &m));
    ixs.push(
        spl_token::instruction::mint_to(&spl_token::id(), &m, &nft.token, &owner.pubkey(), &[], 1)
            .unwrap(),
    );
    ctx.send("mint NFT token", &ixs, &[owner, &mint]);

    let ixs = vec![
        create_metadata_accounts_v3(
            tm,
            nft.metadata,
            m,
            owner.pubkey(),
            ctx.payer.pubkey(),
            owner.pubkey(),
            "Resize Test".to_string(),
            "RSZ".to_string(),
            "https://example.com/resize-test.json".to_string(),
            Some(vec![Creator {
                address: owner.pubkey(),
                verified: true,
                share: 100,
            }]),
            500,
            true,
            true,
            None,
            None,
            None,
        ),
        create_master_edition_v3(
            tm,
            nft.edition,
            m,
            owner.pubkey(),
            owner.pubkey(),
            nft.metadata,
            ctx.payer.pubkey(),
            max_supply,
        ),
    ];
    ctx.send("create metadata + master edition", &ixs, &[owner]);
    println!(
        "  minted: metadata {} bytes, master edition {} bytes \
         (mpl-token-metadata 1.4 requires exactly {} and {})",
        ctx.data_len(&nft.metadata),
        ctx.data_len(&nft.edition),
        MAX_METADATA_LEN,
        MAX_MASTER_EDITION_LEN
    );
    nft
}

/// Shrink the NFT's metadata and master edition with Token Metadata's `Resize`,
/// signed by the holder, and check the metadata really got smaller.
///
/// Note: today's Token Metadata already creates accounts smaller than the old
/// fixed sizes that mpl-token-metadata 1.4 expects (metadata 607 vs 679 bytes,
/// master edition 20 vs 282), and `Resize` then shrinks them to their exact
/// serialized size.
fn resize_nft(ctx: &Ctx, nft: &Nft, holder: &Keypair) {
    let tm = mpl_token_metadata::id();
    let (md_before, ed_before) = (ctx.data_len(&nft.metadata), ctx.data_len(&nft.edition));

    let ix = Instruction {
        program_id: tm,
        accounts: vec![
            AccountMeta::new(nft.metadata, false),
            AccountMeta::new(nft.edition, false),
            AccountMeta::new_readonly(nft.mint, false),
            // payer: also the authority (the holder) because `authority` is omitted.
            AccountMeta::new(holder.pubkey(), true),
            // authority: omitted (optional accounts are passed as the program id).
            AccountMeta::new_readonly(tm, false),
            AccountMeta::new_readonly(nft.token, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data: vec![RESIZE_DISCRIMINATOR],
    };
    ctx.send("Token Metadata Resize", &[ix], &[holder]);

    let (md, ed) = (ctx.data_len(&nft.metadata), ctx.data_len(&nft.edition));
    println!(
        "  resized: metadata {} -> {} bytes, master edition {} -> {} bytes",
        md_before, md, ed_before, ed
    );
    assert!(md < md_before, "metadata was not resized");
    assert!(ed <= ed_before, "master edition grew");
}

// ---------------------------------------------------------------------------
// Vault, auction and auction manager setup
// ---------------------------------------------------------------------------

struct Auction {
    seller: Keypair,
    nft: Nft,
    currency_mint: Pubkey,
    store: Pubkey,
    vault: Pubkey,
    vault_pda: Pubkey,
    fraction_mint: Pubkey,
    safety_deposit_box: Pubkey,
    token_store: Pubkey,
    auction: Pubkey,
    auction_extended: Pubkey,
    auction_manager: Pubkey,
    token_tracker: Pubkey,
    accept_payment: Pubkey,
    safety_deposit_config: Pubkey,
}

/// Mint an NFT (optionally resized), lock it in a combined vault, create an
/// auction for it and an AuctionManagerV2, then run ValidateSafetyDepositBoxV2,
/// which is where a resized NFT used to be rejected.
fn setup_validated_auction(
    ctx: &Ctx,
    resize: bool,
    max_supply: Option<u64>,
    winning_config_type: WinningConfigType,
) -> Auction {
    let (metaplex, vault_program, auction_program) =
        (mpl_metaplex::id(), mpl_token_vault::id(), mpl_auction::id());

    let seller = Keypair::new();
    ctx.airdrop(&seller.pubkey(), 10_000_000_000);
    let s = seller.pubkey();

    let nft = mint_nft(ctx, &seller, max_supply);
    if resize {
        resize_nft(ctx, &nft, &seller);
    }

    // Bidding currency: a fresh SPL mint, so the test never touches wrapped SOL.
    let currency = Keypair::new();
    ctx.send(
        "create bidding currency mint",
        &ctx.create_mint_ixs(&currency.pubkey(), &ctx.payer.pubkey(), &ctx.payer.pubkey()),
        &[&currency],
    );

    // Store (public, so no whitelisted-creator account is needed).
    let store = pda(
        &[METAPLEX_PREFIX.as_bytes(), metaplex.as_ref(), s.as_ref()],
        &metaplex,
    );
    ctx.send(
        "SetStore",
        &[create_set_store_instruction(
            metaplex,
            store,
            s,
            ctx.payer.pubkey(),
            true,
        )],
        &[&seller],
    );

    // --- Vault ---
    let vault = Keypair::new();
    let v = vault.pubkey();
    let vault_pda = pda(
        &[VAULT_PREFIX.as_bytes(), vault_program.as_ref(), v.as_ref()],
        &vault_program,
    );
    let external_price = Keypair::new();
    let fraction_mint = Keypair::new();
    let fraction_treasury = Keypair::new();
    let redeem_treasury = Keypair::new();
    let price_mint = spl_token::native_mint::id();

    let mut ixs = vec![
        system_instruction::create_account(
            &ctx.payer.pubkey(),
            &external_price.pubkey(),
            ctx.rent(MAX_EXTERNAL_ACCOUNT_SIZE),
            MAX_EXTERNAL_ACCOUNT_SIZE as u64,
            &vault_program,
        ),
        create_update_external_price_account_instruction(
            vault_program,
            external_price.pubkey(),
            0,
            price_mint,
            true,
        ),
    ];
    ixs.extend(ctx.create_mint_ixs(&fraction_mint.pubkey(), &vault_pda, &vault_pda));
    ctx.send(
        "create external price account + fraction mint",
        &ixs,
        &[&external_price, &fraction_mint],
    );

    let mut ixs = ctx.create_token_account_ixs(
        &fraction_treasury.pubkey(),
        &fraction_mint.pubkey(),
        &vault_pda,
    );
    ixs.extend(ctx.create_token_account_ixs(&redeem_treasury.pubkey(), &price_mint, &vault_pda));
    ixs.push(system_instruction::create_account(
        &ctx.payer.pubkey(),
        &v,
        ctx.rent(MAX_VAULT_SIZE),
        MAX_VAULT_SIZE as u64,
        &vault_program,
    ));
    ctx.send(
        "create vault treasuries + vault account",
        &ixs,
        &[&fraction_treasury, &redeem_treasury, &vault],
    );

    ctx.send(
        "InitVault",
        &[create_init_vault_instruction(
            vault_program,
            fraction_mint.pubkey(),
            redeem_treasury.pubkey(),
            fraction_treasury.pubkey(),
            v,
            s,
            external_price.pubkey(),
            false,
        )],
        &[],
    );

    let token_store = Keypair::new();
    let safety_deposit_box = pda(
        &[VAULT_PREFIX.as_bytes(), v.as_ref(), nft.mint.as_ref()],
        &vault_program,
    );
    let mut ixs = ctx.create_token_account_ixs(&token_store.pubkey(), &nft.mint, &vault_pda);
    ixs.push(create_add_token_to_inactive_vault_instruction(
        vault_program,
        safety_deposit_box,
        nft.token,
        token_store.pubkey(),
        v,
        s,
        ctx.payer.pubkey(),
        s,
        1,
    ));
    ctx.send("AddTokenToInactiveVault", &ixs, &[&token_store, &seller]);

    ctx.send(
        "ActivateVault",
        &[create_activate_vault_instruction(
            vault_program,
            v,
            fraction_mint.pubkey(),
            fraction_treasury.pubkey(),
            vault_pda,
            s,
            0,
        )],
        &[&seller],
    );

    // --- Auction (authority = the auction manager PDA) ---
    let auction = pda(
        &[
            AUCTION_PREFIX.as_bytes(),
            auction_program.as_ref(),
            v.as_ref(),
        ],
        &auction_program,
    );
    let auction_extended = pda(
        &[
            AUCTION_PREFIX.as_bytes(),
            auction_program.as_ref(),
            v.as_ref(),
            EXTENDED.as_bytes(),
        ],
        &auction_program,
    );
    let auction_manager = pda(&[METAPLEX_PREFIX.as_bytes(), auction.as_ref()], &metaplex);

    // Combine the vault, handing vault authority to the auction manager.
    let outstanding_shares = Keypair::new();
    let paying_account = Keypair::new();
    let mut ixs =
        ctx.create_token_account_ixs(&outstanding_shares.pubkey(), &fraction_mint.pubkey(), &s);
    ixs.extend(ctx.create_token_account_ixs(&paying_account.pubkey(), &price_mint, &s));
    ixs.push(create_combine_vault_instruction(
        vault_program,
        v,
        outstanding_shares.pubkey(),
        paying_account.pubkey(),
        fraction_mint.pubkey(),
        fraction_treasury.pubkey(),
        redeem_treasury.pubkey(),
        auction_manager,
        s,
        s,
        vault_pda,
        external_price.pubkey(),
    ));
    ctx.send(
        "CombineVault",
        &ixs,
        &[&outstanding_shares, &paying_account, &seller],
    );

    ctx.send(
        "CreateAuctionV2",
        &[create_auction_instruction_v2(
            auction_program,
            ctx.payer.pubkey(),
            CreateAuctionArgsV2 {
                winners: WinnerLimit::Capped(1),
                end_auction_at: None,
                end_auction_gap: None,
                token_mint: currency.pubkey(),
                authority: auction_manager,
                resource: v,
                price_floor: PriceFloor::None([0; 32]),
                tick_size: None,
                gap_tick_size_percentage: None,
                instant_sale_price: None,
                name: None,
            },
        )],
        &[],
    );

    // --- Auction manager ---
    let token_tracker = pda(
        &[
            METAPLEX_PREFIX.as_bytes(),
            metaplex.as_ref(),
            auction_manager.as_ref(),
            TOTALS.as_bytes(),
        ],
        &metaplex,
    );
    let accept_payment = Keypair::new();
    let mut ixs = ctx.create_token_account_ixs(
        &accept_payment.pubkey(),
        &currency.pubkey(),
        &auction_manager,
    );
    // Built by hand: create_init_auction_manager_v2_instruction omits the token tracker.
    ixs.push(Instruction {
        program_id: metaplex,
        accounts: vec![
            AccountMeta::new(auction_manager, false),
            AccountMeta::new(token_tracker, false),
            AccountMeta::new_readonly(v, false),
            AccountMeta::new_readonly(auction, false),
            AccountMeta::new_readonly(s, false),
            AccountMeta::new(ctx.payer.pubkey(), true),
            AccountMeta::new_readonly(accept_payment.pubkey(), false),
            AccountMeta::new_readonly(store, false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(sysvar::rent::id(), false),
        ],
        data: MetaplexInstruction::InitAuctionManagerV2(InitAuctionManagerV2Args {
            amount_type: TupleNumericType::U8,
            length_type: TupleNumericType::U8,
            max_ranges: 10,
        })
        .try_to_vec()
        .unwrap(),
    });
    ctx.send("InitAuctionManagerV2", &ixs, &[&accept_payment]);

    // --- ValidateSafetyDepositBoxV2: reads Metadata (and MasterEditionV2 for printing) ---
    let safety_deposit_config = pda(
        &[
            METAPLEX_PREFIX.as_bytes(),
            metaplex.as_ref(),
            auction_manager.as_ref(),
            safety_deposit_box.as_ref(),
        ],
        &metaplex,
    );
    let original_authority_lookup = pda(
        &[
            METAPLEX_PREFIX.as_bytes(),
            auction.as_ref(),
            nft.metadata.as_ref(),
        ],
        &metaplex,
    );
    // Built by hand: create_validate_safety_deposit_box_v2_instruction omits the token tracker.
    let validate = Instruction {
        program_id: metaplex,
        accounts: vec![
            AccountMeta::new(safety_deposit_config, false),
            AccountMeta::new(token_tracker, false),
            AccountMeta::new(auction_manager, false),
            AccountMeta::new(nft.metadata, false),
            AccountMeta::new(original_authority_lookup, false),
            // Public store: no whitelisted creator entry, pass the system program.
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(store, false),
            AccountMeta::new_readonly(safety_deposit_box, false),
            AccountMeta::new_readonly(token_store.pubkey(), false),
            AccountMeta::new_readonly(nft.mint, false),
            AccountMeta::new_readonly(nft.edition, false),
            AccountMeta::new_readonly(v, false),
            AccountMeta::new_readonly(s, true),
            AccountMeta::new_readonly(s, true),
            AccountMeta::new(ctx.payer.pubkey(), true),
            AccountMeta::new_readonly(mpl_token_metadata::id(), false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(sysvar::rent::id(), false),
        ],
        data: MetaplexInstruction::ValidateSafetyDepositBoxV2(SafetyDepositConfig {
            key: MetaplexKey::SafetyDepositConfigV1,
            auction_manager,
            order: 0,
            winning_config_type,
            amount_type: TupleNumericType::U8,
            length_type: TupleNumericType::U8,
            amount_ranges: vec![AmountRange(1, 1)],
            participation_config: None,
            participation_state: None,
        })
        .try_to_vec()
        .unwrap(),
    };
    ctx.send("ValidateSafetyDepositBoxV2", &[validate], &[&seller]);
    assert!(
        ctx.client.get_account(&safety_deposit_config).is_ok(),
        "safety deposit config was not created"
    );

    Auction {
        seller,
        nft,
        currency_mint: currency.pubkey(),
        store,
        vault: v,
        vault_pda,
        fraction_mint: fraction_mint.pubkey(),
        safety_deposit_box,
        token_store: token_store.pubkey(),
        auction,
        auction_extended,
        auction_manager,
        token_tracker,
        accept_payment: accept_payment.pubkey(),
        safety_deposit_config,
    }
}

/// Start the auction, bid, end it, redeem the NFT to the winner, claim the bid
/// into the auction manager and pay the creator out with EmptyPaymentAccount
/// (which reads the Metadata account again).
fn run_token_only_transfer_auction(ctx: &Ctx, a: &Auction) {
    let (metaplex, auction_program) = (mpl_metaplex::id(), mpl_auction::id());
    let s = a.seller.pubkey();

    ctx.send(
        "StartAuction",
        &[create_start_auction_instruction(
            metaplex,
            a.auction_manager,
            a.auction,
            s,
            a.store,
        )],
        &[&a.seller],
    );

    // Bidder with currency to spend.
    let bidder = Keypair::new();
    let b = bidder.pubkey();
    ctx.airdrop(&b, 1_000_000_000);
    let bidder_currency = get_associated_token_address(&b, &a.currency_mint);
    ctx.send(
        "fund bidder",
        &[
            ctx.create_ata_ix(&b, &a.currency_mint),
            spl_token::instruction::mint_to(
                &spl_token::id(),
                &a.currency_mint,
                &bidder_currency,
                &ctx.payer.pubkey(),
                &[],
                BID_AMOUNT,
            )
            .unwrap(),
        ],
        &[],
    );

    let bidder_pot = pda(
        &[
            AUCTION_PREFIX.as_bytes(),
            auction_program.as_ref(),
            a.auction.as_ref(),
            b.as_ref(),
        ],
        &auction_program,
    );
    let bidder_pot_token = pda(
        &[
            AUCTION_PREFIX.as_bytes(),
            bidder_pot.as_ref(),
            BIDDER_POT_TOKEN.as_bytes(),
        ],
        &auction_program,
    );
    let bidder_metadata = pda(
        &[
            AUCTION_PREFIX.as_bytes(),
            auction_program.as_ref(),
            a.auction.as_ref(),
            b.as_ref(),
            "metadata".as_bytes(),
        ],
        &auction_program,
    );
    ctx.send(
        "PlaceBid",
        &[place_bid_instruction(
            auction_program,
            b,
            bidder_currency,
            bidder_pot_token,
            a.currency_mint,
            b,
            ctx.payer.pubkey(),
            PlaceBidArgs {
                amount: BID_AMOUNT,
                resource: a.vault,
            },
        )],
        &[&bidder],
    );

    ctx.send(
        "EndAuction",
        &[create_end_auction_instruction(
            metaplex,
            a.auction_manager,
            a.auction,
            a.auction_extended,
            s,
            a.store,
            EndAuctionArgs { reveal: None },
        )],
        &[&a.seller],
    );

    // Redeem: the winner receives the NFT. Built by hand because
    // create_redeem_bid_instruction omits the V2 safety deposit config account.
    let destination = get_associated_token_address(&b, &a.nft.mint);
    let bid_redemption = pda(
        &[
            METAPLEX_PREFIX.as_bytes(),
            a.auction.as_ref(),
            bidder_metadata.as_ref(),
        ],
        &metaplex,
    );
    let redeem = Instruction {
        program_id: metaplex,
        accounts: vec![
            AccountMeta::new(a.auction_manager, false),
            AccountMeta::new(a.token_store, false),
            AccountMeta::new(destination, false),
            AccountMeta::new(bid_redemption, false),
            AccountMeta::new(a.safety_deposit_box, false),
            AccountMeta::new(a.vault, false),
            AccountMeta::new(a.fraction_mint, false),
            AccountMeta::new_readonly(a.auction, false),
            AccountMeta::new_readonly(bidder_metadata, false),
            AccountMeta::new_readonly(b, true),
            AccountMeta::new(ctx.payer.pubkey(), true),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(mpl_token_vault::id(), false),
            AccountMeta::new_readonly(mpl_token_metadata::id(), false),
            AccountMeta::new_readonly(a.store, false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(sysvar::rent::id(), false),
            AccountMeta::new_readonly(a.vault_pda, false),
            AccountMeta::new(a.safety_deposit_config, false),
            AccountMeta::new_readonly(a.auction_extended, false),
        ],
        data: MetaplexInstruction::RedeemBid.try_to_vec().unwrap(),
    };
    ctx.send(
        "RedeemBid",
        &[ctx.create_ata_ix(&b, &a.nft.mint), redeem],
        &[&bidder],
    );
    assert_eq!(
        ctx.token_amount(&destination),
        1,
        "winner did not receive the NFT"
    );
    assert_eq!(ctx.token_amount(&a.token_store), 0);

    // Move the winning bid into the auction manager's accept-payment account.
    let claim = Instruction {
        program_id: metaplex,
        accounts: vec![
            AccountMeta::new(a.accept_payment, false),
            AccountMeta::new(bidder_pot_token, false),
            AccountMeta::new(bidder_pot, false),
            AccountMeta::new(a.auction_manager, false),
            AccountMeta::new_readonly(a.auction, false),
            AccountMeta::new_readonly(b, false),
            AccountMeta::new_readonly(a.currency_mint, false),
            AccountMeta::new_readonly(a.vault, false),
            AccountMeta::new_readonly(a.store, false),
            AccountMeta::new_readonly(auction_program, false),
            AccountMeta::new_readonly(sysvar::clock::id(), false),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(a.auction_extended, false),
        ],
        data: MetaplexInstruction::ClaimBid.try_to_vec().unwrap(),
    };
    ctx.send("ClaimBid", &[claim], &[]);
    assert_eq!(ctx.token_amount(&a.accept_payment), BID_AMOUNT);

    // Pay the creator (100% share, primary sale => 100% of the bid).
    let creator_destination = get_associated_token_address(&s, &a.currency_mint);
    let payout_ticket = pda(
        &[
            METAPLEX_PREFIX.as_bytes(),
            a.auction_manager.as_ref(),
            b"0",
            b"0",
            b"0",
            a.safety_deposit_box.as_ref(),
            s.as_ref(),
        ],
        &metaplex,
    );
    let empty = Instruction {
        program_id: metaplex,
        accounts: vec![
            AccountMeta::new(a.accept_payment, false),
            AccountMeta::new(creator_destination, false),
            AccountMeta::new(a.auction_manager, false),
            AccountMeta::new(payout_ticket, false),
            AccountMeta::new(ctx.payer.pubkey(), true),
            AccountMeta::new_readonly(a.nft.metadata, false),
            AccountMeta::new_readonly(a.nft.edition, false),
            AccountMeta::new_readonly(a.safety_deposit_box, false),
            AccountMeta::new_readonly(a.store, false),
            AccountMeta::new_readonly(a.vault, false),
            AccountMeta::new_readonly(a.auction, false),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(sysvar::rent::id(), false),
            AccountMeta::new_readonly(a.token_tracker, false),
            AccountMeta::new_readonly(a.safety_deposit_config, false),
        ],
        data: MetaplexInstruction::EmptyPaymentAccount(EmptyPaymentAccountArgs {
            winning_config_index: Some(0),
            winning_config_item_index: Some(0),
            creator_index: Some(0),
        })
        .try_to_vec()
        .unwrap(),
    };
    ctx.send(
        "EmptyPaymentAccount",
        &[ctx.create_ata_ix(&s, &a.currency_mint), empty],
        &[],
    );
    assert_eq!(
        ctx.token_amount(&creator_destination),
        BID_AMOUNT,
        "creator was not paid"
    );
    assert_eq!(ctx.token_amount(&a.accept_payment), 0);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The fix: a resized NFT goes through a whole TokenOnlyTransfer auction
/// (validate, start, bid, end, redeem, claim, creator payout).
#[test]
#[ignore = "needs a local validator: see localnet/README.md"]
fn token_only_transfer_auction_with_resized_nft() {
    let ctx = Ctx::new();
    let a = setup_validated_auction(&ctx, true, Some(0), WinningConfigType::TokenOnlyTransfer);
    run_token_only_transfer_auction(&ctx, &a);
}

/// The same flow with an NFT that was never resized. Today's Token Metadata
/// already creates these below the old fixed sizes (see `mint_nft`), so the
/// unpatched program rejects them too. Also a control for the harness: if this
/// fails, the problem is the setup, not resizing.
#[test]
#[ignore = "needs a local validator: see localnet/README.md"]
fn token_only_transfer_auction_with_unresized_nft() {
    let ctx = Ctx::new();
    let a = setup_validated_auction(&ctx, false, Some(0), WinningConfigType::TokenOnlyTransfer);
    run_token_only_transfer_auction(&ctx, &a);
}

/// Validation of a printing auction deserializes the MasterEditionV2 as well,
/// so check a resized master edition is accepted there. (Redeeming printing
/// prizes is not tested: the current Token Metadata program has removed the
/// vault-proxy print instruction that RedeemPrintingV2Bid relies on.)
#[test]
#[ignore = "needs a local validator: see localnet/README.md"]
fn printing_v2_validation_with_resized_master_edition() {
    let ctx = Ctx::new();
    setup_validated_auction(&ctx, true, Some(10), WinningConfigType::PrintingV2);
}
