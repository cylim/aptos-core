// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::jwks;
use aptos_crypto_derive::{BCSCryptoHash, CryptoHasher};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, CryptoHasher, BCSCryptoHash)]
pub enum ValidatorTransaction {
    DummyTopic1(DummyValidatorTransaction),
    #[cfg(any(test, feature = "fuzzing"))]
    DummyTopic2(DummyValidatorTransaction),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, CryptoHasher, BCSCryptoHash)]
pub struct DummyValidatorTransaction {
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
}

impl ValidatorTransaction {
    pub fn dummy1(payload: Vec<u8>) -> Self {
        Self::DummyTopic1(DummyValidatorTransaction { payload })
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub fn dummy2(payload: Vec<u8>) -> Self {
        Self::DummyTopic2(DummyValidatorTransaction { payload })
    }

    pub fn size_in_bytes(&self) -> usize {
        bcs::serialized_size(self).unwrap()
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
#[allow(non_camel_case_types)]
pub enum Topic {
    DKG,
    JWK_CONSENSUS(jwks::Issuer),
    DUMMY1,
    #[cfg(any(test, feature = "fuzzing"))]
    DUMMY2,
}
