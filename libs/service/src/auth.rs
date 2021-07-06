mod basic;

use bytestring::ByteString;
use codec::Login;

#[async_trait::async_trait]
pub trait Auth {
    async fn auth(&self, login: &Login) -> Option<ByteString>;
}

pub use basic::BasicAuth;
