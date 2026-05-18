use anchor_lang::prelude::*;
use anchor_lang::solana_program::{program::invoke_signed, system_instruction};
use anchor_lang::system_program;

declare_id!("67g5HhrzQ2eqqEDJTEogHGBjAfwRD8evD3wg6LTDV8GK");

pub const HOUSE_EDGE_BPS: u64 = 1_000;
pub const BASIS_POINTS: u64 = 10_000;
pub const MAX_WINNERS: usize = 111;
pub const MAX_GAME_ID_LEN: usize = 64;
pub const EMERGENCY_RECOVERY_DELAY_SLOTS: u64 = 216_000;

pub const MODE_DAILY: u8 = 0;
pub const MODE_HIGHROLLER: u8 = 1;
pub const MODE_FLASH: u8 = 2;

#[program]
pub mod doorcash {
    use super::*;

    pub fn initialize_game(
        ctx: Context<InitializeGame>,
        game_id: String,
        mode: u8,
        seed_hash: [u8; 32],
        entry_fee_lamports: u64,
        max_players: u8,
        min_players: u8,
    ) -> Result<()> {
        require!(
            game_id.len() <= MAX_GAME_ID_LEN,
            DoorCashError::GameIdTooLong
        );
        require!(is_valid_mode(mode), DoorCashError::InvalidMode);
        require!(min_players >= 2, DoorCashError::InvalidConfig);
        require!(max_players >= min_players, DoorCashError::InvalidConfig);
        require!(entry_fee_lamports > 0, DoorCashError::InvalidConfig);

        let mode_state = &mut ctx.accounts.mode_state;
        if mode_state.active_streak_id == 0 {
            mode_state.mode = mode;
            mode_state.active_streak_id = 1;
        } else {
            require!(mode_state.mode == mode, DoorCashError::ModeMismatch);
        }
        require!(
            !mode_state.refund_round_open,
            DoorCashError::ModeRefundRoundOpen
        );

        let game = &mut ctx.accounts.game;
        game.game_id = game_id;
        game.authority = ctx.accounts.authority.key();
        game.house_wallet = ctx.accounts.house_wallet.key();
        game.seed_hash = seed_hash;
        game.entry_fee = entry_fee_lamports;
        game.max_players = max_players;
        game.min_players = min_players;
        game.player_count = 0;
        game.total_pot = 0;
        game.prize_each = 0;
        game.house_cut = 0;
        game.carry_in = 0;
        game.carry_out = 0;
        game.refund_pool = 0;
        game.refund_per_entry = 0;
        game.refund_round_id = 0;
        game.streak_id = mode_state.active_streak_id;
        game.mode = mode;
        game.settlement_kind = SettlementKind::Unsettled;
        game.status = GameStatus::Waiting;
        game.vault_bump = ctx.bumps.vault;
        game.seed_revealed = false;
        game.initialized_slot = Clock::get()?.slot;
        Ok(())
    }

