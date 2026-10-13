//! The private page: the full local snapshot, encrypted here with a key that
//! only exists on this machine and in the link's #fragment.

use crate::account::{Account, random, site};
use crate::store::Res;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chrono::Utc;
use serde_json::json;

pub fn url(account: &Account) -> String {
    format!(
        "{}/me#{}.{}",
        site(),
        account.id,
        URL_SAFE_NO_PAD.encode(account.link_key)
    )
}

pub fn encrypt(key: &[u8; 32], plain: &[u8]) -> Res<([u8; 12], Vec<u8>)> {
    let iv: [u8; 12] = random()?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "bad key length")?;
    let data = cipher
        .encrypt(&Nonce::from(iv), plain)
        .map_err(|_| "encryption failed")?;
    Ok((iv, data))
}

pub fn upload(account: &Account) -> Res<()> {
    let plain = crate::report::snapshot(Utc::now()).to_string();
    let (iv, data) = encrypt(&account.link_key, plain.as_bytes())?;
    let body = json!({"v": 1, "iv": STANDARD.encode(iv), "data": STANDARD.encode(data)});
    account.request("PUT", "/api/v1/private", Some(&body))?;
    Ok(())
}

pub fn run(args: &[String]) -> Res<()> {
    if args.iter().any(|a| a == "--dry-run") {
        println!(
            "{}",
            serde_json::to_string_pretty(&crate::report::snapshot(Utc::now()))?
        );
        return Ok(());
    }
    let mut account = Account::load().ok_or("not logged in; run `ai-usage login` first")?;
    let rotate = args.iter().any(|a| a == "--rotate");
    if rotate {
        account.link_key = random()?;
        account.save()?;
    }
    upload(&account)?;
    if rotate {
        println!("New link made; the old one no longer opens.");
    }
    println!("{}", url(&account));
    println!("Anyone with this link can see your private page. `ai-usage link --rotate` replaces it.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypts_with_the_same_key_only() {
        let key = [7u8; 32];
        let (iv, data) = encrypt(&key, b"{\"v\":1}").unwrap();
        let open = |k: &[u8; 32]| {
            Aes256Gcm::new_from_slice(k)
                .unwrap()
                .decrypt(&Nonce::from(iv), data.as_slice())
        };
        assert_eq!(open(&key).unwrap(), b"{\"v\":1}");
        assert!(open(&[8u8; 32]).is_err());
    }
}
