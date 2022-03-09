use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

use bincode::Options;
use k256::ecdsa::{
    signature::{Signer, Verifier},
    Signature, SigningKey, VerifyingKey,
};
use serde::{Deserialize, Serialize};
use serde_derive::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedMessage<M> {
    inner: Vec<u8>,
    // it seems like `Signature` has a buggy serde implementation, which cannot
    // work well with bincode's `deserialize_from`
    // signature: Signature, // do I want to generalize signing algorithm?
    signature: Vec<u8>,
    _marker: PhantomData<M>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InauthenticMessage;

impl Display for InauthenticMessage {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "inauthentic message")
    }
}
impl Error for InauthenticMessage {}

#[derive(Debug, Clone)]
pub struct VerifiedMessage<M>(M, SignedMessage<M>);

impl<M> Deref for VerifiedMessage<M> {
    type Target = M;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<M> DerefMut for VerifiedMessage<M> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<M> SignedMessage<M> {
    pub fn sign(message: M, key: &SigningKey) -> Self
    where
        M: Serialize,
    {
        let inner = bincode::DefaultOptions::new().serialize(&message).unwrap();
        let signature: Signature = key.sign(&inner);
        Self {
            inner,
            signature: signature.to_vec(),
            _marker: PhantomData,
        }
    }

    pub fn verify(self, key: &VerifyingKey) -> Result<VerifiedMessage<M>, InauthenticMessage>
    where
        M: for<'a> Deserialize<'a>,
    {
        let signature: Signature = if let Ok(signature) = self.signature[..].try_into() {
            signature
        } else {
            return Err(InauthenticMessage);
        };
        if key.verify(&self.inner, &signature).is_ok() {
            if let Ok(message) = bincode::DefaultOptions::new().deserialize(&self.inner) {
                return Ok(VerifiedMessage(message, self));
            }
        }
        Err(InauthenticMessage)
    }

    // upgrade to VerifiedMessage as well?
    pub fn assume_verified(self) -> M
    where
        M: for<'a> Deserialize<'a>,
    {
        bincode::DefaultOptions::new()
            .deserialize(&self.inner)
            .unwrap()
    }
}

impl<M> VerifiedMessage<M> {
    pub fn signed_message(&self) -> &SignedMessage<M> {
        &self.1
    }
}