    pub fn enter_game(ctx: Context<EnterGame>, game_id: String) -> Result<()> {
        let game = &mut ctx.accounts.game;

        require!(
            matches!(game.status, GameStatus::Waiting | GameStatus::Lobby),
            DoorCashError::GameNotAccepting
        );
        require!(
            !ctx.accounts.mode_state.refund_round_open,
            DoorCashError::ModeRefundRoundOpen
        );
        require!(
            ctx.accounts.mode_state.mode == game.mode,
            DoorCashError::ModeMismatch
        );
        require!(
            game.player_count < game.max_players as u16,
            DoorCashError::LobbyFull
        );

        let new_pot = game
            .total_pot
            .checked_add(game.entry_fee)
            .ok_or(DoorCashError::ArithmeticOverflow)?;

        let cpi_ctx = CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.player.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
            },
        );
        system_program::transfer(cpi_ctx, game.entry_fee)?;

        game.player_count = game
            .player_count
            .checked_add(1)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        game.total_pot = new_pot;

        let entry = &mut ctx.accounts.player_entry;
        entry.player = ctx.accounts.player.key();
        entry.game_id = game_id.clone();
        entry.mode = game.mode;
        entry.entered_at_slot = Clock::get()?.slot;
        entry.refunded = false;
        entry.merged_streak_id = 0;

        let participant = &mut ctx.accounts.mode_participant;
        if participant.wallet == Pubkey::default() {
            participant.wallet = ctx.accounts.player.key();
            participant.mode = game.mode;
            participant.streak_id = 0;
            participant.eligible_entries = 0;
            participant.last_paid_refund_round_id = 0;
        } else {
            require!(
                participant.wallet == ctx.accounts.player.key(),
                DoorCashError::Unauthorized
            );
            require!(participant.mode == game.mode, DoorCashError::ModeMismatch);
        }

        emit!(PlayerEntered {
            game_id: game.game_id.clone(),
            player: ctx.accounts.player.key(),
            count: game.player_count,
        });

        Ok(())
    }

    pub fn start_game(ctx: Context<StartGame>) -> Result<()> {
        let game = &mut ctx.accounts.game;
        require!(
            matches!(game.status, GameStatus::Waiting | GameStatus::Lobby),
            DoorCashError::InvalidGameStatus
        );
        require!(
            game.player_count >= game.min_players as u16,
            DoorCashError::NotEnoughPlayers
        );
        require!(
            !ctx.accounts.mode_state.refund_round_open,
            DoorCashError::ModeRefundRoundOpen
        );
        game.status = GameStatus::Active;
        Ok(())
    }

    pub fn reveal_seed(ctx: Context<AuthorityAction>, seed: [u8; 32]) -> Result<()> {
        let game = &mut ctx.accounts.game;
        require!(
            matches!(
                game.status,
                GameStatus::Active | GameStatus::Settling | GameStatus::Complete
            ),
            DoorCashError::InvalidGameStatus
        );
        require!(!game.seed_revealed, DoorCashError::SeedAlreadyRevealed);

        let computed = solana_sha256_hasher::hashv(&[&seed]);
        require!(
            computed.to_bytes() == game.seed_hash,
            DoorCashError::SeedHashMismatch
        );

        game.seed = seed;
        game.seed_revealed = true;

        emit!(SeedRevealed {
            game_id: game.game_id.clone(),
            seed
        });
        Ok(())
    }

    pub fn settle_winners<'info>(
        ctx: Context<'_, '_, 'info, 'info, SettleWinners<'info>>,
        winner_count: u8,
    ) -> Result<()> {
        let game = &mut ctx.accounts.game;
        let mode_state = &mut ctx.accounts.mode_state;

        require!(
            matches!(game.status, GameStatus::Active),
            DoorCashError::InvalidGameStatus
        );
        require!(game.seed_revealed, DoorCashError::SeedNotRevealed);
        require!(
            winner_count > 0 && winner_count as usize <= MAX_WINNERS,
            DoorCashError::NoWinners
        );
        let expected_remaining_accounts = winner_count as usize * 2;
        require!(
            ctx.remaining_accounts.len() == expected_remaining_accounts,
            DoorCashError::WinnerAccountMismatch
        );
        require!(
            !mode_state.refund_round_open,
            DoorCashError::ModeRefundRoundOpen
        );
        require!(mode_state.mode == game.mode, DoorCashError::ModeMismatch);

        for (idx, winner_chunk) in ctx.remaining_accounts.chunks(2).enumerate() {
            let winner_account = &winner_chunk[0];
            let entry_info = &winner_chunk[1];
            let entry: Account<PlayerEntry> = Account::try_from(entry_info)?;
            let (expected_entry, _) = Pubkey::find_program_address(
                &[
                    b"entry",
                    game.game_id.as_bytes(),
                    winner_account.key.as_ref(),
                ],
                ctx.program_id,
            );

            require!(
                entry_info.key() == expected_entry,
                DoorCashError::InvalidWinnerEntry
            );
            require!(entry.player == winner_account.key(), DoorCashError::InvalidWinnerEntry);
            require!(entry.game_id == game.game_id, DoorCashError::EntryGameMismatch);
            require!(entry.mode == game.mode, DoorCashError::ModeMismatch);
            require!(!entry.refunded, DoorCashError::AlreadyRefunded);

            for prior_chunk in ctx.remaining_accounts.chunks(2).take(idx) {
                require!(
                    prior_chunk[0].key() != winner_account.key(),
                    DoorCashError::DuplicateWinner
                );
            }
        }

        let ante_in = game.total_pot;
        let vault_balance = ctx.accounts.game_vault.to_account_info().lamports();
        require!(
            vault_balance >= ante_in,
            DoorCashError::InsufficientVaultFunds
        );
        let vault_dust = vault_balance
            .checked_sub(ante_in)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        let carry_in = mode_state.carry_lamports;
        let house_cut = calculate_house_cut(ante_in)?;
        let ante_after_house = ante_in
            .checked_sub(house_cut)
            .ok_or(DoorCashError::ArithmeticOverflow)?;

        transfer_game_vault(
            &game.game_id,
            game.vault_bump,
            &ctx.accounts.game_vault.to_account_info(),
            &ctx.accounts.house_wallet.to_account_info(),
            vault_dust,
        )?;

        transfer_game_vault(
            &game.game_id,
            game.vault_bump,
            &ctx.accounts.game_vault.to_account_info(),
            &ctx.accounts.house_wallet.to_account_info(),
            house_cut,
        )?;

        transfer_game_vault(
            &game.game_id,
            game.vault_bump,
            &ctx.accounts.game_vault.to_account_info(),
            &ctx.accounts.mode_vault.to_account_info(),
            ante_after_house,
        )?;

        let distributable = carry_in
            .checked_add(ante_after_house)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        let prize_each = distributable
            .checked_div(winner_count as u64)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        let payout_total = prize_each
            .checked_mul(winner_count as u64)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        let remainder = distributable
            .checked_sub(payout_total)
            .ok_or(DoorCashError::ArithmeticOverflow)?;

        for winner_chunk in ctx.remaining_accounts.chunks(2) {
            let winner_account = &winner_chunk[0];
            transfer_mode_vault(
                game.mode,
                &ctx.accounts.mode_vault.to_account_info(),
                winner_account,
                prize_each,
            )?;
        }

        mode_state.carry_lamports = remainder;
        mode_state.active_streak_id = mode_state
            .active_streak_id
            .checked_add(1)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        mode_state.eligible_entry_count = 0;
        mode_state.refund_per_entry_lamports = 0;
        mode_state.refund_remainder_lamports = 0;
        mode_state.refund_round_open = false;
        mode_state.refund_paid_entry_count = 0;

        game.prize_each = prize_each;
        game.house_cut = house_cut;
        game.carry_in = carry_in;
        game.carry_out = remainder;
        game.refund_pool = 0;
        game.refund_per_entry = 0;
        game.refund_round_id = 0;
        game.settlement_kind = SettlementKind::Winner;
        game.status = GameStatus::Complete;

        emit!(WinnerSettlement {
            game_id: game.game_id.clone(),
            winner_count,
            prize_each,
            house_cut,
            carry_in,
            carry_out: remainder,
        });

        Ok(())
    }

    pub fn register_no_winner_entries<'info>(
        ctx: Context<'_, '_, 'info, 'info, RegisterNoWinnerEntries<'info>>,
    ) -> Result<()> {
        let game = &ctx.accounts.game;
        let mode_state = &mut ctx.accounts.mode_state;

        require!(
            matches!(game.status, GameStatus::Active),
            DoorCashError::InvalidGameStatus
        );
        require!(
            !mode_state.refund_round_open,
            DoorCashError::ModeRefundRoundOpen
        );
        require!(mode_state.mode == game.mode, DoorCashError::ModeMismatch);
        require!(
            ctx.remaining_accounts.len() % 2 == 0,
            DoorCashError::BatchAccountMismatch
        );

        for chunk in ctx.remaining_accounts.chunks(2) {
            let entry_info = &chunk[0];
            let participant_info = &chunk[1];

            let mut entry: Account<PlayerEntry> = Account::try_from(entry_info)?;
            let mut participant: Account<ModeParticipant> = Account::try_from(participant_info)?;

            require!(
                entry.game_id == game.game_id,
                DoorCashError::EntryGameMismatch
            );
            require!(entry.mode == game.mode, DoorCashError::ModeMismatch);
            require!(
                participant.wallet == entry.player,
                DoorCashError::Unauthorized
            );
            require!(participant.mode == game.mode, DoorCashError::ModeMismatch);

            if participant.streak_id != game.streak_id {
                participant.streak_id = game.streak_id;
                participant.eligible_entries = 0;
            }

            if entry.merged_streak_id != game.streak_id {
                participant.eligible_entries = participant
                    .eligible_entries
                    .checked_add(1)
                    .ok_or(DoorCashError::ArithmeticOverflow)?;
                mode_state.eligible_entry_count = mode_state
                    .eligible_entry_count
                    .checked_add(1)
                    .ok_or(DoorCashError::ArithmeticOverflow)?;
                entry.merged_streak_id = game.streak_id;
            }

            entry.exit(ctx.program_id)?;
            participant.exit(ctx.program_id)?;
        }

        Ok(())
    }

    pub fn open_no_winner_settlement(ctx: Context<OpenNoWinnerSettlement>) -> Result<()> {
        let game = &mut ctx.accounts.game;
        let mode_state = &mut ctx.accounts.mode_state;

        require!(
            matches!(game.status, GameStatus::Active),
            DoorCashError::InvalidGameStatus
        );
        require!(game.seed_revealed, DoorCashError::SeedNotRevealed);
        require!(
            !mode_state.refund_round_open,
            DoorCashError::ModeRefundRoundOpen
        );
        require!(mode_state.mode == game.mode, DoorCashError::ModeMismatch);
        require!(
            mode_state.eligible_entry_count > 0,
            DoorCashError::NoEligibleRefundEntries
        );

        let ante_in = game.total_pot;
        let vault_balance = ctx.accounts.game_vault.to_account_info().lamports();
        require!(
            vault_balance >= ante_in,
            DoorCashError::InsufficientVaultFunds
        );
        let vault_dust = vault_balance
            .checked_sub(ante_in)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        let carry_in = mode_state.carry_lamports;
        let house_cut = calculate_house_cut(ante_in)?;
        let ante_after_house = ante_in
            .checked_sub(house_cut)
            .ok_or(DoorCashError::ArithmeticOverflow)?;

        transfer_game_vault(
            &game.game_id,
            game.vault_bump,
            &ctx.accounts.game_vault.to_account_info(),
            &ctx.accounts.house_wallet.to_account_info(),
            vault_dust,
        )?;

        transfer_game_vault(
            &game.game_id,
            game.vault_bump,
            &ctx.accounts.game_vault.to_account_info(),
            &ctx.accounts.house_wallet.to_account_info(),
            house_cut,
        )?;

        transfer_game_vault(
            &game.game_id,
            game.vault_bump,
            &ctx.accounts.game_vault.to_account_info(),
            &ctx.accounts.mode_vault.to_account_info(),
            ante_after_house,
        )?;

        let distributable = carry_in
            .checked_add(ante_after_house)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        let refund_pool = distributable
            .checked_div(2)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        let carry_out_base = distributable
            .checked_sub(refund_pool)
            .ok_or(DoorCashError::ArithmeticOverflow)?;

        let refund_per_entry = refund_pool
            .checked_div(mode_state.eligible_entry_count)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        let paid_total = refund_per_entry
            .checked_mul(mode_state.eligible_entry_count)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        let refund_remainder = refund_pool
            .checked_sub(paid_total)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        let carry_out = carry_out_base
            .checked_add(refund_remainder)
            .ok_or(DoorCashError::ArithmeticOverflow)?;

        mode_state.carry_lamports = carry_out;
        mode_state.open_refund_round_id = mode_state
            .open_refund_round_id
            .checked_add(1)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        mode_state.refund_per_entry_lamports = refund_per_entry;
        mode_state.refund_remainder_lamports = refund_remainder;
        mode_state.refund_round_open = true;
        mode_state.refund_paid_entry_count = 0;

        game.house_cut = house_cut;
        game.carry_in = carry_in;
        game.carry_out = carry_out;
        game.refund_pool = refund_pool;
        game.refund_per_entry = refund_per_entry;
        game.refund_round_id = mode_state.open_refund_round_id;
        game.prize_each = 0;
        game.settlement_kind = SettlementKind::NoWinner;
        game.status = GameStatus::Settling;

        emit!(NoWinnerSettlementOpened {
            game_id: game.game_id.clone(),
            refund_round_id: mode_state.open_refund_round_id,
            house_cut,
            carry_in,
            refund_pool,
            carry_out,
            refund_per_entry,
            eligible_entry_count: mode_state.eligible_entry_count,
        });

        Ok(())
    }

    pub fn pay_no_winner_batch<'info>(
        ctx: Context<'_, '_, 'info, 'info, PayNoWinnerBatch<'info>>,
        refund_round_id: u64,
    ) -> Result<()> {
        let game = &ctx.accounts.game;
        let mode_state = &mut ctx.accounts.mode_state;

        require!(
            matches!(game.status, GameStatus::Settling),
            DoorCashError::InvalidGameStatus
        );
        require!(mode_state.mode == game.mode, DoorCashError::ModeMismatch);
        require!(
            mode_state.refund_round_open,
            DoorCashError::RefundRoundNotOpen
        );
        require!(
            mode_state.open_refund_round_id == refund_round_id
                && game.refund_round_id == refund_round_id,
            DoorCashError::RefundRoundMismatch
        );
        require!(
            ctx.remaining_accounts.len() % 2 == 0,
            DoorCashError::BatchAccountMismatch
        );

        for chunk in ctx.remaining_accounts.chunks(2) {
            let participant_info = &chunk[0];
            let wallet_info = &chunk[1];

            let mut participant: Account<ModeParticipant> = Account::try_from(participant_info)?;
            require!(participant.mode == game.mode, DoorCashError::ModeMismatch);
            require!(
                participant.wallet == wallet_info.key(),
                DoorCashError::Unauthorized
            );
            require!(
                participant.streak_id == game.streak_id,
                DoorCashError::StreakMismatch
            );

            if participant.last_paid_refund_round_id == refund_round_id {
                continue;
            }

            let amount = mode_state
                .refund_per_entry_lamports
                .checked_mul(participant.eligible_entries)
                .ok_or(DoorCashError::ArithmeticOverflow)?;

            if amount > 0 {
                transfer_mode_vault(
                    game.mode,
                    &ctx.accounts.mode_vault.to_account_info(),
                    wallet_info,
                    amount,
                )?;
            }

            participant.last_paid_refund_round_id = refund_round_id;
            mode_state.refund_paid_entry_count = mode_state
                .refund_paid_entry_count
                .checked_add(participant.eligible_entries)
                .ok_or(DoorCashError::ArithmeticOverflow)?;
            participant.exit(ctx.program_id)?;
        }

        Ok(())
    }

    pub fn repair_no_winner_streak_entries<'info>(
        ctx: Context<'_, '_, 'info, 'info, RepairNoWinnerStreakEntries<'info>>,
    ) -> Result<()> {
        let game = &ctx.accounts.game;
        let mode_state = &ctx.accounts.mode_state;

        require!(
            matches!(game.status, GameStatus::Settling),
            DoorCashError::InvalidGameStatus
        );
        require!(
            matches!(game.settlement_kind, SettlementKind::NoWinner),
            DoorCashError::InvalidGameStatus
        );
        require!(mode_state.mode == game.mode, DoorCashError::ModeMismatch);
        require!(
            mode_state.refund_round_open,
            DoorCashError::RefundRoundNotOpen
        );
        require!(
            mode_state.open_refund_round_id == game.refund_round_id,
            DoorCashError::RefundRoundMismatch
        );
        require!(
            ctx.remaining_accounts.len() % 2 == 0,
            DoorCashError::BatchAccountMismatch
        );

        let mut repaired_entries: u64 = 0;

        for chunk in ctx.remaining_accounts.chunks(2) {
            let entry_info = &chunk[0];
            let participant_info = &chunk[1];

            let mut entry: Account<PlayerEntry> = Account::try_from(entry_info)?;
            let mut participant: Account<ModeParticipant> = Account::try_from(participant_info)?;

            require!(
                entry.game_id == game.game_id,
                DoorCashError::EntryGameMismatch
            );
            require!(entry.mode == game.mode, DoorCashError::ModeMismatch);
            require!(
                participant.wallet == entry.player,
                DoorCashError::Unauthorized
            );
            require!(participant.mode == game.mode, DoorCashError::ModeMismatch);
            require!(
                participant.last_paid_refund_round_id != game.refund_round_id,
                DoorCashError::AlreadyRefunded
            );

            if participant.streak_id != game.streak_id {
                participant.streak_id = game.streak_id;
                participant.eligible_entries = 0;
            }

            if entry.merged_streak_id != game.streak_id {
                participant.eligible_entries = participant
                    .eligible_entries
                    .checked_add(1)
                    .ok_or(DoorCashError::ArithmeticOverflow)?;
                entry.merged_streak_id = game.streak_id;
                repaired_entries = repaired_entries
                    .checked_add(1)
                    .ok_or(DoorCashError::ArithmeticOverflow)?;
            } else if participant.eligible_entries == 0 {
                participant.eligible_entries = 1;
                repaired_entries = repaired_entries
                    .checked_add(1)
                    .ok_or(DoorCashError::ArithmeticOverflow)?;
            }

            entry.exit(ctx.program_id)?;
            participant.exit(ctx.program_id)?;
        }

        emit!(NoWinnerEntriesRepaired {
            game_id: game.game_id.clone(),
            refund_round_id: game.refund_round_id,
            repaired_entries,
        });

        Ok(())
    }

    pub fn close_no_winner_settlement(
        ctx: Context<CloseNoWinnerSettlement>,
        refund_round_id: u64,
    ) -> Result<()> {
        let game = &mut ctx.accounts.game;
        let mode_state = &mut ctx.accounts.mode_state;

        require!(
            matches!(game.status, GameStatus::Settling),
            DoorCashError::InvalidGameStatus
        );
        require!(
            mode_state.refund_round_open,
            DoorCashError::RefundRoundNotOpen
        );
        require!(
            mode_state.open_refund_round_id == refund_round_id
                && game.refund_round_id == refund_round_id,
            DoorCashError::RefundRoundMismatch
        );
        require!(
            mode_state.refund_paid_entry_count >= mode_state.eligible_entry_count,
            DoorCashError::RefundRoundIncomplete
        );

        mode_state.refund_round_open = false;
        mode_state.refund_per_entry_lamports = 0;
        mode_state.refund_remainder_lamports = 0;
        mode_state.refund_paid_entry_count = 0;

        game.status = GameStatus::Complete;

        emit!(NoWinnerSettlementClosed {
            game_id: game.game_id.clone(),
            refund_round_id,
            carry_out: mode_state.carry_lamports,
        });

        Ok(())
    }

    pub fn cancel_game(ctx: Context<AuthorityAction>) -> Result<()> {
        let game = &mut ctx.accounts.game;
        require!(
            !matches!(
                game.status,
                GameStatus::Active | GameStatus::Settling | GameStatus::Complete
            ),
            DoorCashError::CannotCancelActiveGame
        );
        game.status = GameStatus::Cancelled;
        emit!(GameCancelled {
            game_id: game.game_id.clone()
        });
        Ok(())
    }

    pub fn refund_player_single(ctx: Context<RefundPlayer>, game_id: String) -> Result<()> {
        let game = &mut ctx.accounts.game;
        require!(
            matches!(game.status, GameStatus::Cancelled),
            DoorCashError::GameNotCancelled
        );

        let entry = &mut ctx.accounts.player_entry;
        require!(!entry.refunded, DoorCashError::AlreadyRefunded);
        let vault_balance = ctx.accounts.vault.to_account_info().lamports();
        require!(
            vault_balance >= game.entry_fee,
            DoorCashError::InsufficientVaultFunds
        );
        let remaining_after_refund = vault_balance
            .checked_sub(game.entry_fee)
            .ok_or(DoorCashError::ArithmeticOverflow)?;

        if remaining_after_refund > 0 && remaining_after_refund < game.entry_fee {
            transfer_game_vault(
                &game_id,
                game.vault_bump,
                &ctx.accounts.vault.to_account_info(),
                &ctx.accounts.house_wallet.to_account_info(),
                remaining_after_refund,
            )?;
        }

        transfer_game_vault(
            &game_id,
            game.vault_bump,
            &ctx.accounts.vault.to_account_info(),
            &ctx.accounts.player.to_account_info(),
            game.entry_fee,
        )?;

        entry.refunded = true;
        Ok(())
    }

    pub fn admin_sweep_game_vault_dust(
        ctx: Context<AdminGameVaultRecovery>,
        _game_id: String,
    ) -> Result<()> {
        let game = &ctx.accounts.game;
        let vault_balance = ctx.accounts.vault.to_account_info().lamports();
        let protected_lamports = protected_game_vault_lamports(game);

        if vault_balance <= protected_lamports {
            emit!(GameVaultRecovery {
                game_id: game.game_id.clone(),
                destination: ctx.accounts.house_wallet.key(),
                amount: 0,
                recovery_kind: RecoveryKind::Dust,
            });
            return Ok(());
        }

        let dust = vault_balance
            .checked_sub(protected_lamports)
            .ok_or(DoorCashError::ArithmeticOverflow)?;
        transfer_game_vault(
            &game.game_id,
            game.vault_bump,
            &ctx.accounts.vault.to_account_info(),
            &ctx.accounts.house_wallet.to_account_info(),
            dust,
        )?;

        emit!(GameVaultRecovery {
            game_id: game.game_id.clone(),
            destination: ctx.accounts.house_wallet.key(),
            amount: dust,
            recovery_kind: RecoveryKind::Dust,
        });
        Ok(())
    }

    pub fn admin_emergency_sweep_game_vault(
        ctx: Context<AdminGameVaultRecovery>,
        _game_id: String,
    ) -> Result<()> {
        let game = &mut ctx.accounts.game;
        require!(
            !matches!(game.status, GameStatus::Waiting | GameStatus::Lobby),
            DoorCashError::InvalidGameStatus
        );
        require!(
            Clock::get()?.slot
                >= game
                    .initialized_slot
                    .checked_add(EMERGENCY_RECOVERY_DELAY_SLOTS)
                    .ok_or(DoorCashError::ArithmeticOverflow)?,
            DoorCashError::EmergencyRecoveryTooEarly
        );

        let amount = ctx.accounts.vault.to_account_info().lamports();
        transfer_game_vault(
            &game.game_id,
            game.vault_bump,
            &ctx.accounts.vault.to_account_info(),
            &ctx.accounts.house_wallet.to_account_info(),
            amount,
        )?;
        if !matches!(game.status, GameStatus::Complete) {
            game.status = GameStatus::Cancelled;
        }

        emit!(GameVaultRecovery {
            game_id: game.game_id.clone(),
            destination: ctx.accounts.house_wallet.key(),
            amount,
            recovery_kind: RecoveryKind::Emergency,
        });
        Ok(())
    }

    pub fn admin_emergency_sweep_mode_vault(
        ctx: Context<AdminModeVaultRecovery>,
        mode: u8,
    ) -> Result<()> {
        require!(is_valid_mode(mode), DoorCashError::InvalidMode);
        require!(ctx.accounts.game.mode == mode, DoorCashError::ModeMismatch);
        require!(ctx.accounts.mode_state.mode == mode, DoorCashError::ModeMismatch);
        require!(
            Clock::get()?.slot
                >= ctx
                    .accounts
                    .game
                    .initialized_slot
                    .checked_add(EMERGENCY_RECOVERY_DELAY_SLOTS)
                    .ok_or(DoorCashError::ArithmeticOverflow)?,
            DoorCashError::EmergencyRecoveryTooEarly
        );

        let amount = ctx.accounts.mode_vault.to_account_info().lamports();
        transfer_mode_vault(
            mode,
            &ctx.accounts.mode_vault.to_account_info(),
            &ctx.accounts.house_wallet.to_account_info(),
            amount,
        )?;

        let mode_state = &mut ctx.accounts.mode_state;
        mode_state.carry_lamports = 0;
        mode_state.eligible_entry_count = 0;
        mode_state.refund_per_entry_lamports = 0;
        mode_state.refund_remainder_lamports = 0;
        mode_state.refund_round_open = false;
        mode_state.refund_paid_entry_count = 0;
        mode_state.active_streak_id = mode_state
            .active_streak_id
            .checked_add(1)
            .ok_or(DoorCashError::ArithmeticOverflow)?;

        emit!(ModeVaultRecovery {
            mode,
            destination: ctx.accounts.house_wallet.key(),
            amount,
        });
        Ok(())
    }
}

