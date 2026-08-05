//! Claude Desktop's Electron `safeStorage` codec.
//!
//! OAuth caches and authentication cookies are decrypted before YAAT stores
//! them. On restore they are encrypted with the target profile's OS-backed
//! codec so Claude Desktop can read them normally.

use std::path::Path;

#[cfg(any(target_os = "macos", test))]
use aes::Aes128;
#[cfg(any(windows, test))]
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
#[cfg(any(target_os = "macos", test))]
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
#[cfg(any(target_os = "macos", test))]
use pbkdf2::pbkdf2_hmac;
#[cfg(any(target_os = "macos", test))]
use sha1::Sha1;
#[cfg(windows)]
use zeroize::Zeroize;
use zeroize::Zeroizing;

#[cfg(any(target_os = "macos", windows, test))]
const VERSION_PREFIX: &[u8] = b"v10";
#[cfg(any(target_os = "macos", windows, test))]
const SAFE_STORAGE_VERSION: &str = "v10";
#[cfg(target_os = "macos")]
const MAC_KEYCHAIN_SERVICE: &str = "Claude Safe Storage";
#[cfg(target_os = "macos")]
const MAC_KEYCHAIN_ACCOUNT: &str = "Claude Key";
#[cfg(any(target_os = "macos", test))]
const MAC_SALT: &[u8] = b"saltysalt";
#[cfg(any(target_os = "macos", test))]
const MAC_ITERATIONS: u32 = 1003;
#[cfg(any(target_os = "macos", test))]
const MAC_IV: [u8; 16] = [b' '; 16];
#[cfg(any(windows, test))]
const WINDOWS_KEY_BYTES: usize = 32;
#[cfg(any(windows, test))]
const WINDOWS_NONCE_BYTES: usize = 12;
#[cfg(any(windows, test))]
const WINDOWS_TAG_BYTES: usize = 16;
#[cfg(windows)]
const LOCAL_STATE_FILE: &str = "Local State";
#[cfg(windows)]
const DPAPI_KEY_PREFIX: &[u8] = b"DPAPI";

#[cfg(any(target_os = "macos", test))]
type Aes128CbcDecryptor = cbc::Decryptor<Aes128>;
#[cfg(any(target_os = "macos", test))]
type Aes128CbcEncryptor = cbc::Encryptor<Aes128>;

pub(super) fn decrypt_base64(root: &Path, value: &str) -> Result<Zeroizing<String>, String> {
    let ciphertext = Zeroizing::new(
        BASE64
            .decode(value)
            .map_err(|_| "Claude Desktop OAuth cache is not valid Base64".to_string())?,
    );
    let plaintext = decrypt_bytes(root, &ciphertext)?;
    let plaintext = String::from_utf8(plaintext.to_vec())
        .map_err(|_| "Claude Desktop OAuth cache is not valid UTF-8".to_string())?;
    let parsed: serde_json::Value = serde_json::from_str(&plaintext)
        .map_err(|_| "Claude Desktop OAuth cache is not valid JSON".to_string())?;
    if !parsed.is_object() {
        return Err("Claude Desktop OAuth cache must be a JSON object".into());
    }
    Ok(Zeroizing::new(plaintext))
}

pub(super) fn encrypt_base64(root: &Path, value: &str) -> Result<Zeroizing<String>, String> {
    let parsed: serde_json::Value = serde_json::from_str(value)
        .map_err(|_| "saved Claude Desktop OAuth cache is not valid JSON".to_string())?;
    if !parsed.is_object() {
        return Err("saved Claude Desktop OAuth cache must be a JSON object".into());
    }
    let ciphertext = encrypt_bytes(root, value.as_bytes())?;
    Ok(Zeroizing::new(BASE64.encode(&ciphertext)))
}

pub(super) fn decrypt_bytes(root: &Path, ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
    system_decrypt(root, ciphertext).map(Zeroizing::new)
}

pub(super) fn encrypt_bytes(root: &Path, plaintext: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
    system_encrypt(root, plaintext).map(Zeroizing::new)
}

#[cfg(target_os = "macos")]
fn system_decrypt(_root: &Path, ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let password = mac_password()?;
    mac_decrypt(&password, ciphertext)
}

#[cfg(target_os = "macos")]
fn system_encrypt(_root: &Path, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let password = mac_password()?;
    mac_encrypt(&password, plaintext)
}

