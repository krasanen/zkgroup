//
// Copyright (C) 2020 Signal Messenger, LLC.
// All rights reserved.
//
// SPDX-License-Identifier: GPL-3.0-only
//

#![allow(non_snake_case)]

use crate::api;
use crate::common::constants::*;
use crate::common::errors::*;
use crate::common::simple_types::*;
use crate::crypto;
use aead::{generic_array::GenericArray, Aead, NewAead};
use aes_gcm_siv::Aes256GcmSiv;
use poksho::ShoSha256;
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Serialize, Deserialize, Default)]
pub struct GroupMasterKey {
    pub(crate) bytes: [u8; 32],
}

#[derive(Copy, Clone, Serialize, Deserialize)]
pub struct GroupSecretParams {
    pub(crate) uid_enc_key_pair: crypto::uid_encryption::KeyPair,
    pub(crate) profile_key_enc_key_pair: crypto::profile_key_encryption::KeyPair,
    sig_key_pair: crypto::signature::KeyPair,
    master_key: GroupMasterKey,
    group_id: GroupIdentifierBytes,
}

#[derive(Copy, Clone, Serialize, Deserialize)]
pub struct GroupPublicParams {
    pub(crate) uid_enc_public_key: crypto::uid_encryption::PublicKey,
    pub(crate) profile_key_enc_public_key: crypto::profile_key_encryption::PublicKey,
    sig_public_key: crypto::signature::PublicKey,
    group_id: GroupIdentifierBytes,
}

impl GroupMasterKey {
    pub fn new(bytes: [u8; 32]) -> Self {
        GroupMasterKey { bytes }
    }
}

impl GroupSecretParams {
    pub fn generate(randomness: RandomnessBytes) -> Self {
        let mut master_key: GroupMasterKey = Default::default();
        master_key.bytes.copy_from_slice(
            &ShoSha256::shohash(
                b"Signal_ZKGroup_Master_Random",
                &randomness,
                GROUP_MASTER_KEY_LEN as u64,
            )[0..GROUP_MASTER_KEY_LEN],
        );
        GroupSecretParams::derive_from_master_key(master_key)
    }

    pub fn derive_from_master_key(master_key: GroupMasterKey) -> Self {
        let uid_enc_key_pair = crypto::uid_encryption::KeyPair::derive_from(master_key.bytes);
        let profile_key_enc_key_pair =
            crypto::profile_key_encryption::KeyPair::derive_from(master_key.bytes);
        let sig_key_pair = crypto::signature::KeyPair::derive_from(
            &master_key.bytes,
            b"Signal_ZKGroup_Sig_Client_KeyDerive",
        );

        let mut group_id: GroupIdentifierBytes = Default::default();
        group_id.copy_from_slice(
            &ShoSha256::shohash(
                b"Signal_ZKGroup_GroupId",
                &master_key.bytes,
                GROUP_IDENTIFIER_LEN as u64,
            )[0..GROUP_IDENTIFIER_LEN],
        );

        Self {
            uid_enc_key_pair,
            profile_key_enc_key_pair,
            sig_key_pair,
            master_key,
            group_id,
        }
    }

    pub fn get_master_key(&self) -> GroupMasterKey {
        self.master_key
    }

    pub fn get_group_identifier(&self) -> GroupIdentifierBytes {
        self.group_id
    }

    pub fn get_public_params(&self) -> GroupPublicParams {
        GroupPublicParams {
            uid_enc_public_key: self.uid_enc_key_pair.get_public_key(),
            profile_key_enc_public_key: self.profile_key_enc_key_pair.get_public_key(),
            sig_public_key: self.sig_key_pair.get_public_key(),
            group_id: self.group_id,
        }
    }

    pub fn sign(
        &self,
        randomness: RandomnessBytes,
        message: &[u8],
    ) -> Result<ChangeSignatureBytes, ZkGroupError> {
        self.sig_key_pair.sign(message, randomness)
    }