#[account]
#[derive(InitSpace)]
pub struct Game {
    #[max_len(MAX_GAME_ID_LEN)]
    pub game_id: String,
    pub authority: Pubkey,
    pub house_wallet: Pubkey,
    pub seed_hash: [u8; 32],
    pub seed: [u8; 32],
    pub seed_revealed: bool,
    pub mode: u8,
    pub entry_fee: u64,
    pub max_players: u8,
    pub min_players: u8,
    pub player_count: u16,
    pub total_pot: u64,
    pub prize_each: u64,
    pub house_cut: u64,
    pub carry_in: u64,
    pub carry_out: u64,
    pub refund_pool: u64,
    pub refund_per_entry: u64,
    pub refund_round_id: u64,
    pub streak_id: u64,
    pub settlement_kind: SettlementKind,
    pub vault_bump: u8,
    pub status: GameStatus,
    pub initialized_slot: u64,
}

#[account]
#[derive(InitSpace)]
pub struct PlayerEntry {
    pub player: Pubkey,
    #[max_len(MAX_GAME_ID_LEN)]
    pub game_id: String,
    pub mode: u8,
    pub entered_at_slot: u64,
    pub refunded: bool,
    pub merged_streak_id: u64,
}

#[account]
#[derive(InitSpace)]
pub struct ModeState {
    pub mode: u8,
    pub active_streak_id: u64,
    pub carry_lamports: u64,
    pub eligible_entry_count: u64,
    pub open_refund_round_id: u64,
    pub refund_per_entry_lamports: u64,
    pub refund_remainder_lamports: u64,
    pub refund_round_open: bool,
    pub refund_paid_entry_count: u64,
}