#[cfg(target_os = "macos")]
fn mac_password() -> Result<Zeroizing<Vec<u8>>, String> {
    let entry = keyring::Entry::new(MAC_KEYCHAIN_SERVICE, MAC_KEYCHAIN_ACCOUNT)
        .map_err(|error| format!("Claude Desktop Safe Storage is unavailable: {error}"))?;
    entry
        .get_secret()
        .map(Zeroizing::new)
        .map_err(|error| format!("Claude Desktop Safe Storage key is unavailable: {error}"))
}

#[cfg(any(target_os = "macos", test))]
fn mac_key(password: &[u8]) -> Zeroizing<[u8; 16]> {
    let mut key = Zeroizing::new([0_u8; 16]);
    pbkdf2_hmac::<Sha1>(password, MAC_SALT, MAC_ITERATIONS, key.as_mut());
    key
}

#[cfg(any(target_os = "macos", test))]
fn mac_decrypt(password: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    reject_unknown_version(ciphertext)?;
    let encrypted = ciphertext
        .strip_prefix(VERSION_PREFIX)
        .ok_or_else(|| "Claude Desktop Safe Storage payload has an unknown version".to_string())?;
    if encrypted.is_empty() || encrypted.len() % 16 != 0 {
        return Err("Claude Desktop Safe Storage payload has an invalid length".into());
    }
    let key = mac_key(password);
    let mut buffer = Zeroizing::new(encrypted.to_vec());
    let plaintext = Aes128CbcDecryptor::new((&*key).into(), (&MAC_IV).into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map_err(|_| "Claude Desktop Safe Storage decryption failed".to_string())?;
    Ok(plaintext.to_vec())
}

#[cfg(any(target_os = "macos", test))]
fn mac_encrypt(password: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let key = mac_key(password);
    let block_count = plaintext
        .len()
        .checked_div(16)
        .and_then(|blocks| blocks.checked_add(1))
        .ok_or_else(|| "Claude Desktop OAuth cache is too large".to_string())?;
    let padded_length = block_count
        .checked_mul(16)
        .ok_or_else(|| "Claude Desktop OAuth cache is too large".to_string())?;
    let mut buffer = Zeroizing::new(vec![0_u8; padded_length]);
    buffer[..plaintext.len()].copy_from_slice(plaintext);
    let encrypted = Aes128CbcEncryptor::new((&*key).into(), (&MAC_IV).into())
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
        .map_err(|_| "Claude Desktop Safe Storage encryption failed".to_string())?;
    let mut output = Vec::with_capacity(VERSION_PREFIX.len() + encrypted.len());
    output.extend_from_slice(VERSION_PREFIX);
    output.extend_from_slice(encrypted);
    Ok(output)
}

#[cfg(windows)]
fn system_decrypt(root: &Path, ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    if ciphertext.starts_with(VERSION_PREFIX) {
        let key = windows_key(root, false)?;
        return windows_decrypt(&key, ciphertext);
    }
    reject_unknown_version(ciphertext)?;
    dpapi(ciphertext, false)
}

#[cfg(windows)]
fn system_encrypt(root: &Path, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let key = windows_key(root, true)?;
    windows_encrypt(&key, plaintext)
}

#[cfg(windows)]
fn windows_key(root: &Path, create: bool) -> Result<Zeroizing<[u8; WINDOWS_KEY_BYTES]>, String> {
    use std::fs;

    use serde_json::Value;

    use crate::activation::{ConfigFormat, OwnedPath, PatchEngine, PatchOperation};

    let path = root.join(LOCAL_STATE_FILE);
    if path.exists() {
        let bytes = fs::read(&path)
            .map_err(|error| format!("failed to read Claude Desktop Local State: {error}"))?;
        let state: Value = serde_json::from_slice(&bytes)
            .map_err(|_| "Claude Desktop Local State is not valid JSON".to_string())?;
        if let Some(value) = state.pointer("/os_crypt/encrypted_key") {
            let encoded = value.as_str().ok_or_else(|| {
                "Claude Desktop Local State encryption key is malformed".to_string()
            })?;
            return decode_windows_key(encoded);
        }
    }
    if !create {
        return Err("Claude Desktop Local State encryption key is unavailable".into());
    }

    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let mut key = Zeroizing::new([0_u8; WINDOWS_KEY_BYTES]);
    getrandom::fill(key.as_mut())
        .map_err(|error| format!("failed to generate Claude Desktop encryption key: {error}"))?;
    let wrapped = Zeroizing::new(dpapi(key.as_ref(), true)?);
    let mut encoded_key =
        Zeroizing::new(Vec::with_capacity(DPAPI_KEY_PREFIX.len() + wrapped.len()));
    encoded_key.extend_from_slice(DPAPI_KEY_PREFIX);
    encoded_key.extend_from_slice(&wrapped);
    let operation = PatchOperation::set(
        OwnedPath::from_segments(["os_crypt", "encrypted_key"])
            .map_err(|error| error.to_string())?,
        Value::String(BASE64.encode(&encoded_key)),
    );
    let prepared = PatchEngine::prepare_file(path, ConfigFormat::Json, vec![operation])
        .map_err(|error| error.to_string())?;
    PatchEngine::commit(prepared).map_err(|error| error.to_string())?;
    Ok(key)
}

#[cfg(windows)]
fn decode_windows_key(encoded: &str) -> Result<Zeroizing<[u8; WINDOWS_KEY_BYTES]>, String> {
    let wrapped =
        Zeroizing::new(BASE64.decode(encoded).map_err(|_| {
            "Claude Desktop Local State encryption key is invalid Base64".to_string()
        })?);
    let wrapped = wrapped.strip_prefix(DPAPI_KEY_PREFIX).ok_or_else(|| {
        "Claude Desktop Local State encryption key has an unknown format".to_string()
    })?;
    let unwrapped = Zeroizing::new(dpapi(wrapped, false)?);
    let key: [u8; WINDOWS_KEY_BYTES] = unwrapped.as_slice().try_into().map_err(|_| {
        "Claude Desktop Local State encryption key has an invalid length".to_string()
    })?;
    Ok(Zeroizing::new(key))
}

#[cfg(any(windows, test))]
fn windows_decrypt(key: &[u8; WINDOWS_KEY_BYTES], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let encrypted = ciphertext
        .strip_prefix(VERSION_PREFIX)
        .ok_or_else(|| "Claude Desktop Safe Storage payload has an unknown version".to_string())?;
    if encrypted.len() < WINDOWS_NONCE_BYTES + WINDOWS_TAG_BYTES {
        return Err("Claude Desktop Safe Storage payload has an invalid length".into());
    }
    let (nonce, encrypted) = encrypted.split_at(WINDOWS_NONCE_BYTES);
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| "Claude Desktop Safe Storage key has an invalid length".to_string())?;
    cipher
        .decrypt(Nonce::from_slice(nonce), encrypted)
        .map_err(|_| "Claude Desktop Safe Storage decryption failed".to_string())
}

