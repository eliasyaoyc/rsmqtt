#![forbid(unsafe_code)]
#![warn(clippy::default_trait_access)]

pub mod auth;
pub mod storage;

mod client_loop;
mod config;
mod error;
mod filter;
mod message;
mod metrics;
mod state;
mod sys_topics;

pub use client_loop::{client_loop, RemoteAddr};
pub use config::ServiceConfig;
pub use filter::TopicFilter;
pub use state::ServiceState;
pub use sys_topics::sys_topics_update_loop;