#[account]
#[derive(InitSpace)]
pub struct ModeParticipant {
    pub wallet: Pubkey,
    pub mode: u8,
    pub streak_id: u64,
    pub eligible_entries: u64,
    pub last_paid_refund_round_id: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace)]
pub enum GameStatus {
    Waiting,
    Lobby,
    Active,
    Settling,
    Complete,
    Cancelled,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace)]
pub enum SettlementKind {
    Unsettled,
    Winner,
    NoWinner,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryKind {
    Dust,
    Emergency,
}

#[derive(Accounts)]
#[instruction(game_id: String, mode: u8)]
pub struct InitializeGame<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: validated against stored pubkey during settlement
    pub house_wallet: AccountInfo<'info>,
    #[account(
        init,
        payer = authority,
        space = 8 + Game::INIT_SPACE,
        seeds = [b"game", game_id.as_bytes()],
        bump
    )]
    pub game: Account<'info, Game>,
    /// CHECK: vault PDA holds per-game ante lamports
    #[account(
        mut,
        seeds = [b"vault", game_id.as_bytes()],
        bump
    )]
    pub vault: AccountInfo<'info>,
    #[account(
        init_if_needed,
        payer = authority,
        space = 8 + ModeState::INIT_SPACE,
        seeds = [b"mode", mode.to_le_bytes().as_ref()],
        bump
    )]
    pub mode_state: Account<'info, ModeState>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(game_id: String)]
