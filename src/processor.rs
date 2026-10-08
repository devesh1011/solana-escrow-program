use solana_program::account_info::{AccountInfo, next_account_info};
use solana_program::entrypoint::ProgramResult;
use solana_program::program::{invoke, invoke_signed};
use solana_program::program_error::ProgramError::{
    self, AccountAlreadyInitialized, IllegalOwner, IncorrectAuthority, IncorrectProgramId,
    InsufficientFunds, InvalidAccountData, InvalidArgument, InvalidSeeds,
};
use solana_program::pubkey::{self, Pubkey};
use solana_program::{program_pack::Pack, rent::Rent, sysvar::Sysvar};
use solana_system_interface::instruction::create_account;
use spl_token_interface::{
    instruction::{initialize_account3, transfer_checked},
    state::{Account as TokenAccount, Mint},
};

use borsh::{BorshDeserialize, BorshSerialize};

#[derive(BorshSerialize, BorshDeserialize, PartialEq)]
enum AgreementStatus {
    Funded,
    Submitted,
    Paid,
    Cancelled,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct EscrowAccount {
    agreement_id: u64,
    employer_address: Pubkey,
    employee_address: Pubkey,
    token_mint: Pubkey,
    amount: u64,
    agreement_status: AgreementStatus,
    bump: u8,
}

impl EscrowAccount {
    const SPACE: usize = 32 + 32 + 32 + 8 + 8 + 1 + 1;
}

#[derive(BorshSerialize, BorshDeserialize)]
struct CreateAgreementInstruction {
    agreement_id: u64,
    employee_address: Pubkey,
    amount: u64,
}

pub fn process_instruction(
    pubkey: &Pubkey,
    accounts: &[AccountInfo],
    input: &[u8],
) -> ProgramResult {
    let (instruction_tag, instruction_data) = input
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;

    match *instruction_tag {
        0 => create_and_fund(pubkey, accounts, instruction_data),
        1 => submit_work(pubkey, accounts, instruction_data),
        2 => approve_and_release(pubkey, accounts, instruction_data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn create_and_fund(pubkey: &Pubkey, accounts: &[AccountInfo], input: &[u8]) -> ProgramResult {
    let mut iter = accounts.iter();

    // The client must pass the employer as the first account.
    let employer_account = next_account_info(&mut iter)?;
    let escrow_account = next_account_info(&mut iter)?;
    let system_program_account = next_account_info(&mut iter)?;
    let token_mint = next_account_info(&mut iter)?;
    let employer_token_account = next_account_info(&mut iter)?;
    let vault_account = next_account_info(&mut iter)?;
    let token_program_account: &AccountInfo<'_> = next_account_info(&mut iter)?;

    if !employer_account.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // deserializing and reading the agreement data instruction.
    let agreement_data: CreateAgreementInstruction =
        CreateAgreementInstruction::try_from_slice(input)?;

    if agreement_data.amount == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }

    let agreement_id_bytes: [u8; 8] = agreement_data.agreement_id.to_le_bytes();

    let seeds: &[&[u8]] = &[
        b"escrow",
        employer_account.key.as_ref(),
        agreement_id_bytes.as_ref(),
    ];

    let (pda, bump) = Pubkey::find_program_address(seeds, pubkey);

    if escrow_account.key != &pda {
        return Err(ProgramError::InvalidAccountData);
    }

    if !employer_account.is_writable || !escrow_account.is_writable {
        return Err(ProgramError::InvalidArgument);
    }

    if system_program_account.key != &solana_system_interface::program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    let space = EscrowAccount::SPACE;

    let rent: Rent = Rent::get()?;
    let required_lamports = rent.minimum_balance(space);

    let create_escrow_instruction = create_account(
        employer_account.key,
        escrow_account.key,
        required_lamports,
        space as u64,
        pubkey,
    );

    let bump_bytes: [u8; 1] = [bump];

    let signer_seeds: &[&[u8]] = &[
        b"escrow",
        employer_account.key.as_ref(),
        agreement_id_bytes.as_ref(),
        &bump_bytes,
    ];

    invoke_signed(&create_escrow_instruction, accounts, &[signer_seeds])?;

    if token_program_account.key != &spl_token_interface::ID || !token_program_account.executable {
        return Err(IncorrectProgramId);
    }

    if token_mint.owner != token_program_account.key
        || employer_token_account.owner != token_program_account.key
    {
        return Err(IllegalOwner);
    }

    if !employer_token_account.is_writable || !vault_account.is_writable {
        return Err(InvalidArgument);
    }

    let mint: Mint = Mint::unpack(&token_mint.try_borrow_data()?)?;

    let source = TokenAccount::unpack(&employer_token_account.try_borrow_data()?)?;

    if source.owner != *employer_account.key {
        return Err(IncorrectAuthority);
    }

    if source.mint != *token_mint.key {
        return Err(InvalidAccountData);
    }

    if source.amount < agreement_data.amount {
        return Err(InsufficientFunds);
    }

    // derive a separate vault PDA for this agreement.
    let vault_seeds: &[&[u8]; 2] = &[b"vault", escrow_account.key.as_ref()];

    let (vault_pda, vault_bump) = Pubkey::find_program_address(vault_seeds, pubkey);

    if vault_account.key != &vault_pda {
        return Err(InvalidSeeds);
    }

    if vault_account.owner != &solana_system_interface::program::ID
        || !vault_account.data_is_empty()
        || vault_account.lamports() != 0
    {
        return Err(AccountAlreadyInitialized);
    }

    let rent: Rent = Rent::get()?;
    let vault_bump_bytes = [vault_bump];

    let vault_signer_seeds = &[b"vault", escrow_account.key.as_ref(), &vault_bump_bytes];

    // creating vault instruciton

    let create_vault_instruction = create_account(
        employer_account.key,
        vault_account.key,
        rent.minimum_balance(TokenAccount::LEN),
        TokenAccount::LEN as u64,
        token_program_account.key,
    );

    invoke_signed(
        &create_vault_instruction,
        &[
            employer_account.clone(),
            vault_account.clone(),
            system_program_account.clone(),
        ],
        &[vault_signer_seeds],
    )?;

    let initialize_vault_instruction = initialize_account3(
        token_program_account.key,
        vault_account.key,
        token_mint.key,
        escrow_account.key,
    )?;

    invoke(
        &initialize_vault_instruction,
        &[
            vault_account.clone(),
            token_mint.clone(),
            token_program_account.clone(),
        ],
    )?;

    // transferring the employer's payment into the vault.
    let deposit_instruciton = transfer_checked(
        token_program_account.key,
        employer_token_account.key,
        token_mint.key,
        vault_account.key,
        employer_account.key,
        &[],
        agreement_data.amount,
        mint.decimals,
    )?;

    invoke(
        &deposit_instruciton,
        &[
            employer_token_account.clone(),
            token_mint.clone(),
            vault_account.clone(),
            employer_account.clone(),
            token_program_account.clone(),
        ],
    )?;

    // save the agreement afer the deposit succeeds
    let escrow_state = EscrowAccount {
        employer_address: *employer_account.key,
        employee_address: agreement_data.employee_address,
        token_mint: *token_mint.key,
        agreement_id: agreement_data.agreement_id,
        amount: agreement_data.amount,
        agreement_status: AgreementStatus::Funded,
        bump,
    };

    let mut data = escrow_account.try_borrow_mut_data()?;

    escrow_state
        .serialize(&mut &mut data[..])
        .map_err(|_| ProgramError::InvalidAccountData)?;

    Ok(())
}

fn submit_work(pubkey: &Pubkey, accounts: &[AccountInfo], _input: &[u8]) -> ProgramResult {
    // reading all the accounts using iter
    let mut iter = accounts.iter();

    let employee_account = next_account_info(&mut iter)?;
    let escrow_state_account: &AccountInfo<'_> = next_account_info(&mut iter)?;

    // validating the signer is employee
    if !employee_account.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    // checking if escrow state account is writable
    // and escrow_state_account's owner is the program
    if !escrow_state_account.is_writable || escrow_state_account.owner != pubkey {
        return Err(ProgramError::IncorrectAuthority);
    }

    // deserializing the escrow_account_data from the account
    let mut escrow_account_data =
        EscrowAccount::try_from_slice(&escrow_state_account.try_borrow_data()?)?;

    if employee_account.key != &escrow_account_data.employee_address {
        return Err(ProgramError::IncorrectAuthority);
    }

    // converting agreement id to bytes to be used with seeds
    let agreement_id_bytes = escrow_account_data.agreement_id.to_le_bytes();

    // writing seeds
    let seeds: &[&[u8]] = &[
        b"escrow",
        escrow_account_data.employer_address.as_ref(),
        agreement_id_bytes.as_ref(),
    ];

    // deriving the PDA using the seeds
    let (pda, _bump) = Pubkey::find_program_address(seeds, pubkey);

    // running more vaildation checks
    if pda != *escrow_state_account.key {
        return Err(ProgramError::InvalidSeeds);
    }

    // checking if the agreement status is already funded
    if escrow_account_data.agreement_status != AgreementStatus::Funded {
        return Err(ProgramError::InvalidAccountData);
    }

    // updating the escrow account data to SUBMITTED
    escrow_account_data.agreement_status = AgreementStatus::Submitted;

    let mut data = escrow_state_account.try_borrow_mut_data()?;

    // serializing the data and making change to the blockchain
    escrow_account_data
        .serialize(&mut &mut data[..])
        .map_err(|_| ProgramError::InvalidAccountData)?;

    Ok(())
}

fn approve_and_release(pubkey: &Pubkey, accounts: &[AccountInfo], input: &[u8]) -> ProgramResult {
    let mut iter = accounts.iter();

    let employer_account = next_account_info(&mut iter)?;
    let escrow_state_account = next_account_info(&mut iter)?;
    let token_mint_account = next_account_info(&mut iter)?;
    let vault_account = next_account_info(&mut iter)?;
    let employee_token_account = next_account_info(&mut iter)?;
    let token_program_account = next_account_info(&mut iter)?;

    if !employer_account.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    if !escrow_state_account.is_writable
        || !vault_account.is_writable
        || !employee_token_account.is_writable
    {
        return Err(ProgramError::InvalidArgument);
    }

    if escrow_state_account.owner != pubkey {
        return Err(ProgramError::IllegalOwner);
    }

    let mut escrow_data = EscrowAccount::try_from_slice(&escrow_state_account.try_borrow_data()?)
        .map_err(|_| ProgramError::InvalidAccountData)?;

    if &escrow_data.employer_address != employer_account.key {
        return Err(ProgramError::IncorrectAuthority);
    }

    let agreement_id_bytes = escrow_data.agreement_id.to_le_bytes();

    // writing seeds
    let seeds: &[&[u8]] = &[
        b"escrow",
        escrow_data.employer_address.as_ref(),
        agreement_id_bytes.as_ref(),
    ];

    let (pda, escrow_bump) = Pubkey::find_program_address(seeds, pubkey);

    if &pda != escrow_state_account.key {
        return Err(ProgramError::InvalidAccountOwner);
    }

    if escrow_data.agreement_status != AgreementStatus::Submitted {
        return Err(ProgramError::InvalidSeeds);
    }

    if token_program_account.key != &spl_token_interface::ID || !token_program_account.executable {
        return Err(InvalidArgument);
    }

    if token_mint_account.key != &escrow_data.token_mint {
        return Err(InvalidAccountData);
    }

    if *token_mint_account.owner != spl_token_interface::ID
        || *vault_account.owner != spl_token_interface::ID
        || *employee_token_account.owner != spl_token_interface::ID
    {
        return Err(IllegalOwner);
    }

    // creating the seeds
    let vault_seeds: &[&[u8]; 2] = &[b"vault", escrow_state_account.key.as_ref()];

    let (vault_pda, bump) = Pubkey::find_program_address(vault_seeds, pubkey);

    if *vault_account.key != vault_pda {
        return Err(InvalidAccountData);
    }

    let vault_data: TokenAccount = TokenAccount::unpack(&vault_account.try_borrow_data()?)?;
    let employee_token_data = TokenAccount::unpack(&employee_token_account.try_borrow_data()?)?;
    let mint = Mint::unpack(&token_mint_account.try_borrow_data()?)?;

    if vault_data.mint != escrow_data.token_mint
        || employee_token_data.mint != escrow_data.token_mint
    {
        return Err(InvalidAccountData);
    }

    if vault_account.owner != escrow_state_account.key {
        return Err(IncorrectAuthority);
    }

    if employee_token_data.owner != escrow_data.employee_address {
        return Err(IncorrectAuthority);
    }

    if vault_data.amount < escrow_data.amount {
        return Err(InsufficientFunds);
    }

    let move_token_to_employee = transfer_checked(
        &spl_token_interface::ID,
        vault_account.key,
        token_mint_account.key,
        employee_token_account.key,
        escrow_state_account.key,
        &[],
        escrow_data.amount,
        mint.decimals,
    )?;

    let escrow_bump_bytes = [escrow_bump];

    let escrow_signer_seeds = &[
        b"escrow",
        escrow_data.employer_address.as_ref(),
        agreement_id_bytes.as_ref(),
        &escrow_bump_bytes,
    ];

    invoke_signed(
        &move_token_to_employee,
        &[
            vault_account.clone(),
            token_mint_account.clone(),
            employee_token_account.clone(),
            escrow_state_account.clone(),
            token_program_account.clone(),
        ],
        &[escrow_signer_seeds],
    )?;

    escrow_data.agreement_status = AgreementStatus::Paid;

    let mut data = escrow_state_account.try_borrow_mut_data()?;

    // serializing the data and making change to the blockchain
    escrow_data
        .serialize(&mut &mut data[..])
        .map_err(|_| ProgramError::InvalidAccountData)?;

    Ok(())
}
