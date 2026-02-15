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
    fn username_key(host: &str) -> String {
        format!("{}::username", host)
    }

    fn password_key(host: &str, username: &str) -> String {
        format!("{}::{}::password", host, username)
    }
}

impl CredentialStore for MacKeychainStore {
    fn get(&self, host: &str) -> Result<Option<Credentials>> {
        let username_entry = Entry::new(SERVICE, &Self::username_key(host))
            .context("failed to create keychain entry for username")?;

        let username = match username_entry.get_password() {
            Ok(value) if !value.trim().is_empty() => value,
            Ok(_) => return Ok(None),
            Err(_) => return Ok(None),
        };

        let password_entry = Entry::new(SERVICE, &Self::password_key(host, &username))
            .context("failed to create keychain entry for password")?;

        let password = match password_entry.get_password() {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };

        Ok(Some(Credentials { username, password }))
    }

    fn set(&self, host: &str, creds: &Credentials) -> Result<()> {
        let username_entry = Entry::new(SERVICE, &Self::username_key(host))
            .context("failed to create keychain username entry")?;
        username_entry
            .set_password(&creds.username)
            .context("failed writing username to keychain")?;

        let password_entry = Entry::new(SERVICE, &Self::password_key(host, &creds.username))
            .context("failed to create keychain password entry")?;
        password_entry
            .set_password(&creds.password)
            .context("failed writing password to keychain")?;
        Ok(())
    }

    fn clear(&self, host: &str) -> Result<()> {
        let username_entry = Entry::new(SERVICE, &Self::username_key(host))
            .context("failed to create keychain username entry")?;

        if let Ok(username) = username_entry.get_password() {
            let password_entry = Entry::new(SERVICE, &Self::password_key(host, &username))
                .context("failed to create keychain password entry")?;
            let _ = password_entry.delete_password();
        }

        let _ = username_entry.delete_password();
        Ok(())
    }
}

pub fn prompt_credentials(default_username: Option<&str>) -> Result<Credentials> {
    let mut username_prompt = Input::<String>::new();
    username_prompt = username_prompt.with_prompt("Login username");
    if let Some(default) = default_username {
        if !default.trim().is_empty() {
            username_prompt = username_prompt.default(default.to_string());
        }
    }

    let username = username_prompt.interact_text().context("username input failed")?;

    let password = Password::new()
        .with_prompt("Password")
        .interact()
        .context("password input failed")?;

    Ok(Credentials { username, password })
}