    pub fn encrypt_uuid(&self, uid_bytes: UidBytes) -> api::groups::UuidCiphertext {
        let uid = crypto::uid_struct::UidStruct::new(uid_bytes);
        self.encrypt_uid_struct(uid)
    }

    pub fn encrypt_uid_struct(
        &self,
        uid: crypto::uid_struct::UidStruct,
    ) -> api::groups::UuidCiphertext {
        let ciphertext = self.uid_enc_key_pair.encrypt(uid);
        api::groups::UuidCiphertext { ciphertext }
    }

    pub fn decrypt_uuid(
        &self,
        ciphertext: api::groups::UuidCiphertext,
    ) -> Result<UidBytes, ZkGroupError> {
        let uid = self.uid_enc_key_pair.decrypt(ciphertext.ciphertext)?;
        Ok(uid.to_bytes())
    }

    pub fn encrypt_profile_key(
        &self,
        randomness: RandomnessBytes,
        profile_key: api::profiles::ProfileKey,
    ) -> api::groups::ProfileKeyCiphertext {
        self.encrypt_profile_key_bytes(randomness, profile_key.bytes)
    }

    pub fn encrypt_profile_key_bytes(
        &self,
        _randomness: RandomnessBytes,
        profile_key_bytes: ProfileKeyBytes,
    ) -> api::groups::ProfileKeyCiphertext {
        let profile_key = crypto::profile_key_struct::ProfileKeyStruct::new(profile_key_bytes);
        let ciphertext = self.profile_key_enc_key_pair.encrypt(profile_key);
        api::groups::ProfileKeyCiphertext { ciphertext }
    }

    pub fn decrypt_profile_key(
        &self,
        ciphertext: api::groups::ProfileKeyCiphertext,
    ) -> Result<api::profiles::ProfileKey, ZkGroupError> {
        let profile_key_struct = self
            .profile_key_enc_key_pair
            .decrypt(ciphertext.ciphertext)?;
        Ok(api::profiles::ProfileKey {
            bytes: profile_key_struct.bytes,
        })
    }

