// Copyright © Aptos Foundation

use crate::{
    jwks::rsa::RSA_JWK,
    on_chain_config::CurrentTimeMicroseconds,
    transaction::{
        authenticator::{
            AnyPublicKey, AnySignature, EphemeralPublicKey, EphemeralSignature, MAX_NUM_OF_SIGS,
        },
        SignedTransaction,
    },
};
use anyhow::{anyhow, ensure, Context, Ok, Result};
use aptos_crypto::{poseidon_bn254, CryptoMaterialError, ValidCryptoMaterial};
use ark_bn254;
use ark_serialize::CanonicalSerialize;
use base64::{URL_SAFE, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_with::skip_serializing_none;
use std::{
    collections::BTreeMap,
    str,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const PEPPER_NUM_BYTES: usize = poseidon_bn254::BYTES_PACKED_PER_SCALAR;
pub const EPK_BLINDER_NUM_BYTES: usize = poseidon_bn254::BYTES_PACKED_PER_SCALAR;
pub const NONCE_NUM_BYTES: usize = 32;
pub const IDC_NUM_BYTES: usize = 32;

// TODO(ZkIdGroth16Zkp): add some static asserts here that these don't exceed the MAX poseidon input sizes
// TODO(ZkIdGroth16Zkp): determine what our circuit will accept

pub const MAX_EPK_BYTES: usize = 93; // Supports public key lengths of up to 93 bytes.
pub const MAX_ISS_BYTES: usize = 248;
pub const MAX_AUD_VAL_BYTES: usize = 248;
pub const MAX_UID_KEY_BYTES: usize = 248;
pub const MAX_UID_VAL_BYTES: usize = 248;
pub const MAX_JWT_HEADER_BYTES: usize = 248;

pub const MAX_ZK_PUBLIC_KEY_BYTES: usize = MAX_ISS_BYTES + MAX_EPK_BYTES;

// TODO(ZkIdGroth16Zkp): determine max length of zkSNARK + OIDC overhead + ephemeral pubkey and signature
pub const MAX_ZK_SIGNATURE_BYTES: usize = 2048;

// TODO(ZkIdGroth16Zkp): each zkID Groth16 proof will take ~2 ms to verify, or so. We cannot verify too many due to DoS.
//  Figure out what this should be.
pub const MAX_ZK_ID_AUTHENTICATORS_ALLOWED: usize = 10;

// How far in the future from the JWT issued at time the EPK expiry can be set.
pub const MAX_EXPIRY_HORIZON_SECS: u64 = 1728000000; // 20000 days TODO(zkid): finalize this value

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct JwkId {
    /// The OIDC provider associated with this JWK
    pub iss: String,
    /// The Key ID associated with this JWK (https://datatracker.ietf.org/doc/html/rfc7517#section-4.5)
    pub kid: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
pub struct OpenIdSig {
    /// The base64url encoded JWS signature of the OIDC JWT (https://datatracker.ietf.org/doc/html/rfc7515#section-3)
    pub jwt_sig: String,
    /// The base64url encoded JSON payload of the OIDC JWT (https://datatracker.ietf.org/doc/html/rfc7519#section-3)
    pub jwt_payload: String,
    /// The name of the key in the claim that maps to the user identifier; e.g., "sub" or "email"
    pub uid_key: String,
    /// The random value used to obfuscate the EPK from OIDC providers in the nonce field
    pub epk_blinder: [u8; EPK_BLINDER_NUM_BYTES],
    /// The privacy-preserving value used to calculate the identity commitment. It is typically uniquely derived from `(iss, client_id, uid_key, uid_val)`.
    pub pepper: Pepper,
}

impl OpenIdSig {
    /// Verifies an `OpenIdSig` by doing the following checks:
    ///  1. Check that the ephemeral public key lifespan is under MAX_EXPIRY_HORIZON_SECS
    ///  2. Check that the iss claim in the ZkIdPublicKey matches the one in the jwt_payload
    ///  3. Check that the identity commitment in the ZkIdPublicKey matches the one constructed from the jwt_payload
    ///  4. Check that the nonce constructed from the ephemeral public key, blinder, and exp_timestamp_secs matches the one in the jwt_payload
    pub fn verify_jwt_claims(
        &self,
        exp_timestamp_secs: u64,
        epk: &EphemeralPublicKey,
        pk: &ZkIdPublicKey,
    ) -> Result<()> {
        let jwt_payload_json = base64url_to_str(&self.jwt_payload)?;
        let claims: Claims = serde_json::from_str(&jwt_payload_json)?;

        // TODO(zkid): Store MAX_EXPIRY_HORIZON_SECS in a resource in zkid.move. Then, move this check
        //  into the prologue for the ZK-less OpenID path.
        let max_expiration_date =
            seconds_from_epoch(claims.oidc_claims.iat + MAX_EXPIRY_HORIZON_SECS);
        let expiration_date: SystemTime = seconds_from_epoch(exp_timestamp_secs);
        ensure!(
            expiration_date < max_expiration_date,
            "The ephemeral public key's expiration date is too far into the future"
        );

        ensure!(
            claims.oidc_claims.iss.eq(&pk.iss),
            "'iss' claim was supposed to match \"{}\"",
            pk.iss
        );

        ensure!(
            self.uid_key.eq("sub") || self.uid_key.eq("email"),
            "uid_key must be either 'sub' or 'email', was \"{}\"",
            self.uid_key
        );
        let uid_val = claims.get_uid_val(&self.uid_key)?;

        ensure!(
            IdCommitment::new_from_preimage(
                &claims.oidc_claims.aud,
                &self.uid_key,
                &uid_val,
                &self.pepper
            )?
            .eq(&pk.idc),
            "Address IDC verification failed"
        );

        ensure!(
            self.reconstruct_oauth_nonce(exp_timestamp_secs, epk)?
                .eq(&claims.oidc_claims.nonce),
            "'nonce' claim did not contain the expected EPK and expiration date commitment"
        );

        Ok(())
    }

    pub fn verify_jwt_signature(&self, rsa_jwk: RSA_JWK, jwt_header: &String) -> Result<()> {
        let jwt_payload = &self.jwt_payload;
        let jwt_sig = &self.jwt_sig;
        let jwt_token = format!("{}.{}.{}", jwt_header, jwt_payload, jwt_sig);
        rsa_jwk.verify_signature(&jwt_token)?;
        Ok(())
    }

    pub fn reconstruct_oauth_nonce(
        &self,
        exp_timestamp_secs: u64,
        epk: &EphemeralPublicKey,
    ) -> Result<String> {
        let mut frs = poseidon_bn254::pad_and_pack_bytes_to_scalars_with_len(
            epk.to_bytes().as_slice(),
            MAX_EPK_BYTES,
        )?;

        frs.push(ark_bn254::Fr::from(exp_timestamp_secs));
        frs.push(poseidon_bn254::pack_bytes_to_one_scalar(
            &self.epk_blinder[..],
        )?);

        let nonce_fr = poseidon_bn254::hash_scalars(frs)?;
        let mut nonce_bytes = [0u8; NONCE_NUM_BYTES];
        nonce_fr.serialize_uncompressed(&mut nonce_bytes[..])?;

        Ok(base64::encode_config(nonce_bytes, URL_SAFE_NO_PAD))
    }
}

impl TryFrom<&[u8]> for OpenIdSig {
    type Error = CryptoMaterialError;

    fn try_from(bytes: &[u8]) -> Result<Self, CryptoMaterialError> {
        bcs::from_bytes::<OpenIdSig>(bytes).map_err(|_e| CryptoMaterialError::DeserializationError)
    }
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize)]
pub struct OidcClaims {
    iss: String,
    aud: String,
    sub: String,
    nonce: String,
    iat: u64,
    email: Option<String>,
    email_verified: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    #[serde(flatten)]
    oidc_claims: OidcClaims,
    #[serde(default)]
    additional_claims: BTreeMap<String, Value>,
}

impl Claims {
    fn get_uid_val(&self, uid_key: &String) -> Result<String> {
        match uid_key.as_str() {
            "email" => {
                let email_verified = self
                    .oidc_claims
                    .email_verified
                    .clone()
                    .context("'email_verified' claim is missing")?;
                // the 'email_verified' claim may be a boolean or a boolean-as-a-string.
                let email_verified_as_bool = email_verified.as_bool().unwrap_or(false);
                let email_verified_as_str = email_verified.as_str().unwrap_or("false");
                ensure!(
                    email_verified_as_bool || email_verified_as_str.eq("true"),
                    "'email_verified' claim was not \"true\""
                );
                self.oidc_claims
                    .email
                    .clone()
                    .context("email claim missing on jwt")
            },
            "sub" => Ok(self.oidc_claims.sub.clone()),
            _ => {
                let uid_val = self
                    .additional_claims
                    .get(uid_key)
                    .context(format!("{} claim missing on jwt", uid_key))?
                    .as_str()
                    .context(format!("{} value is not a string", uid_key))?;
                Ok(uid_val.to_string())
            },
        }
    }
}

pub type G1 = Vec<String>;
pub type G2 = Vec<Vec<String>>;
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
pub struct Groth16Zkp {
    a: G1,
    b: G2,
    c: G1,
}

impl TryFrom<&[u8]> for Groth16Zkp {
    type Error = CryptoMaterialError;

    fn try_from(bytes: &[u8]) -> Result<Self, CryptoMaterialError> {
        bcs::from_bytes::<Groth16Zkp>(bytes).map_err(|_e| CryptoMaterialError::DeserializationError)
    }
}

/// Allows us to support direct verification of OpenID signatures, in the rare case that we would
/// need to turn off ZK proofs due to a bug in the circuit.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
pub enum ZkpOrOpenIdSig {
    Groth16Zkp(Groth16Zkp),
    OpenIdSig(OpenIdSig),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
pub struct ZkIdSignature {
    /// A \[ZKPoK of an\] OpenID signature over several relevant fields (e.g., `aud`, `sub`, `iss`,
    /// `nonce`) where `nonce` contains a commitment to `ephemeral_pubkey` and an expiration time
    /// `exp_timestamp_secs`.
    pub sig: ZkpOrOpenIdSig,

    /// The header contains two relevant fields:
    ///  1. `kid`, which indicates which of the OIDC provider's JWKs should be used to verify the
    ///     \[ZKPoK of an\] OpenID signature.,
    ///  2. `alg`, which indicates which type of signature scheme was used to sign the JWT
    pub jwt_header: String,

    /// The expiry time of the `ephemeral_pubkey` represented as a UNIX epoch timestamp in seconds.
    pub exp_timestamp_secs: u64,

    /// A short lived public key used to verify the `ephemeral_signature`.
    pub ephemeral_pubkey: EphemeralPublicKey,
    /// The signature of the transaction signed by the private key of the `ephemeral_pubkey`.
    pub ephemeral_signature: EphemeralSignature,
}

impl TryFrom<&[u8]> for ZkIdSignature {
    type Error = CryptoMaterialError;

    fn try_from(bytes: &[u8]) -> Result<Self, CryptoMaterialError> {
        bcs::from_bytes::<ZkIdSignature>(bytes)
            .map_err(|_e| CryptoMaterialError::DeserializationError)
    }
}

impl ValidCryptoMaterial for ZkIdSignature {
    fn to_bytes(&self) -> Vec<u8> {
        bcs::to_bytes(&self).expect("Only unhandleable errors happen here.")
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JWTHeader {
    pub kid: String,
    pub alg: String,
}

impl ZkIdSignature {
    pub fn parse_jwt_header(&self) -> Result<JWTHeader> {
        let jwt_header_json = base64url_to_str(&self.jwt_header)?;
        let header: JWTHeader = serde_json::from_str(&jwt_header_json)?;
        Ok(header)
    }

    pub fn verify_expiry(&self, current_time: &CurrentTimeMicroseconds) -> Result<()> {
        let block_time = UNIX_EPOCH + Duration::from_micros(current_time.microseconds);
        let expiry_time = seconds_from_epoch(self.exp_timestamp_secs);

        if block_time > expiry_time {
            Err(anyhow!("zkID Signature is expired"))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Pepper(pub(crate) [u8; PEPPER_NUM_BYTES]);

impl Pepper {
    pub fn new(bytes: [u8; PEPPER_NUM_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn to_bytes(&self) -> &[u8; PEPPER_NUM_BYTES] {
        &self.0
    }

    #[cfg(test)]
    pub fn from_number(num: u128) -> Self {
        let big_int = num_bigint::BigUint::from(num);
        let bytes: Vec<u8> = big_int.to_bytes_le();
        let mut extended_bytes = [0u8; PEPPER_NUM_BYTES];
        extended_bytes[..bytes.len()].copy_from_slice(&bytes);
        Self(extended_bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct IdCommitment(pub(crate) [u8; IDC_NUM_BYTES]);

impl IdCommitment {
    pub fn new_from_preimage(
        aud: &str,
        uid_key: &str,
        uid_val: &str,
        pepper: &Pepper,
    ) -> Result<Self> {
        let aud_val_hash = poseidon_bn254::pad_and_hash_string(aud, MAX_AUD_VAL_BYTES)?;
        let uid_key_hash = poseidon_bn254::pad_and_hash_string(uid_key, MAX_UID_KEY_BYTES)?;
        let uid_val_hash = poseidon_bn254::pad_and_hash_string(uid_val, MAX_UID_VAL_BYTES)?;
        let pepper_scalar = poseidon_bn254::pack_bytes_to_one_scalar(pepper.0.as_slice())?;

        let fr = poseidon_bn254::hash_scalars(vec![
            aud_val_hash,
            uid_key_hash,
            uid_val_hash,
            pepper_scalar,
        ])?;

        let mut idc_bytes = [0u8; IDC_NUM_BYTES];
        fr.serialize_uncompressed(&mut idc_bytes[..])?;
        Ok(IdCommitment(idc_bytes))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        bcs::to_bytes(&self).expect("Only unhandleable errors happen here.")
    }
}

impl TryFrom<&[u8]> for IdCommitment {
    type Error = CryptoMaterialError;

    fn try_from(_value: &[u8]) -> Result<Self, Self::Error> {
        bcs::from_bytes::<IdCommitment>(_value)
            .map_err(|_e| CryptoMaterialError::DeserializationError)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ZkIdPublicKey {
    /// The OIDC provider.
    pub iss: String,

    /// SNARK-friendly commitment to:
    /// 1. The application's ID; i.e., the `aud` field in the signed OIDC JWT representing the OAuth client ID.
    /// 2. The OIDC provider's internal identifier for the user; e.g., the `sub` field in the signed OIDC JWT
    ///    which is Google's internal user identifier for bob@gmail.com, or the `email` field.
    ///
    /// e.g., H(aud || uid_key || uid_val || pepper), where `pepper` is the commitment's randomness used to hide
    ///  `aud` and `sub`.
    pub idc: IdCommitment,
}

impl ZkIdPublicKey {
    pub fn to_bytes(&self) -> Vec<u8> {
        bcs::to_bytes(&self).expect("Only unhandleable errors happen here.")
    }
}

impl TryFrom<&[u8]> for ZkIdPublicKey {
    type Error = CryptoMaterialError;

    fn try_from(_value: &[u8]) -> Result<Self, Self::Error> {
        bcs::from_bytes::<ZkIdPublicKey>(_value)
            .map_err(|_e| CryptoMaterialError::DeserializationError)
    }
}

pub fn get_zkid_authenticators(
    transaction: &SignedTransaction,
) -> Result<Vec<(ZkIdPublicKey, ZkIdSignature)>> {
    // Check all the signers in the TXN
    let single_key_authenticators = transaction
        .authenticator_ref()
        .to_single_key_authenticators()?;
    let mut authenticators = Vec::with_capacity(MAX_NUM_OF_SIGS);
    for authenticator in single_key_authenticators {
        if let (AnyPublicKey::ZkId { public_key }, AnySignature::ZkId { signature }) =
            (authenticator.public_key(), authenticator.signature())
        {
            authenticators.push((public_key.clone(), signature.clone()))
        }
    }
    Ok(authenticators)
}

fn base64url_to_str(b64: &str) -> Result<String> {
    let decoded_bytes = base64::decode_config(b64, URL_SAFE)?;
    // Convert the decoded bytes to a UTF-8 string
    let str = String::from_utf8(decoded_bytes)?;
    Ok(str)
}

fn seconds_from_epoch(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}