pub struct EnterGame<'info> {
    #[account(mut)]
    pub player: Signer<'info>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"game", game_id.as_bytes()],
        bump,
        has_one = authority @ DoorCashError::Unauthorized
    )]
    pub game: Account<'info, Game>,
    /// CHECK: vault receives SOL
    #[account(mut, seeds = [b"vault", game_id.as_bytes()], bump = game.vault_bump)]
    pub vault: AccountInfo<'info>,
    #[account(
        init,
        payer = player,
        space = 8 + PlayerEntry::INIT_SPACE,
        seeds = [b"entry", game_id.as_bytes(), player.key().as_ref()],
        bump
    )]
    pub player_entry: Account<'info, PlayerEntry>,
    #[account(
        init_if_needed,
        payer = player,
        space = 8 + ModeParticipant::INIT_SPACE,
        seeds = [b"mode-participant", game.mode.to_le_bytes().as_ref(), player.key().as_ref()],
        bump
    )]
    pub mode_participant: Account<'info, ModeParticipant>,
    #[account(mut, seeds = [b"mode", game.mode.to_le_bytes().as_ref()], bump)]
    pub mode_state: Account<'info, ModeState>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct StartGame<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ DoorCashError::Unauthorized)]
    pub game: Account<'info, Game>,
    #[account(mut, seeds = [b"mode", game.mode.to_le_bytes().as_ref()], bump)]
    pub mode_state: Account<'info, ModeState>,
}