    pub fn encrypt_blob(
        &self,
        randomness: RandomnessBytes,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, ZkGroupError> {
        let key_vec = ShoSha256::shohash(
            b"Signal_ZKGroup_BlobEnc_KeyDerive",
            &self.master_key.bytes,
            32,
        );
        let nonce_vec = ShoSha256::shohash(b"Signal_ZKGroup_BlobEnc_Nonce", &randomness, 12);
        match self.encrypt_blob_aesgcmsiv(&key_vec, &nonce_vec, plaintext) {
            Ok(mut ciphertext_vec) => {
                ciphertext_vec.extend(nonce_vec);
                Ok(ciphertext_vec)
            }
            Err(e) => Err(e),
        }
    }

    pub fn decrypt_blob(self, ciphertext: &[u8]) -> Result<Vec<u8>, ZkGroupError> {
        let key_vec = ShoSha256::shohash(
            b"Signal_ZKGroup_BlobEnc_KeyDerive",
            &self.master_key.bytes,
            32,
        );
        if ciphertext.len() < 12 {
            // 12 bytes for IV
            return Err(ZkGroupError::DecryptionFailure);
        }
        let nonce = &ciphertext[ciphertext.len() - 12..];
        let ciphertext = &ciphertext[..ciphertext.len() - 12];
        self.decrypt_blob_aesgcmsiv(&key_vec, nonce, ciphertext)
    }

    fn encrypt_blob_aesgcmsiv(
        &self,
        key: &[u8],
        nonce: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, ZkGroupError> {
        let key = GenericArray::from_slice(key);
        let aead_cipher = Aes256GcmSiv::new(*key);
        let nonce = GenericArray::from_slice(nonce);
        match aead_cipher.encrypt(nonce, plaintext) {
            Ok(ciphertext_vec) => Ok(ciphertext_vec),
            Err(_) => Err(ZkGroupError::BadArgs),
        }
    }

    fn decrypt_blob_aesgcmsiv(
        self,
        key: &[u8],
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, ZkGroupError> {
        if ciphertext.len() < 16 {
            // 16 bytes for tag
            return Err(ZkGroupError::DecryptionFailure);
        }
        let key = GenericArray::from_slice(key);
        let aead_cipher = Aes256GcmSiv::new(*key);
        let nonce = GenericArray::from_slice(nonce);
        match aead_cipher.decrypt(nonce, ciphertext) {
            Ok(plaintext_vec) => Ok(plaintext_vec),
            Err(_) => Err(ZkGroupError::DecryptionFailure),
        }
    }
}

impl GroupPublicParams {
    pub fn get_group_identifier(&self) -> GroupIdentifierBytes {
        self.group_id
    }

    pub fn verify_signature(
        &self,
        message: &[u8],
        signature: ChangeSignatureBytes,
    ) -> Result<(), ZkGroupError> {
        self.sig_public_key.verify(message, signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aesgcmsiv_vec1() {
        // https://tools.ietf.org/html/rfc8452#appendix-C

        let group_secret_params = GroupSecretParams::generate([0u8; 32]);

        let plaintext_vec = vec![
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let key_vec = vec![
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let nonce_vec = vec![
            0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let ciphertext_vec = vec![
            0x4a, 0x6a, 0x9d, 0xb4, 0xc8, 0xc6, 0x54, 0x92, 0x01, 0xb9, 0xed, 0xb5, 0x30, 0x06,
            0xcb, 0xa8, 0x21, 0xec, 0x9c, 0xf8, 0x50, 0x94, 0x8a, 0x7c, 0x86, 0xc6, 0x8a, 0xc7,
            0x53, 0x9d, 0x02, 0x7f, 0xe8, 0x19, 0xe6, 0x3a, 0xbc, 0xd0, 0x20, 0xb0, 0x06, 0xa9,
            0x76, 0x39, 0x76, 0x32, 0xeb, 0x5d,
        ];

        let calc_ciphertext = group_secret_params
            .encrypt_blob_aesgcmsiv(&key_vec, &nonce_vec, &plaintext_vec)
            .unwrap();

        assert!(&calc_ciphertext[..ciphertext_vec.len()] == &ciphertext_vec[..]);

        let calc_plaintext = group_secret_params
            .decrypt_blob_aesgcmsiv(&key_vec, &nonce_vec, &calc_ciphertext)
            .unwrap();
        assert!(&calc_plaintext[..] == &plaintext_vec[..]);
    }

    #[test]
    fn test_aesgcmsiv_vec2() {
        // https://tools.ietf.org/html/rfc8452#appendix-C

        let group_secret_params = GroupSecretParams::generate([0u8; 32]);

        let plaintext_vec = vec![
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x4d, 0xb9, 0x23, 0xdc, 0x79, 0x3e, 0xe6, 0x49, 0x7c, 0x76, 0xdc, 0xc0,
            0x3a, 0x98, 0xe1, 0x08,
        ];

        let key_vec = vec![
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let nonce_vec = vec![
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let ciphertext_vec = vec![
            0xf3, 0xf8, 0x0f, 0x2c, 0xf0, 0xcb, 0x2d, 0xd9, 0xc5, 0x98, 0x4f, 0xcd, 0xa9, 0x08,
            0x45, 0x6c, 0xc5, 0x37, 0x70, 0x3b, 0x5b, 0xa7, 0x03, 0x24, 0xa6, 0x79, 0x3a, 0x7b,
            0xf2, 0x18, 0xd3, 0xea, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let calc_ciphertext = group_secret_params
            .encrypt_blob_aesgcmsiv(&key_vec, &nonce_vec, &plaintext_vec)
            .unwrap();

        assert!(&calc_ciphertext[..ciphertext_vec.len()] == &ciphertext_vec[..]);

        let calc_plaintext = group_secret_params
            .decrypt_blob_aesgcmsiv(&key_vec, &nonce_vec, &calc_ciphertext)
            .unwrap();
        assert!(&calc_plaintext[..] == &plaintext_vec[..]);
    }
}
