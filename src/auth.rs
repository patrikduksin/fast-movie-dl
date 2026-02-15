use anyhow::{Context, Result};
use dialoguer::{Input, Password};
use keyring::Entry;

const SERVICE: &str = "fast-movie-dl";

#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

pub trait CredentialStore {
    fn get(&self, host: &str) -> Result<Option<Credentials>>;
    fn set(&self, host: &str, creds: &Credentials) -> Result<()>;
    fn clear(&self, host: &str) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct MacKeychainStore;

impl MacKeychainStore {
    fn credentials_key(host: &str) -> String {
        format!("{}::credentials", host)
    }

    fn legacy_username_key(host: &str) -> String {
        format!("{}::username", host)
    }

    fn legacy_password_key(host: &str, username: &str) -> String {
        format!("{}::{}::password", host, username)
    }

    fn read_legacy_credentials(&self, host: &str) -> Result<Option<Credentials>> {
        let username_entry = Entry::new(SERVICE, &Self::legacy_username_key(host))
            .context("failed to create keychain entry for username")?;

        let username = match username_entry.get_password() {
            Ok(value) if !value.trim().is_empty() => value,
            Ok(_) => return Ok(None),
            Err(_) => return Ok(None),
        };

        let password_entry = Entry::new(SERVICE, &Self::legacy_password_key(host, &username))
            .context("failed to create keychain entry for password")?;

        let password = match password_entry.get_password() {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };

        Ok(Some(Credentials { username, password }))
    }

    fn clear_legacy_credentials(&self, host: &str) -> Result<()> {
        let username_entry = Entry::new(SERVICE, &Self::legacy_username_key(host))
            .context("failed to create keychain username entry")?;

        if let Ok(username) = username_entry.get_password() {
            let password_entry = Entry::new(SERVICE, &Self::legacy_password_key(host, &username))
                .context("failed to create keychain password entry")?;
            let _ = password_entry.delete_password();
        }

        let _ = username_entry.delete_password();
        Ok(())
    }
}

impl CredentialStore for MacKeychainStore {
    fn get(&self, host: &str) -> Result<Option<Credentials>> {
        let credentials_entry = Entry::new(SERVICE, &Self::credentials_key(host))
            .context("failed to create keychain credentials entry")?;

        match credentials_entry.get_password() {
            Ok(raw) => {
                if let Some(creds) = decode_credentials_blob(&raw) {
                    return Ok(Some(creds));
                }
            }
            Err(_) => {}
        }

        self.read_legacy_credentials(host)
    }

    fn set(&self, host: &str, creds: &Credentials) -> Result<()> {
        let credentials_entry = Entry::new(SERVICE, &Self::credentials_key(host))
            .context("failed to create keychain credentials entry")?;
        credentials_entry
            .set_password(&encode_credentials_blob(creds))
            .context("failed writing credentials to keychain")?;

        let _ = self.clear_legacy_credentials(host);
        Ok(())
    }

    fn clear(&self, host: &str) -> Result<()> {
        let credentials_entry = Entry::new(SERVICE, &Self::credentials_key(host))
            .context("failed to create keychain credentials entry")?;
        let _ = credentials_entry.delete_password();
        let _ = self.clear_legacy_credentials(host);
        Ok(())
    }
}

fn encode_credentials_blob(creds: &Credentials) -> String {
    format!("{}\n{}", creds.username, creds.password)
}

fn decode_credentials_blob(raw: &str) -> Option<Credentials> {
    let (username, password) = raw.split_once('\n')?;
    if username.trim().is_empty() {
        return None;
    }

    Some(Credentials {
        username: username.to_string(),
        password: password.to_string(),
    })
}

pub fn prompt_credentials(default_username: Option<&str>) -> Result<Credentials> {
    let mut username_prompt = Input::<String>::new();
    username_prompt = username_prompt.with_prompt("Login username");
    if let Some(default) = default_username {
        if !default.trim().is_empty() {
            username_prompt = username_prompt.default(default.to_string());
        }
    }

    let username = username_prompt
        .interact_text()
        .context("username input failed")?;

    let password = Password::new()
        .with_prompt("Password")
        .interact()
        .context("password input failed")?;

    Ok(Credentials { username, password })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes_credentials_blob() {
        let creds = Credentials {
            username: "paolo1989".to_string(),
            password: "s3cr3t!".to_string(),
        };

        let raw = encode_credentials_blob(&creds);
        let decoded = decode_credentials_blob(&raw).expect("expected valid decoded credentials");

        assert_eq!(decoded.username, creds.username);
        assert_eq!(decoded.password, creds.password);
    }

    #[test]
    fn rejects_blob_without_separator() {
        let decoded = decode_credentials_blob("invalid-format");
        assert!(decoded.is_none());
    }
}
