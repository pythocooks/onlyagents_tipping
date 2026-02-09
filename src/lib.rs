use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint,
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};

entrypoint!(process_instruction);

/// Instructions: 0=Initialize, 1=Tip, 2=UpdateFee
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let (tag, rest) = instruction_data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;

    match tag {
        0 => initialize(program_id, accounts, rest),
        1 => tip(program_id, accounts, rest),
        2 => update_fee(program_id, accounts, rest),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

#[derive(BorshSerialize, BorshDeserialize)]
pub struct TipConfig {
    pub is_initialized: bool,
    pub admin: Pubkey,
    pub treasury: Pubkey,
    pub fee_bps: u16,
    pub total_tips: u64,
    pub total_volume: u64,
}

const CONFIG_SIZE: usize = 1 + 32 + 32 + 2 + 8 + 8; // 83 bytes

fn get_config_pda(program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"config"], program_id)
}

/// Initialize: [fee_bps: u16]
/// Accounts: [config (w), treasury, admin (s,w), system_program]
fn initialize(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let config_acc = next_account_info(iter)?;
    let treasury = next_account_info(iter)?;
    let admin = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !admin.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    let fee_bps = u16::from_le_bytes(data[..2].try_into().map_err(|_| ProgramError::InvalidInstructionData)?);
    if fee_bps > 1000 {
        msg!("Fee too high: max 1000 bps (10%)");
        return Err(ProgramError::InvalidArgument);
    }

    let (pda, bump) = get_config_pda(program_id);
    if *config_acc.key != pda {
        return Err(ProgramError::InvalidSeeds);
    }

    let rent = Rent::get()?;
    let lamports = rent.minimum_balance(CONFIG_SIZE);

    invoke_signed(
        &system_instruction::create_account(
            admin.key, &pda, lamports, CONFIG_SIZE as u64, program_id,
        ),
        &[admin.clone(), config_acc.clone(), system_program.clone()],
        &[&[b"config", &[bump]]],
    )?;

    let config = TipConfig {
        is_initialized: true,
        admin: *admin.key,
        treasury: *treasury.key,
        fee_bps,
        total_tips: 0,
        total_volume: 0,
    };

    config.serialize(&mut &mut config_acc.data.borrow_mut()[..])?;
    Ok(())
}

/// Tip: [amount: u64]
/// Accounts: [config (w), tipper (s), tipper_token (w), creator_token (w), treasury_token (w), token_program]
fn tip(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let config_acc = next_account_info(iter)?;
    let tipper = next_account_info(iter)?;
    let tipper_token = next_account_info(iter)?;
    let creator_token = next_account_info(iter)?;
    let treasury_token = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !tipper.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    let (pda, _) = get_config_pda(program_id);
    if *config_acc.key != pda {
        return Err(ProgramError::InvalidSeeds);
    }

    let mut config = TipConfig::try_from_slice(&config_acc.data.borrow())?;
    if !config.is_initialized {
        return Err(ProgramError::UninitializedAccount);
    }

    let amount = u64::from_le_bytes(data[..8].try_into().map_err(|_| ProgramError::InvalidInstructionData)?);
    if amount == 0 {
        msg!("Tip amount must be > 0");
        return Err(ProgramError::InvalidArgument);
    }

    let fee = amount.checked_mul(config.fee_bps as u64).unwrap() / 10_000;
    let creator_amount = amount.checked_sub(fee).unwrap();

    // Transfer to creator
    invoke(
        &spl_token::instruction::transfer(
            token_program.key, tipper_token.key, creator_token.key, tipper.key, &[], creator_amount,
        )?,
        &[tipper_token.clone(), creator_token.clone(), tipper.clone()],
    )?;

    // Transfer fee to treasury
    if fee > 0 {
        if *treasury_token.key != config.treasury {
            msg!("Treasury mismatch");
            return Err(ProgramError::InvalidArgument);
        }
        invoke(
            &spl_token::instruction::transfer(
                token_program.key, tipper_token.key, treasury_token.key, tipper.key, &[], fee,
            )?,
            &[tipper_token.clone(), treasury_token.clone(), tipper.clone()],
        )?;
    }

    config.total_tips += 1;
    config.total_volume += amount;
    config.serialize(&mut &mut config_acc.data.borrow_mut()[..])?;

    msg!("Tip: {} to creator, {} fee", creator_amount, fee);
    Ok(())
}

/// UpdateFee: [new_fee_bps: u16]
/// Accounts: [config (w), admin (s)]
fn update_fee(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let config_acc = next_account_info(iter)?;
    let admin = next_account_info(iter)?;

    if !admin.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    let (pda, _) = get_config_pda(program_id);
    if *config_acc.key != pda {
        return Err(ProgramError::InvalidSeeds);
    }

    let mut config = TipConfig::try_from_slice(&config_acc.data.borrow())?;
    if !config.is_initialized {
        return Err(ProgramError::UninitializedAccount);
    }
    if config.admin != *admin.key {
        return Err(ProgramError::IllegalOwner);
    }

    let new_fee = u16::from_le_bytes(data[..2].try_into().map_err(|_| ProgramError::InvalidInstructionData)?);
    if new_fee > 1000 {
        return Err(ProgramError::InvalidArgument);
    }

    config.fee_bps = new_fee;
    config.serialize(&mut &mut config_acc.data.borrow_mut()[..])?;
    Ok(())
}
