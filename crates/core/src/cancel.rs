//! Cooperative cancellation token. `cli` installs a Ctrl-C handler that sets it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::error::{CoreError, Result};

#[derive(Clone, Default)]
pub struct Token {
    flag: Arc<AtomicBool>,
}

impl Token {
    pub fn new() -> Self {
        Token::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Return `Err(Cancelled)` if the token has been set.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(CoreError::Cancelled)
        } else {
            Ok(())
        }
    }
}
