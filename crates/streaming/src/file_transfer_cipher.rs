//! Transfer encryption boundary.

use crate::TransferError;

/// Authenticated encryption boundary for transfer chunks.
pub trait TransferCipher: std::fmt::Debug + Send + Sync {
    /// Seal plaintext.
    ///
    /// # Errors
    /// Returns [`TransferError::Encryption`] on backend failures.
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, TransferError>;

    /// Open ciphertext.
    ///
    /// # Errors
    /// Returns [`TransferError::Encryption`] on authentication/decode failure.
    fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, TransferError>;
}

/// No-op cipher for testing and local integration only.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlaintextTransferCipher;

impl TransferCipher for PlaintextTransferCipher {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, TransferError> {
        Ok(plaintext.to_vec())
    }

    fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, TransferError> {
        Ok(ciphertext.to_vec())
    }
}