#[derive(Accounts)]
pub struct AuthorityAction<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ DoorCashError::Unauthorized)]
    pub game: Account<'info, Game>,
}

#[derive(Accounts)]
pub struct SettleWinners<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ DoorCashError::Unauthorized, has_one = house_wallet @ DoorCashError::HouseWalletMismatch)]
    pub game: Account<'info, Game>,
    #[account(mut, seeds = [b"vault", game.game_id.as_bytes()], bump = game.vault_bump)]
    /// CHECK: per-game vault PDA
    pub game_vault: AccountInfo<'info>,
    #[account(mut, seeds = [b"mode", game.mode.to_le_bytes().as_ref()], bump)]
    pub mode_state: Account<'info, ModeState>,
    #[account(mut, seeds = [b"mode-vault", game.mode.to_le_bytes().as_ref()], bump)]
    /// CHECK: mode rollover/refund vault PDA
    pub mode_vault: AccountInfo<'info>,
    #[account(mut)]
    /// CHECK: must match game.house_wallet
    pub house_wallet: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterNoWinnerEntries<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ DoorCashError::Unauthorized)]
    pub game: Account<'info, Game>,
    #[account(mut, seeds = [b"mode", game.mode.to_le_bytes().as_ref()], bump)]
    pub mode_state: Account<'info, ModeState>,
}