#[cfg(any(target_os = "macos", windows, test))]
fn reject_unknown_version(ciphertext: &[u8]) -> Result<(), String> {
    if ciphertext.starts_with(VERSION_PREFIX) {
        return Ok(());
    }
    if ciphertext.first() != Some(&b'v') {
        return Ok(());
    }
    let detected = ciphertext
        .get(..3)
        .and_then(|value| std::str::from_utf8(value).ok())
        .unwrap_or("unknown");
    Err(format!(
        "Claude Desktop Safe Storage uses {detected}, but this YAAT version supports {SAFE_STORAGE_VERSION}; update YAAT before importing or switching this account"
    ))
}

#[cfg(windows)]
fn windows_encrypt(key: &[u8; WINDOWS_KEY_BYTES], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let mut nonce = [0_u8; WINDOWS_NONCE_BYTES];
    getrandom::fill(&mut nonce)
        .map_err(|error| format!("failed to generate Claude Desktop encryption nonce: {error}"))?;
    windows_encrypt_with_nonce(key, &nonce, plaintext)
}

#[cfg(any(windows, test))]
fn windows_encrypt_with_nonce(
    key: &[u8; WINDOWS_KEY_BYTES],
    nonce: &[u8; WINDOWS_NONCE_BYTES],
    plaintext: &[u8],
) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| "Claude Desktop Safe Storage key has an invalid length".to_string())?;
    let encrypted = cipher
        .encrypt(Nonce::from_slice(nonce), plaintext)
        .map_err(|_| "Claude Desktop Safe Storage encryption failed".to_string())?;
    let mut output = Vec::with_capacity(VERSION_PREFIX.len() + nonce.len() + encrypted.len());
    output.extend_from_slice(VERSION_PREFIX);
    output.extend_from_slice(nonce);
    output.extend_from_slice(&encrypted);
    Ok(output)
}

