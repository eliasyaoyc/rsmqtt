use std::collections::HashMap;

use anyhow::Result;
use serde::Deserialize;
use serde_yaml::Value;

use service::{Plugin, PluginFactory};

#[derive(Debug, Deserialize)]
struct Config {
    users: HashMap<String, String>,
}

pub struct BasicAuth;

#[async_trait::async_trait]
impl PluginFactory for BasicAuth {
    fn name(&self) -> &'static str {
        "basic-auth"
    }

    async fn create(&self, config: Value) -> Result<Box<dyn Plugin>> {
        let config: Config = serde_yaml::from_value(config)?;
        Ok(Box::new(BasicAuthImpl {
            users: config.users,
        }))
    }
}

struct BasicAuthImpl {
    users: HashMap<String, String>,
}

#[async_trait::async_trait]
impl Plugin for BasicAuthImpl {
    async fn auth(&self, user: &str, password: &str) -> Result<Option<String>> {
        match self.users.get(user) {
            Some(phc) if passwd_util::verify_password(&phc, password) => Ok(Some(user.to_string())),
            _ => Ok(None),
        }
    }
}