#[derive(Accounts)]
pub struct OpenNoWinnerSettlement<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ DoorCashError::Unauthorized, has_one = house_wallet @ DoorCashError::HouseWalletMismatch)]
    pub game: Account<'info, Game>,
    #[account(mut, seeds = [b"vault", game.game_id.as_bytes()], bump = game.vault_bump)]
    /// CHECK: per-game vault PDA
    pub game_vault: AccountInfo<'info>,
    #[account(mut, seeds = [b"mode", game.mode.to_le_bytes().as_ref()], bump)]
    pub mode_state: Account<'info, ModeState>,
    #[account(mut, seeds = [b"mode-vault", game.mode.to_le_bytes().as_ref()], bump)]
    /// CHECK: mode rollover/refund vault PDA
    pub mode_vault: AccountInfo<'info>,
    #[account(mut)]
    /// CHECK: must match game.house_wallet
    pub house_wallet: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct PayNoWinnerBatch<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ DoorCashError::Unauthorized)]
    pub game: Account<'info, Game>,
    #[account(mut, seeds = [b"mode", game.mode.to_le_bytes().as_ref()], bump)]
    pub mode_state: Account<'info, ModeState>,
    #[account(mut, seeds = [b"mode-vault", game.mode.to_le_bytes().as_ref()], bump)]
    /// CHECK: mode rollover/refund vault PDA
    pub mode_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RepairNoWinnerStreakEntries<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ DoorCashError::Unauthorized)]
    pub game: Account<'info, Game>,
    #[account(mut, seeds = [b"mode", game.mode.to_le_bytes().as_ref()], bump)]
    pub mode_state: Account<'info, ModeState>,
}

#[derive(Accounts)]
pub struct CloseNoWinnerSettlement<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ DoorCashError::Unauthorized)]
    pub game: Account<'info, Game>,
    #[account(mut, seeds = [b"mode", game.mode.to_le_bytes().as_ref()], bump)]
    pub mode_state: Account<'info, ModeState>,
}

#[derive(Accounts)]
#[instruction(game_id: String)]
pub struct RefundPlayer<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"game", game_id.as_bytes()],
        bump,
        has_one = authority @ DoorCashError::Unauthorized,
        has_one = house_wallet @ DoorCashError::HouseWalletMismatch
    )]
    pub game: Account<'info, Game>,
    #[account(mut)]
    /// CHECK: must match game.house_wallet
    pub house_wallet: AccountInfo<'info>,
    #[account(mut, seeds = [b"vault", game_id.as_bytes()], bump = game.vault_bump)]
    /// CHECK: game vault PDA
    pub vault: AccountInfo<'info>,
    #[account(mut)]
    /// CHECK: refund recipient wallet
    pub player: AccountInfo<'info>,
    #[account(
        mut,
        seeds = [b"entry", game_id.as_bytes(), player.key().as_ref()],
        bump,
        constraint = player_entry.player == player.key() @ DoorCashError::Unauthorized
    )]
    pub player_entry: Account<'info, PlayerEntry>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(game_id: String)]
pub struct AdminGameVaultRecovery<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"game", game_id.as_bytes()],
        bump,
        has_one = authority @ DoorCashError::Unauthorized,
        has_one = house_wallet @ DoorCashError::HouseWalletMismatch
    )]
    pub game: Account<'info, Game>,
    #[account(mut)]
    /// CHECK: must match game.house_wallet
    pub house_wallet: AccountInfo<'info>,
    #[account(mut, seeds = [b"vault", game_id.as_bytes()], bump = game.vault_bump)]
    /// CHECK: game vault PDA
    pub vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(mode: u8)]
pub struct AdminModeVaultRecovery<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ DoorCashError::Unauthorized, has_one = house_wallet @ DoorCashError::HouseWalletMismatch)]
    pub game: Account<'info, Game>,
    #[account(mut)]
    /// CHECK: must match game.house_wallet
    pub house_wallet: AccountInfo<'info>,
    #[account(mut, seeds = [b"mode", mode.to_le_bytes().as_ref()], bump)]
    pub mode_state: Account<'info, ModeState>,
    #[account(mut, seeds = [b"mode-vault", mode.to_le_bytes().as_ref()], bump)]
    /// CHECK: mode rollover/refund vault PDA
    pub mode_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[event]
pub struct PlayerEntered {
    pub game_id: String,
    pub player: Pubkey,
    pub count: u16,
}

#[event]
pub struct SeedRevealed {
    pub game_id: String,
    pub seed: [u8; 32],
}

#[event]
pub struct WinnerSettlement {
    pub game_id: String,
    pub winner_count: u8,
    pub prize_each: u64,
    pub house_cut: u64,
    pub carry_in: u64,
    pub carry_out: u64,
}

