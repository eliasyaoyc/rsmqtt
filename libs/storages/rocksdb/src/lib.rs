#![forbid(unsafe_code)]
#![warn(clippy::default_trait_access)]

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroU16;
use std::ops::Deref;
use std::sync::Arc;

use anyhow::Result;
use bytestring::ByteString;
use codec::{LastWill, Publish, Qos, RetainHandling, SubscribeFilter};
use fnv::FnvHashMap;
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use rocksdb::{Options, DB};
use serde::Deserialize;
use serde_yaml::Value;
use service::{Message, SessionInfo, Storage, StorageMetrics, TopicFilter};
use tokio::sync::Notify;

macro_rules! session_not_found {
    ($client_id:expr) => {
        anyhow::bail!("session '{}' not found", $client_id)
    };
}

#[derive(Debug, Deserialize)]
struct Config {
    path: String,
}

#[derive(Clone)]
struct Filter {
    subscribe_filter: SubscribeFilter,
    topic_filter: TopicFilter,
    id: Option<usize>,
}

impl Deref for Filter {
    type Target = SubscribeFilter;

    fn deref(&self) -> &Self::Target {
        &self.subscribe_filter
    }
}

struct Session {
    notify: Arc<Notify>,
    subscription_filters: HashMap<ByteString, Filter>,
    last_will: Option<LastWill>,
    session_expiry_interval: u32,
    last_will_expiry_interval: u32,
    inflight_pub_packets: VecDeque<Publish>,
    uncompleted_messages: FnvHashMap<NonZeroU16, Message>,
}

struct RocksdbStorageInner {
    db: DB,
    retain_messages: HashMap<ByteString, Message>,
    sessions: HashMap<ByteString, RwLock<Session>>,

    /// All of the share subscriptions
    ///
    /// share name -> client id -> path -> filter
    share_subscriptions: HashMap<String, HashMap<String, HashMap<ByteString, Filter>>>,
}

pub struct RocksdbStorage {
    inner: RwLock<RocksdbStorageInner>,
}

impl RocksdbStorage {
    pub fn create(config: Value) -> Result<Self> {
        let config: Config = serde_yaml::from_value(config)?;
        let mut options = Options::default();
        options.create_if_missing(true);

        let db = DB::open(&options, &config.path)?;
        let retain_messages = Self::load_retain_messages(&db)?;

        Ok(Self {
            inner: RwLock::new(RocksdbStorageInner {
                db,
                retain_messages,
                sessions: HashMap::new(),
                share_subscriptions: HashMap::new(),
            }),
        })
    }

    fn load_retain_messages(db: &DB) -> Result<HashMap<ByteString, Message>> {
        let mut retain_messages = HashMap::new();

        for (key, value) in db.prefix_iterator(format!("RM/")) {
            if let Some(topic) = key.strip_prefix(b"RM/") {
                retain_messages.insert(
                    std::str::from_utf8(topic)?.into(),
                    bincode::deserialize(&value)?,
                );
            }
        }

        Ok(retain_messages)
    }
}

#[async_trait::async_trait]
impl Storage for RocksdbStorage {
    async fn update_retained_message(&self, topic: ByteString, msg: Message) -> Result<()> {
        let mut inner = self.inner.write();
        let key = format!("RM/{}", topic);
        if msg.is_empty() {
            inner.db.delete(key)?;
            inner.retain_messages.remove(&topic);
        } else {
            inner.db.put(key, bincode::serialize(&msg)?);
            inner.retain_messages.insert(topic, msg);
        }
        Ok(())
    }

    async fn create_session(
        &self,
        client_id: ByteString,
        clean_start: bool,
        last_will: Option<LastWill>,
        session_expiry_interval: u32,
        last_will_expiry_interval: u32,
    ) -> Result<(bool, Arc<Notify>)> {
        todo!()
    }

    async fn remove_session(&self, client_id: &str) -> Result<bool> {
        todo!()
    }

    async fn get_sessions(&self) -> Result<Vec<SessionInfo>> {
        todo!()
    }

    async fn subscribe(
        &self,
        client_id: &str,
        subscribe_filter: SubscribeFilter,
        topic_filter: TopicFilter,
        id: Option<usize>,
    ) -> Result<()> {
        todo!()
    }

    async fn unsubscribe(
        &self,
        client_id: &str,
        path: &str,
        topic_filter: TopicFilter,
    ) -> Result<bool> {
        todo!()
    }

    async fn next_messages(&self, client_id: &str, limit: Option<usize>) -> Result<Vec<Message>> {
        todo!()
    }

    async fn consume_messages(&self, client_id: &str, count: usize) -> Result<()> {
        todo!()
    }

    async fn publish(&self, msgs: Vec<Message>) -> Result<()> {
        todo!()
    }

    async fn add_inflight_pub_packet(&self, client_id: &str, publish: Publish) -> Result<()> {
        todo!()
    }

    async fn get_inflight_pub_packets(
        &self,
        client_id: &str,
        packet_id: NonZeroU16,
        remove: bool,
    ) -> Result<Option<Publish>> {
        todo!()
    }

    async fn get_all_inflight_pub_packets(&self, client_id: &str) -> Result<Vec<Publish>> {
        todo!()
    }

    async fn add_uncompleted_message(
        &self,
        client_id: &str,
        packet_id: NonZeroU16,
        msg: Message,
    ) -> Result<bool> {
        todo!()
    }

    async fn get_uncompleted_message(
        &self,
        client_id: &str,
        packet_id: NonZeroU16,
        remove: bool,
    ) -> Result<Option<Message>> {
        todo!()
    }

    async fn metrics(&self) -> Result<StorageMetrics> {
        todo!()
    }
}