#[cfg(windows)]
fn dpapi(input: &[u8], encrypt: bool) -> Result<Vec<u8>, String> {
    use std::ptr;
    use std::slice;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    let input_len = u32::try_from(input.len())
        .map_err(|_| "Claude Desktop Safe Storage payload is too large".to_string())?;
    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: input_len,
        pbData: input.as_ptr().cast_mut(),
    };
    let mut output_blob = CRYPT_INTEGER_BLOB::default();
    // SAFETY: The input slice stays alive for the call, the output descriptor
    // is initialized, and all optional pointers are null as allowed by DPAPI.
    let success = unsafe {
        if encrypt {
            CryptProtectData(
                &input_blob,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output_blob,
            )
        } else {
            CryptUnprotectData(
                &input_blob,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output_blob,
            )
        }
    };
    if success == 0 {
        return Err(format!(
            "Claude Desktop Safe Storage {} failed: {}",
            if encrypt { "encryption" } else { "decryption" },
            std::io::Error::last_os_error()
        ));
    }
    if output_blob.pbData.is_null() {
        return Err("Claude Desktop Safe Storage returned an empty buffer".into());
    }
    let output_len = usize::try_from(output_blob.cbData)
        .map_err(|_| "Claude Desktop Safe Storage returned an invalid length".to_string())?;
    // SAFETY: A successful DPAPI call returns `cbData` readable bytes owned by
    // LocalAlloc. They are copied before the allocation is released.
    let mut output = unsafe { slice::from_raw_parts(output_blob.pbData, output_len).to_vec() };
    // SAFETY: `pbData` was allocated by DPAPI and has not been freed yet.
    unsafe {
        LocalFree(output_blob.pbData.cast());
    }
    if output.is_empty() && !input.is_empty() {
        output.zeroize();
        return Err("Claude Desktop Safe Storage returned empty output".into());
    }
    Ok(output)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn system_decrypt(_root: &Path, _ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    Err("Claude Desktop credential import is supported on macOS and Windows only".into())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn system_encrypt(_root: &Path, _plaintext: &[u8]) -> Result<Vec<u8>, String> {
    Err("Claude Desktop credential import is supported on macOS and Windows only".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_codec_round_trips_electron_v10_payloads() {
        let password = b"test-safe-storage-password";
        let plaintext = br#"{"account":{"token":"secret"}}"#;
        let ciphertext = mac_encrypt(password, plaintext).unwrap();
        assert_eq!(&ciphertext[..3], VERSION_PREFIX);
        assert_ne!(&ciphertext[3..], plaintext);
        assert_eq!(mac_decrypt(password, &ciphertext).unwrap(), plaintext);
    }

    #[test]
    fn mac_codec_rejects_unknown_versions_and_bad_padding() {
        let error = mac_decrypt(b"password", b"v11payload").unwrap_err();
        assert!(error.contains("uses v11"));
        assert!(error.contains("supports v10"));
        assert!(mac_decrypt(b"password", b"v10short").is_err());
    }

    #[test]
    fn windows_codec_round_trips_electron_v10_payloads() {
        let key = [7_u8; WINDOWS_KEY_BYTES];
        let nonce = [9_u8; WINDOWS_NONCE_BYTES];
        let plaintext = br#"{"account":{"token":"secret"}}"#;
        let ciphertext = windows_encrypt_with_nonce(&key, &nonce, plaintext).unwrap();
        assert_eq!(&ciphertext[..3], VERSION_PREFIX);
        assert_eq!(&ciphertext[3..15], &nonce);
        assert_ne!(&ciphertext[15..], plaintext);
        assert_eq!(windows_decrypt(&key, &ciphertext).unwrap(), plaintext);
    }

    #[test]
    fn windows_codec_rejects_tampered_payloads() {
        let key = [7_u8; WINDOWS_KEY_BYTES];
        let nonce = [9_u8; WINDOWS_NONCE_BYTES];
        let mut ciphertext = windows_encrypt_with_nonce(&key, &nonce, b"secret").unwrap();
        *ciphertext.last_mut().unwrap() ^= 1;
        assert!(windows_decrypt(&key, &ciphertext).is_err());
        assert!(windows_decrypt(&key, b"v10short").is_err());
    }
}
