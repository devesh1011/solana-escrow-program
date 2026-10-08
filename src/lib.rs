// Escrow Program

use solana_program::account_info::AccountInfo;
use solana_program::entrypoint::{entrypoint, ProgramResult};
use solana_program::pubkey::Pubkey;

mod processor;

entrypoint!(process_instruction);

pub fn process_instruction(
    _pubkey: &Pubkey,          // the pub key where this program is deployed
    _accounts: &[AccountInfo], // array of all the accounts program going to read or write to
    _instruction_data: &[u8], // array of instruction max is 32 bytes (actual data which is going to be used in the program)
) -> ProgramResult {
    processor::process_instruction(_pubkey, _accounts, _instruction_data)
}