#[event]
pub struct NoWinnerSettlementOpened {
    pub game_id: String,
    pub refund_round_id: u64,
    pub house_cut: u64,
    pub carry_in: u64,
    pub refund_pool: u64,
    pub carry_out: u64,
    pub refund_per_entry: u64,
    pub eligible_entry_count: u64,
}

#[event]
pub struct NoWinnerSettlementClosed {
    pub game_id: String,
    pub refund_round_id: u64,
    pub carry_out: u64,
}

#[event]
pub struct NoWinnerEntriesRepaired {
    pub game_id: String,
    pub refund_round_id: u64,
    pub repaired_entries: u64,
}

#[event]
pub struct GameCancelled {
    pub game_id: String,
}

#[event]
pub struct GameVaultRecovery {
    pub game_id: String,
    pub destination: Pubkey,
    pub amount: u64,
    pub recovery_kind: RecoveryKind,
}

#[event]
pub struct ModeVaultRecovery {
    pub mode: u8,
    pub destination: Pubkey,
    pub amount: u64,
}

#[error_code]
pub enum DoorCashError {
    #[msg("Not authorised to perform this action")]
    Unauthorized,
    #[msg("Game is not currently accepting players")]
    GameNotAccepting,
    #[msg("Lobby is full")]
    LobbyFull,
    #[msg("Not enough players to start")]
    NotEnoughPlayers,
    #[msg("Seed has already been revealed")]
    SeedAlreadyRevealed,
    #[msg("Seed must be revealed before settlement")]
    SeedNotRevealed,
    #[msg("Seed does not match committed hash")]
    SeedHashMismatch,
    #[msg("Invalid game status for this operation")]
    InvalidGameStatus,
    #[msg("Cannot cancel a game that is Active, Settling, or Complete")]
    CannotCancelActiveGame,
    #[msg("Game must be Cancelled to issue refunds")]
    GameNotCancelled,
    #[msg("Player has already been refunded")]
    AlreadyRefunded,
    #[msg("No winners provided")]
    NoWinners,
    #[msg("Winner account count does not match winner_count argument")]
    WinnerAccountMismatch,
    #[msg("Arithmetic overflow in settlement calculation")]
    ArithmeticOverflow,
    #[msg("House wallet does not match game record")]
    HouseWalletMismatch,
    #[msg("Invalid game configuration")]
    InvalidConfig,
    #[msg("Game ID exceeds maximum length")]
    GameIdTooLong,
    #[msg("Invalid game mode")]
    InvalidMode,
    #[msg("Mode account does not match game mode")]
    ModeMismatch,
    #[msg("A no-winner refund round is still open for this mode")]
    ModeRefundRoundOpen,
    #[msg("Batch account list is malformed")]
    BatchAccountMismatch,
    #[msg("Player entry does not belong to this game")]
    EntryGameMismatch,
    #[msg("No eligible entries are registered for this refund round")]
    NoEligibleRefundEntries,
    #[msg("Refund round is not open")]
    RefundRoundNotOpen,
    #[msg("Refund round id does not match the active mode state")]
    RefundRoundMismatch,
    #[msg("Refund round has not paid all eligible entries yet")]
    RefundRoundIncomplete,
    #[msg("Mode participant streak does not match the game streak")]
    StreakMismatch,
    #[msg("Winner entry does not match the supplied winner wallet")]
    InvalidWinnerEntry,
    #[msg("Winner list contains the same wallet more than once")]
    DuplicateWinner,
    #[msg("Vault does not contain enough lamports for the recorded pot")]
    InsufficientVaultFunds,
    #[msg("Emergency recovery delay has not elapsed")]
    EmergencyRecoveryTooEarly,
}

fn is_valid_mode(mode: u8) -> bool {
    matches!(mode, MODE_DAILY | MODE_HIGHROLLER | MODE_FLASH)
}

fn calculate_house_cut(ante_in: u64) -> Result<u64> {
    ante_in
        .checked_mul(HOUSE_EDGE_BPS)
        .ok_or_else(|| error!(DoorCashError::ArithmeticOverflow))?
        .checked_div(BASIS_POINTS)
        .ok_or_else(|| error!(DoorCashError::ArithmeticOverflow))
}

fn protected_game_vault_lamports(game: &Game) -> u64 {
    match game.status {
        GameStatus::Waiting | GameStatus::Lobby | GameStatus::Active => game.total_pot,
        GameStatus::Cancelled if matches!(game.settlement_kind, SettlementKind::Unsettled) => {
            game.total_pot
        }
        _ => 0,
    }
}

fn transfer_game_vault<'info>(
    game_id: &str,
    vault_bump: u8,
    from: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let game_id_bytes = game_id.as_bytes().to_vec();
    let signer_seeds: &[&[&[u8]]] = &[&[b"vault", game_id_bytes.as_slice(), &[vault_bump]]];
    invoke_signed(
        &system_instruction::transfer(from.key, to.key, amount),
        &[from.clone(), to.clone()],
        signer_seeds,
    )
    .map_err(Into::into)
}

fn transfer_mode_vault<'info>(
    mode: u8,
    from: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let signer_seeds: &[&[&[u8]]] =
        &[&[b"mode-vault", &[mode], &[mode_vault_bump(mode, from.key)?]]];
    invoke_signed(
        &system_instruction::transfer(from.key, to.key, amount),
        &[from.clone(), to.clone()],
        signer_seeds,
    )
    .map_err(Into::into)
}

fn mode_vault_bump(mode: u8, key: &Pubkey) -> Result<u8> {
    let (expected, bump) = Pubkey::find_program_address(&[b"mode-vault", &[mode]], &crate::ID);
    require!(expected == *key, DoorCashError::ModeMismatch);
    Ok(bump)
}
