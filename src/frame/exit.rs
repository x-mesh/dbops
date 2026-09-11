//! Process exit-code mappers. Two independent vocabularies live here:
//! nagios-style check exit codes (driven by [`CheckStatus`]) for check
//! subcommands, and general unix exit codes for everything else (init,
//! reset, seed, connection probes, ...).

use crate::frame::result::CheckStatus;

/// Nagios/check_postgres plugin exit codes.
///
/// This mapping is a public contract every nagios-compatible monitoring
/// system relies on (0=ok, 1=warning, 2=critical, 3=unknown). The numbers
/// themselves must never change, only what maps to them.
#[allow(dead_code)] // consumed by domain modules once they stop being stubs
pub fn from_status(status: CheckStatus) -> u8 {
    match status {
        CheckStatus::Ok => 0,
        CheckStatus::Warning => 1,
        CheckStatus::Critical => 2,
        CheckStatus::Unknown => 3,
    }
}

/// General-purpose exit codes for subcommands that are not nagios-style
/// checks (`init`, `reset`, `seed`, confirmation prompts, connection
/// probes, ...). Distinct from the nagios vocabulary above so a caller
/// can never accidentally return `2` and have it misread as "critical".
#[allow(dead_code)] // consumed by domain modules once they stop being stubs
pub mod unix {
    pub const SUCCESS: u8 = 0;
    pub const GENERAL_ERROR: u8 = 1;
    pub const CONFIRMATION_DECLINED: u8 = 2;
    pub const ARGUMENT_ERROR: u8 = 3;
    pub const CONNECTION_FAILED: u8 = 4;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nagios_mapping_covers_every_status() {
        assert_eq!(from_status(CheckStatus::Ok), 0);
        assert_eq!(from_status(CheckStatus::Warning), 1);
        assert_eq!(from_status(CheckStatus::Critical), 2);
        assert_eq!(from_status(CheckStatus::Unknown), 3);
    }

    #[test]
    fn unix_codes_match_the_agreed_convention() {
        assert_eq!(unix::SUCCESS, 0);
        assert_eq!(unix::GENERAL_ERROR, 1);
        assert_eq!(unix::CONFIRMATION_DECLINED, 2);
        assert_eq!(unix::ARGUMENT_ERROR, 3);
        assert_eq!(unix::CONNECTION_FAILED, 4);
    }
}
