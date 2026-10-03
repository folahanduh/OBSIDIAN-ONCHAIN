use anchor_lang::prelude::*;

#[error_code]
#[derive(PartialEq, Eq)]
pub enum GuardError {
    #[msg("Program is paused")]
    Paused,
    #[msg("Invalid economics or configuration")]
    InvalidConfig,
    #[msg("Address is on the sanctions denylist")]
    Denylisted,
    #[msg("Denylist is full")]
    DenylistFull,
    #[msg("Markup outside the configured bounds")]
    MarkupOutOfBounds,
    #[msg("Fee must be charged in a configured fee mint on the chosen side")]
    BadFeeMint,
    #[msg("Fee pool not initialized")]
    PoolNotInitialized,
    #[msg("Input and output mints must differ")]
    SameMint,
    #[msg("Token account not owned by the signer")]
    WrongOwner,
    #[msg("pre_swap must be a top-level instruction of this program")]
    NotTopLevel,
    #[msg("Transaction must be: pre_swap, one allowed router instruction, post_swap")]
    BadLayout,
    #[msg("Router program is not on the allowlist")]
    RouterNotAllowed,
    #[msg("post_swap does not reference this session")]
    SessionMismatch,
    #[msg("Swap spent more input than declared")]
    OverSpend,
    #[msg("Output below minimum (slippage limit)")]
    SlippageExceeded,
    #[msg("Output balance decreased")]
    OutputDecreased,
    #[msg("Distribution epoch has not elapsed")]
    EpochNotElapsed,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("Insufficient stake")]
    InsufficientStake,
    #[msg("Nothing to withdraw")]
    NothingToWithdraw,
    #[msg("Cooldown still active")]
    CooldownActive,
    #[msg("Amount must be non-zero")]
    ZeroAmount,
    #[msg("Nothing to claim")]
    NothingToClaim,
}

impl From<tenebra_tokenomics::TokenomicsError> for GuardError {
    fn from(e: tenebra_tokenomics::TokenomicsError) -> Self {
        use tenebra_tokenomics::TokenomicsError as T;
        match e {
            T::InvalidConfig => GuardError::InvalidConfig,
            T::Overflow => GuardError::Overflow,
            T::InsufficientStake => GuardError::InsufficientStake,
            T::NothingToWithdraw => GuardError::NothingToWithdraw,
            T::CooldownActive => GuardError::CooldownActive,
            T::ZeroAmount => GuardError::ZeroAmount,
        }
    }
}

/// `?`-friendly conversion from tokenomics results.
pub trait TkResultExt<T> {
    fn tk(self) -> Result<T>;
}

impl<T> TkResultExt<T> for core::result::Result<T, tenebra_tokenomics::TokenomicsError> {
    fn tk(self) -> Result<T> {
        self.map_err(|e| error!(GuardError::from(e)))
    }
}
