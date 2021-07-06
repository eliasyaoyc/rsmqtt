use std::collections::HashMap;

use anyhow::Result;
use bytestring::ByteString;
use codec::Login;
use passwd::HashType;
use serde::Deserialize;
use serde_yaml::Value;

use crate::auth::Auth;

#[derive(Debug, Deserialize)]
struct Config {
    hash: HashType,
    user_file: String,
}

#[derive(Debug, Deserialize)]
pub struct BasicAuth {
    #[serde(default = "default_hash")]
    hash: HashType,
    users: HashMap<String, String>,
}

fn default_hash() -> HashType {
    HashType::Pbkdf2Sha512
}

impl BasicAuth {
    pub fn try_new(value: &Value) -> Result<Self> {
        let config: Config = serde_yaml::from_value(value.clone())?;
        let users: HashMap<String, String> =
            serde_yaml::from_reader(std::fs::File::open(&config.user_file)?)?;
        Ok(Self {
            hash: config.hash,
            users,
        })
    }
}

#[async_trait::async_trait]
impl Auth for BasicAuth {
    async fn auth(&self, login: &Login) -> Option<ByteString> {
        match self.users.get(&*login.username) {
            Some(phc) if self.hash.verify_password(&phc, &login.password) => {
                Some(login.username.clone())
            }
            _ => None,
        }
    }
}
