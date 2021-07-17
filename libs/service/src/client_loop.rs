use std::borrow::Cow;
use std::fmt::{self, Display, Formatter};
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use bytestring::ByteString;
use codec::{
    Codec, ConnAck, ConnAckProperties, Connect, ConnectReasonCode, DecodeError, Disconnect,
    DisconnectProperties, DisconnectReasonCode, EncodeError, LastWill, Packet, PacketIdAllocator,
    ProtocolLevel, PubAck, PubAckProperties, PubAckReasonCode, PubComp, PubCompProperties,
    PubCompReasonCode, PubRec, PubRecProperties, PubRecReasonCode, PubRel, PubRelProperties,
    PubRelReasonCode, Publish, Qos, SubAck, SubAckProperties, Subscribe, SubscribeReasonCode,
    UnsubAck, UnsubAckProperties, UnsubAckReasonCode, Unsubscribe,
};
use fnv::FnvHashMap;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Notify};

use crate::error::Error;
use crate::filter_util;
use crate::message::Message;
use crate::plugin::Action;
use crate::state::Control;
use crate::ServiceState;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum Qos2State {
    Published,
    Recorded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAddr {
    pub protocol: Cow<'static, str>,
    pub addr: Option<ByteString>,
}

impl Display for RemoteAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}://{}",
            self.protocol,
            self.addr.as_deref().unwrap_or("unknown")
        )
    }
}

pub struct Connection<R, W> {
    state: Arc<ServiceState>,
    remote_addr: RemoteAddr,
    client_id: Option<ByteString>,
    control_sender: mpsc::UnboundedSender<Control>,
    uid: Option<ByteString>,
    notify: Arc<Notify>,
    codec: Codec<R, W>,
    session_expiry_interval: u32,
    receive_in_max: usize,
    receive_out_max: usize,
    receive_in_quota: usize,
    receive_out_quota: usize,
    max_topic_alias: usize,
    topic_alias: FnvHashMap<NonZeroU16, ByteString>,
    keep_alive: u16,
    last_active: Instant,
    last_will: Option<LastWill>,
    packet_id_allocator: PacketIdAllocator,
    inflight_qos2_messages: FnvHashMap<NonZeroU16, Qos2State>,
    uncompleted_messages: FnvHashMap<NonZeroU16, Message>,
}

impl<R, W> Connection<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin,
{
    async fn send_packet(&mut self, packet: &Packet) -> Result<(), Error> {
        tracing::debug!(
            remote_addr = %self.remote_addr,
            packet = ?packet,
            "send packet",
        );
        match self.codec.encode(packet).await {
            Ok(packet_size) => {
                self.state.service_metrics.inc_msgs_sent(1);
                self.state.service_metrics.inc_bytes_sent(packet_size);
                if let Packet::Publish(publish) = packet {
                    self.state
                        .service_metrics
                        .inc_pub_bytes_sent(publish.payload.len());
                }
                Ok(())
            }
            Err(EncodeError::PayloadTooLarge) => Err(Error::server_disconnect(
                DisconnectReasonCode::PacketTooLarge,
            )),
            Err(err) => Err(err.into()),
        }
    }

    async fn send_disconnect(
        &mut self,
        reason_code: DisconnectReasonCode,
        properties: Option<DisconnectProperties>,
    ) -> Result<(), Error> {
        self.send_packet(&Packet::Disconnect(Disconnect {
            reason_code,
            properties: properties.unwrap_or_default(),
        }))
        .await
    }

    async fn check_acl(&self, action: Action, topic: &str) -> Result<(), Error> {
        let mut allow = true;

        for (name, plugin) in &self.state.plugins {
            match plugin
                .check_acl(&self.remote_addr, self.uid.as_deref(), action, topic)
                .await
            {
                Ok(false) => {
                    allow = false;
                    break;
                }
                Ok(true) => {}
                Err(err) => {
                    tracing::error!(
                        plugin = %name,
                        error = %err,
                        "failed to call plugin::check_acl",
                    );
                    return Err(Error::server_disconnect(
                        DisconnectReasonCode::UnspecifiedError,
                    ));
                }
            }
        }

        if !allow {
            return Err(Error::server_disconnect(
                DisconnectReasonCode::NotAuthorized,
            ));
        }

        Ok(())
    }

    async fn handle_packet(&mut self, packet: Packet) -> Result<(), Error> {
        match packet {
            Packet::Connect(connect) => self.handle_connect(connect).await,
            Packet::Publish(publish) => self.handle_publish(publish).await,
            Packet::PubAck(pub_ack) => self.handle_pub_ack(pub_ack).await,
            Packet::PubRec(pub_rec) => self.handle_pub_rec(pub_rec).await,
            Packet::PubRel(pub_rel) => self.handle_pub_rel(pub_rel).await,
            Packet::PubComp(pub_comp) => self.handle_pub_comp(pub_comp).await,
            Packet::Subscribe(subscribe) => self.handle_subscribe(subscribe).await,
            Packet::Unsubscribe(unsubscribe) => self.handle_unsubscribe(unsubscribe).await,
            Packet::PingReq => self.handle_ping_req().await,
            Packet::Disconnect(disconnect) => self.handle_disconnect(disconnect).await,
            Packet::SubAck(_) | Packet::ConnAck(_) | Packet::UnsubAck(_) | Packet::PingResp => Err(
                Error::server_disconnect(DisconnectReasonCode::ProtocolError),
            ),
        }
    }

    async fn handle_connect(&mut self, mut connect: Connect) -> Result<(), Error> {
        let mut conn_ack_properties = ConnAckProperties::default();

        if self.client_id.is_some() {
            return Err(Error::server_disconnect(
                DisconnectReasonCode::ProtocolError,
            ));
        }

        let mut session_expiry_interval = {
            match connect.properties.session_expiry_interval {
                Some(session_expiry_interval)
                    if session_expiry_interval > self.state.config.max_session_expiry_interval =>
                {
                    conn_ack_properties.session_expiry_interval =
                        Some(self.state.config.max_session_expiry_interval);
                    self.state.config.max_session_expiry_interval
                }
                Some(session_expiry_interval) => session_expiry_interval,
                None => {
                    // If the Session Expiry Interval is absent the value 0 is used.
                    0
                }
            }
        };

        let keep_alive = {
            if connect.keep_alive > self.state.config.max_keep_alive {
                conn_ack_properties.server_keep_alive = Some(self.state.config.max_keep_alive);
                self.state.config.max_keep_alive
            } else {
                connect.keep_alive
            }
        };

        let receive_in_max = self.state.config.receive_max as usize;
        let receive_out_max = connect
            .properties
            .receive_max
            .map(|x| x as usize)
            .unwrap_or(usize::MAX);

        if self.state.config.maximum_qos != Qos::ExactlyOnce {
            conn_ack_properties.maximum_qos = Some(self.state.config.maximum_qos);
        }

        let max_packet_size_out = connect.properties.max_packet_size.unwrap_or(u32::MAX);
        let max_packet_size_in = self.state.config.max_packet_size;
        if max_packet_size_in != u32::MAX {
            conn_ack_properties.max_packet_size = Some(max_packet_size_in);
        }

        if !self.state.config.retain_available {
            conn_ack_properties.retain_available = Some(false);
        }

        if !self.state.config.wildcard_subscription_available {
            conn_ack_properties.wildcard_subscription_available = Some(false);
        }

        let max_topic_alias = {
            match connect.properties.topic_alias_max {
                Some(topic_alias_max) if topic_alias_max > self.state.config.max_topic_alias => {
                    conn_ack_properties.topic_alias_max = Some(self.state.config.max_topic_alias);
                    self.state.config.max_topic_alias
                }
                Some(topic_alias_max) => topic_alias_max,
                None => {
                    conn_ack_properties.topic_alias_max = Some(self.state.config.max_topic_alias);
                    self.state.config.max_topic_alias
                }
            }
        };

        if let Some(last_will) = &connect.last_will {
            if last_will.qos > self.state.config.maximum_qos {
                self.send_packet(&Packet::ConnAck(ConnAck {
                    session_present: false,
                    reason_code: ConnectReasonCode::QoSNotSupported,
                    properties: ConnAckProperties::default(),
                }))
                .await?;
                return Ok(());
            }

            if last_will.retain && !self.state.config.retain_available {
                self.send_packet(&Packet::ConnAck(ConnAck {
                    session_present: false,
                    reason_code: ConnectReasonCode::RetainNotSupported,
                    properties: ConnAckProperties::default(),
                }))
                .await?;
                return Ok(());
            }

            if last_will
                .properties
                .payload_format_indicator
                .unwrap_or_default()
                && std::str::from_utf8(&last_will.payload).is_err()
            {
                self.send_packet(&Packet::ConnAck(ConnAck {
                    session_present: false,
                    reason_code: ConnectReasonCode::PayloadFormatInvalid,
                    properties: ConnAckProperties::default(),
                }))
                .await?;
                return Ok(());
            }
        }

        if connect.client_id.is_empty() {
            // If the Server rejects the ClientID it MAY respond to the CONNECT packet with a CONNACK
            // using Reason Code 0x85 (Client Identifier not valid) as described in section 4.13 Handling
            // errors, and then it MUST close the Network Connection [MQTT-3.1.3-8].
            if !connect.clean_start {
                self.send_packet(&Packet::ConnAck(ConnAck {
                    session_present: false,
                    reason_code: ConnectReasonCode::ClientIdentifierNotValid,
                    properties: ConnAckProperties::default(),
                }))
                .await?;
                return Err(Error::ServerDisconnect(None));
            }

            connect.client_id = format!("auto-{}", uuid::Uuid::new_v4()).into();
            conn_ack_properties.assigned_client_identifier = Some(connect.client_id.clone());
        }

        // auth
        let mut uid = None;
        if let Some(login) = &connect.login {
            for (name, plugin) in &self.state.plugins {
                match plugin.auth(&login.username, &login.password).await {
                    Ok(Some(res_uid)) => {
                        uid = Some(res_uid.into());
                        break;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::error!(
                            plugin = %name,
                            error = %err,
                            "failed to call plugin::auth",
                        );
                        return Err(Error::internal_error(err));
                    }
                }
            }

            if uid.is_none() {
                return Err(Error::server_disconnect(
                    DisconnectReasonCode::NotAuthorized,
                ));
            }
        }

        if connect.level == ProtocolLevel::V4 && !connect.clean_start {
            connect.properties.session_expiry_interval =
                Some(self.state.config.max_session_expiry_interval);
            session_expiry_interval = self.state.config.max_session_expiry_interval;
        }

        {
            let mut connections = self.state.connections.write().await;
            if let Some(control_sender) = connections.remove(&*connect.client_id) {
                control_sender.send(Control::SessionTakenOver).ok();
            }
            connections.insert(connect.client_id.to_string(), self.control_sender.clone());
        }

        // create session
        let (session_present, notify) = self.state.storage.create_session(
            &connect.client_id,
            connect.clean_start,
            connect.last_will.clone(),
        );

        self.uid = uid;
        self.notify = notify;
        self.client_id = Some(connect.client_id.clone());
        self.keep_alive = keep_alive;
        self.receive_in_max = receive_in_max;
        self.receive_out_max = receive_out_max;
        self.receive_in_quota = receive_in_max;
        self.receive_out_quota = receive_out_max;
        self.max_topic_alias = max_topic_alias as usize;
        self.session_expiry_interval = session_expiry_interval;
        self.last_will = connect.last_will.clone();

        self.codec.set_output_max_size(max_packet_size_out as usize);
        self.codec.set_input_max_size(max_packet_size_in as usize);

        self.send_packet(&Packet::ConnAck(ConnAck {
            session_present,
            reason_code: ConnectReasonCode::Success,
            properties: conn_ack_properties,
        }))
        .await?;
        self.state.service_metrics.inc_connection_count(1);

        for (_, plugin) in &self.state.plugins {
            plugin
                .on_client_connected(
                    &self.remote_addr,
                    self.client_id.as_ref().unwrap(),
                    self.uid.as_deref(),
                    self.keep_alive,
                    connect.level,
                )
                .await;
        }

        if session_present {
            // retry send inflight publish
            let packets = self
                .state
                .storage
                .get_all_inflight_pub_packets(&connect.client_id);
            for mut publish in packets {
                publish.dup = true;
                self.receive_out_quota -= 1;
                self.send_packet(&Packet::Publish(publish)).await?;
            }
        } else {
            for s in &self.state.config.subscriptions {
                let filter = match filter_util::parse_filter(&s.path) {
                    Some(filter) => filter,
                    None => {
                        tracing::warn!(
                            filter = %s.path,
                            "failed to parse proxy subscription filter",
                        );
                        continue;
                    }
                };
                self.state.storage.subscribe(
                    &connect.client_id,
                    filter,
                    s.qos,
                    s.no_local,
                    s.retain_as_published,
                    s.retain_handling,
                    None,
                );
            }
        }

        Ok(())
    }

    async fn handle_publish(&mut self, mut publish: Publish) -> Result<(), Error> {
        let client_id = match self.client_id.clone() {
            Some(client_id) => client_id,
            None => {
                return Err(Error::server_disconnect(
                    DisconnectReasonCode::ProtocolError,
                ));
            }
        };

        self.state
            .service_metrics
            .inc_pub_bytes_received(publish.payload.len());
        self.state.service_metrics.inc_pub_msgs_received(1);

        if matches!(publish.properties.topic_alias, Some(client) if client.get() > self.state.config.max_topic_alias)
        {
            // A Topic Alias value of 0 or greater than the Maximum Topic Alias is a Protocol Error, the
            // receiver uses DISCONNECT with Reason Code of 0x94 (Topic Alias invalid) as described in section 4.13.
            return Err(Error::server_disconnect(
                DisconnectReasonCode::TopicAliasInvalid,
            ));
        }

        if publish.topic.is_empty() && publish.properties.topic_alias.is_none() {
            // It is a Protocol Error if the Topic Name is zero length and there is no Topic Alias.
            return Err(Error::server_disconnect(
                DisconnectReasonCode::ProtocolError,
            ));
        }

        if publish.qos > Qos::AtMostOnce && publish.packet_id.is_none() {
            return Err(Error::server_disconnect(
                DisconnectReasonCode::ProtocolError,
            ));
        }

        if !publish.properties.subscription_identifiers.is_empty() {
            return Err(Error::server_disconnect(
                DisconnectReasonCode::ProtocolError,
            ));
        }

        if publish.topic.starts_with('$') {
            return Err(Error::server_disconnect(
                DisconnectReasonCode::TopicNameInvalid,
            ));
        }

        if !publish.topic.is_empty() && !filter_util::valid_topic(&publish.topic) {
            return Err(Error::server_disconnect(
                DisconnectReasonCode::TopicNameInvalid,
            ));
        }

        if publish.retain && !self.state.config.retain_available {
            // If the Server included Retain Available in its CONNACK response to a Client
            // with its value set to 0 and it receives a PUBLISH packet with the RETAIN flag is
            // set to 1, then it uses the DISCONNECT Reason Code of 0x9A (Retain not supported) as
            // described in section 4.13.
            return Err(Error::server_disconnect(
                DisconnectReasonCode::RetainNotSupported,
            ));
        }

        if publish
            .properties
            .payload_format_indicator
            .unwrap_or_default()
            && std::str::from_utf8(&publish.payload).is_err()
        {
            return Err(Error::server_disconnect(
                DisconnectReasonCode::PayloadFormatInvalid,
            ));
        }

        publish.topic = match publish.properties.topic_alias {
            Some(topic_alias) if !publish.topic.is_empty() => {
                self.topic_alias.insert(topic_alias, publish.topic.clone());
                publish.topic
            }
            Some(topic_alias) => {
                if let Some(topic) = self.topic_alias.get(&topic_alias) {
                    topic.clone()
                } else {
                    return Err(Error::server_disconnect(
                        DisconnectReasonCode::ProtocolError,
                    ));
                }
            }
            None if !publish.topic.is_empty() => publish.topic.clone(),
            None => {
                return Err(Error::server_disconnect(
                    DisconnectReasonCode::ProtocolError,
                ));
            }
        };

        let retain = publish.retain;
        let packet_id = publish.packet_id;

        // check acl
        self.check_acl(Action::Publish, &publish.topic).await?;

        // rewrite
        self.state.rewrite(&mut publish.topic);

        // create message
        let mut msg = Message::from_publish(&publish).with_from_client_id(client_id.clone());
        if let Some(uid) = &self.uid {
            msg = msg.with_from_uid(uid.clone());
        }

        if retain {
            // update retained message
            self.state.storage.update_retained_message(msg.clone());
        }

        for (_, plugin) in &self.state.plugins {
            plugin
                .on_message_publish(
                    self.client_id.as_ref().unwrap(),
                    self.uid.as_deref(),
                    msg.topic(),
                    msg.qos(),
                    msg.is_retain(),
                    msg.payload().clone(),
                )
                .await;
        }

        // do publish
        match msg.qos() {
            Qos::AtMostOnce => {
                self.state.storage.deliver(std::iter::once(msg));
            }
            Qos::AtLeastOnce => {
                self.state.storage.deliver(std::iter::once(msg));
                self.send_packet(&Packet::PubAck(PubAck {
                    packet_id: packet_id.unwrap(),
                    reason_code: PubAckReasonCode::Success,
                    properties: PubAckProperties::default(),
                }))
                .await?;
            }
            Qos::ExactlyOnce => {
                if self.receive_in_quota == 0 {
                    self.state.service_metrics.inc_msg_dropped(1);
                    return Err(Error::server_disconnect(
                        DisconnectReasonCode::ReceiveMaximumExceeded,
                    ));
                }

                let packet_id = packet_id.unwrap();

                if self
                    .uncompleted_messages
                    .insert(packet_id, msg.clone())
                    .is_some()
                {
                    return if self.codec.protocol_level() == ProtocolLevel::V5 {
                        self.send_packet(&Packet::PubRec(PubRec {
                            packet_id,
                            reason_code: PubRecReasonCode::PacketIdentifierInUse,
                            properties: PubRecProperties::default(),
                        }))
                        .await?;
                        Ok(())
                    } else {
                        Err(Error::server_disconnect(
                            DisconnectReasonCode::ProtocolError,
                        ))
                    };
                }

                self.receive_in_quota -= 1;
                self.send_packet(&Packet::PubRec(PubRec {
                    packet_id,
                    reason_code: PubRecReasonCode::Success,
                    properties: PubRecProperties::default(),
                }))
                .await?;
            }
        }

        Ok(())
    }

    async fn handle_pub_ack(&mut self, pub_ack: PubAck) -> Result<(), Error> {
        let client_id = match &self.client_id {
            Some(client_id) => client_id,
            None => {
                return Err(Error::server_disconnect(
                    DisconnectReasonCode::ProtocolError,
                ))
            }
        };

        tracing::debug!(
            remote_addr = %self.remote_addr,
            client_id = %client_id,
            packet_id = pub_ack.packet_id,
            "remove inflight packet",
        );

        match self
            .state
            .storage
            .get_inflight_pub_packets(client_id, pub_ack.packet_id, true)
        {
            Some(_) => {
                self.receive_out_quota += 1;
                Ok(())
            }
            None => Err(Error::server_disconnect(
                DisconnectReasonCode::ProtocolError,
            )),
        }
    }

    async fn handle_pub_rec(&mut self, pub_rec: PubRec) -> Result<(), Error> {
        let client_id = match &self.client_id {
            Some(client_id) => client_id,
            None => {
                return Err(Error::server_disconnect(
                    DisconnectReasonCode::ProtocolError,
                ))
            }
        };

        match self.inflight_qos2_messages.get_mut(&pub_rec.packet_id) {
            Some(state) if *state != Qos2State::Published => {
                return Err(Error::server_disconnect(
                    DisconnectReasonCode::ProtocolError,
                ));
            }
            Some(state) => *state = Qos2State::Recorded,
            None => {
                return Err(Error::server_disconnect(
                    DisconnectReasonCode::ProtocolError,
                ));
            }
        }

        if !pub_rec.reason_code.is_success() {
            if self
                .state
                .storage
                .get_inflight_pub_packets(client_id, pub_rec.packet_id, true)
                .is_none()
            {
                return Err(Error::server_disconnect(
                    DisconnectReasonCode::ProtocolError,
                ));
            }
            return Ok(());
        }

        match self
            .state
            .storage
            .get_inflight_pub_packets(client_id, pub_rec.packet_id, false)
        {
            Some(_) => {
                self.send_packet(&Packet::PubRel(PubRel {
                    packet_id: pub_rec.packet_id,
                    reason_code: PubRelReasonCode::Success,
                    properties: PubRelProperties::default(),
                }))
                .await?;
                Ok(())
            }
            None => {
                if self.codec.protocol_level() == ProtocolLevel::V5 {
                    self.send_packet(&Packet::PubRel(PubRel {
                        packet_id: pub_rec.packet_id,
                        reason_code: PubRelReasonCode::PacketIdentifierNotFound,
                        properties: PubRelProperties::default(),
                    }))
                    .await
                } else {
                    Err(Error::server_disconnect(
                        DisconnectReasonCode::ProtocolError,
                    ))
                }
            }
        }
    }

    async fn handle_pub_rel(&mut self, pub_rel: PubRel) -> Result<(), Error> {
        if self.client_id.is_none() {
            return Err(Error::server_disconnect(
                DisconnectReasonCode::ProtocolError,
            ));
        }

        match self.uncompleted_messages.remove(&pub_rel.packet_id) {
            Some(msg) => {
                if !pub_rel.reason_code.is_success() {
                    return Ok(());
                }

                self.state.storage.deliver(std::iter::once(msg));
                self.send_packet(&Packet::PubComp(PubComp {
                    packet_id: pub_rel.packet_id,
                    reason_code: PubCompReasonCode::Success,
                    properties: PubCompProperties::default(),
                }))
                .await?;
                self.receive_in_quota += 1;
            }
            None => {
                if self.codec.protocol_level() == ProtocolLevel::V5 {
                    self.send_packet(&Packet::PubComp(PubComp {
                        packet_id: pub_rel.packet_id,
                        reason_code: PubCompReasonCode::PacketIdentifierNotFound,
                        properties: PubCompProperties::default(),
                    }))
                    .await?;
                } else {
                    return Err(Error::server_disconnect(
                        DisconnectReasonCode::ProtocolError,
                    ));
                }
            }
        }

        Ok(())
    }

    async fn handle_pub_comp(&mut self, pub_comp: PubComp) -> Result<(), Error> {
        let client_id = match &self.client_id {
            Some(client_id) => client_id,
            None => {
                return Err(Error::server_disconnect(
                    DisconnectReasonCode::ProtocolError,
                ))
            }
        };

        if !matches!(
            self.inflight_qos2_messages.remove(&pub_comp.packet_id),
            Some(Qos2State::Recorded)
        ) {
            return Err(Error::server_disconnect(
                DisconnectReasonCode::ProtocolError,
            ));
        }

        match self
            .state
            .storage
            .get_inflight_pub_packets(client_id, pub_comp.packet_id, true)
        {
            Some(_) => {
                tracing::debug!(
                    remote_addr = %self.remote_addr,
                    client_id = %client_id,
                    packet_id = pub_comp.packet_id,
                    "remove inflight packet",
                );
                self.receive_out_quota += 1;
                self.handle_notified().await?;
            }
            None => {
                tracing::debug!(
                    remote_addr = %self.remote_addr,
                    client_id = %client_id,
                    packet_id = pub_comp.packet_id,
                    "inflight packet not found",
                );
            }
        }

        Ok(())
    }

    async fn handle_subscribe(&mut self, subscribe: Subscribe) -> Result<(), Error> {
        let client_id = match self.client_id.clone() {
            Some(client_id) => client_id,
            None => {
                return Err(Error::server_disconnect(
                    DisconnectReasonCode::ProtocolError,
                ))
            }
        };

        let mut reason_codes = Vec::with_capacity(subscribe.filters.len());

        for s in &subscribe.filters {
            let filter = match filter_util::parse_filter(&s.path) {
                Some(filter) => filter,
                None => {
                    reason_codes.push(SubscribeReasonCode::TopicFilterInvalid);
                    continue;
                }
            };

            if filter.share_name.is_some() && s.no_local {
                // It is a Protocol Error to set the No Local bit to 1 on a Shared Subscription [MQTT-3.8.3-4].
                return Err(Error::server_disconnect(
                    DisconnectReasonCode::ProtocolError,
                ));
            }

            if !self.state.config.wildcard_subscription_available
                && filter_util::has_wildcards(filter.path)
            {
                reason_codes.push(SubscribeReasonCode::WildcardSubscriptionsNotSupported);
                continue;
            }

            // check acl
            self.check_acl(Action::Subscribe, &filter.path).await?;

            let qos = s.qos.min(self.state.config.maximum_qos);

            for (_, plugin) in &self.state.plugins {
                plugin
                    .on_session_subscribed(
                        self.client_id.as_ref().unwrap(),
                        self.uid.as_deref(),
                        &s.path,
                        qos,
                    )
                    .await;
            }

            reason_codes.push(match qos {
                Qos::AtMostOnce => SubscribeReasonCode::QoS0,
                Qos::AtLeastOnce => SubscribeReasonCode::QoS1,
                Qos::ExactlyOnce => SubscribeReasonCode::QoS2,
            });

            self.state.storage.subscribe(
                &client_id,
                filter,
                s.qos,
                s.no_local,
                s.retain_as_published,
                s.retain_handling,
                subscribe.properties.id,
            );
        }

        self.send_packet(&Packet::SubAck(SubAck {
            packet_id: subscribe.packet_id,
            reason_codes,
            properties: SubAckProperties::default(),
        }))
        .await?;

        Ok(())
    }

    async fn handle_unsubscribe(&mut self, unsubscribe: Unsubscribe) -> Result<(), Error> {
        let client_id = match &self.client_id {
            Some(client_id) => client_id,
            None => {
                return Err(Error::server_disconnect(
                    DisconnectReasonCode::ProtocolError,
                ))
            }
        };
        let mut reason_codes = Vec::new();

        for path in unsubscribe.filters {
            let filter = match filter_util::parse_filter(&*path) {
                Some(filter) => filter,
                None => {
                    reason_codes.push(UnsubAckReasonCode::TopicFilterInvalid);
                    continue;
                }
            };

            for (_, plugin) in &self.state.plugins {
                plugin
                    .on_session_unsubscribed(
                        self.client_id.as_ref().unwrap(),
                        self.uid.as_deref(),
                        &path,
                    )
                    .await;
            }

            match self.state.storage.unsubscribe(client_id, filter) {
                true => reason_codes.push(UnsubAckReasonCode::Success),
                false => reason_codes.push(UnsubAckReasonCode::NoSubscriptionExisted),
            }
        }

        self.send_packet(&Packet::UnsubAck(UnsubAck {
            packet_id: unsubscribe.packet_id,
            reason_codes,
            properties: UnsubAckProperties::default(),
        }))
        .await?;
        Ok(())
    }

    async fn handle_ping_req(&mut self) -> Result<(), Error> {
        self.send_packet(&Packet::PingResp).await
    }

    async fn handle_disconnect(&mut self, disconnect: Disconnect) -> Result<(), Error> {
        tracing::debug!(
            remote_addr = %self.remote_addr,
            reason_code = ?disconnect.reason_code,
            "client disconnect"
        );
        if disconnect.reason_code == DisconnectReasonCode::NormalDisconnection {
            self.last_will = None;
        }
        if let Some(session_expiry_interval) = disconnect.properties.session_expiry_interval {
            self.session_expiry_interval = session_expiry_interval;
        }
        Err(Error::ClientDisconnect(disconnect))
    }

    async fn handle_control(&mut self, control: Control) -> Result<(), Error> {
        match control {
            Control::SessionTakenOver => {
                self.client_id = None;
                self.state.service_metrics.dec_connection_count(1);
                Err(Error::SessionTakenOver)
            }
        }
    }

    async fn handle_notified(&mut self) -> Result<(), Error> {
        if let Some(client_id) = self.client_id.clone() {
            if self.receive_out_quota == 0 {
                return Ok(());
            }

            let msgs = self
                .state
                .storage
                .next_messages(&client_id, Some(self.receive_out_quota));
            assert!(msgs.len() <= self.receive_out_quota);

            for msg in msgs {
                if msg.is_expired() {
                    continue;
                }
                self.delive(msg).await?;
            }
        }

        Ok(())
    }

    async fn delive(&mut self, msg: Message) -> Result<(), Error> {
        let client_id = match self.client_id.clone() {
            Some(client_id) => client_id,
            None => return Ok(()),
        };

        let mut publish = match msg.to_publish_and_update_expiry_interval() {
            Some(publish) => publish,
            None => return Ok(()),
        };

        for (_, plugin) in &self.state.plugins {
            plugin
                .on_message_delivered(
                    self.client_id.as_ref().unwrap(),
                    self.uid.as_deref(),
                    msg.from_client_id().map(|s| &**s),
                    msg.from_uid().map(|s| &**s),
                    msg.topic(),
                    msg.qos(),
                    msg.is_retain(),
                    msg.payload().clone(),
                )
                .await;
        }

        self.state.service_metrics.inc_pub_msgs_sent(1);
        match publish.qos {
            Qos::AtMostOnce => self.send_packet(&Packet::Publish(publish)).await,
            Qos::AtLeastOnce | Qos::ExactlyOnce => {
                let packet_id = self.packet_id_allocator.take();
                publish.packet_id = Some(packet_id);

                if publish.qos > Qos::AtMostOnce {
                    self.receive_out_quota -= 1;
                }

                tracing::debug!(
                    remote_addr = %self.remote_addr,
                    client_id = %client_id,
                    packet_id = packet_id,
                    "add inflight packet",
                );
                self.state
                    .storage
                    .add_inflight_pub_packet(&client_id, publish.clone());
                self.inflight_qos2_messages
                    .insert(packet_id, Qos2State::Published);
                self.send_packet(&Packet::Publish(publish)).await?;
                Ok(())
            }
        }
    }
}

pub async fn client_loop(
    state: Arc<ServiceState>,
    reader: impl AsyncRead + Send + Unpin,
    writer: impl AsyncWrite + Send + Unpin,
    remote_addr: RemoteAddr,
) {
    state.service_metrics.inc_socket_connections(1);

    let (control_sender, mut control_receiver) = mpsc::unbounded_channel();
    let mut connection = Connection {
        state: state.clone(),
        remote_addr,
        client_id: None,
        control_sender,
        uid: None,
        notify: Arc::new(Notify::new()),
        codec: Codec::new(reader, writer),
        session_expiry_interval: 0,
        receive_in_max: 0,
        receive_out_max: 0,
        receive_in_quota: 0,
        receive_out_quota: 0,
        max_topic_alias: 0,
        topic_alias: FnvHashMap::default(),
        keep_alive: 60,
        last_active: Instant::now(),
        last_will: None,
        packet_id_allocator: PacketIdAllocator::default(),
        inflight_qos2_messages: FnvHashMap::default(),
        uncompleted_messages: FnvHashMap::default(),
    };
    let mut keep_alive_interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = keep_alive_interval.tick() => {
                if connection.keep_alive > 0 &&
                    connection.last_active.elapsed().as_secs() > connection.keep_alive as u64 * 3 / 2 {
                    tracing::debug!(
                        remote_addr = %connection.remote_addr,
                        "keep alive timeout",
                    );
                    connection.send_disconnect(DisconnectReasonCode::KeepAliveTimeout, None).await.ok();
                    break;
                }
            }
            res = connection.codec.decode() => {
                match res {
                    Ok(Some((packet, packet_size))) => {
                        connection.state.service_metrics.inc_bytes_received(packet_size);
                        connection.state.service_metrics.inc_msgs_received(1);
                        connection.last_active = Instant::now();
                        tracing::debug!(
                            remote_addr = %connection.remote_addr,
                            packet = ?packet,
                            "receive packet",
                        );
                        match connection.handle_packet(packet).await {
                            Ok(_) => {}
                            Err(Error::InternalError(_)) => {
                                connection.send_disconnect(DisconnectReasonCode::UnspecifiedError, None).await.ok();
                                break;
                            }
                            Err(Error::ServerDisconnect(disconnect)) => {
                                if let Some(disconnect) = disconnect {
                                    tracing::debug!(
                                        remote_addr = %connection.remote_addr,
                                        reason_code = ?disconnect.reason_code,
                                        "server disconnect",
                                    );
                                    connection.send_packet(&Packet::Disconnect(disconnect)).await.ok();
                                } else {
                                    tracing::debug!(
                                        remote_addr = %connection.remote_addr,
                                        "server disconnect",
                                    );
                                }
                                break;
                            }
                            Err(Error::ClientDisconnect { .. }) => break,
                            Err(err) => {
                                tracing::debug!(
                                    remote_addr = %connection.remote_addr,
                                    error = %err,
                                    "error",
                                );
                                break;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(DecodeError::PacketTooLarge) => {
                        connection.send_disconnect(
                            DisconnectReasonCode::PacketTooLarge,
                            None,
                        ).await.ok();
                        break;
                    }
                    Err(err) => {
                        tracing::debug!(
                            remote_addr = %connection.remote_addr,
                            error = %err,
                            "decode packet",
                        );
                        break;
                    }
                }
            }
            item = control_receiver.recv() => {
                if let Some(control) = item {
                    match connection.handle_control(control).await {
                        Ok(()) => {}
                        Err(Error::SessionTakenOver) => {
                            connection.send_disconnect(
                                DisconnectReasonCode::SessionTakenOver,
                                None,
                            ).await.ok();
                            break;
                        },
                        Err(err) => {
                            tracing::debug!(
                                remote_addr = %connection.remote_addr,
                                error = %err,
                                "error",
                            );
                            break;
                        }
                    }
                }
            }
            _ = connection.notify.notified() => {
                if let Err(err) = connection.handle_notified().await {
                    tracing::debug!(
                        remote_addr = %connection.remote_addr,
                        error = %err,
                        "error",
                    );
                    break;
                }
            }
        }
    }

    if let Some(client_id) = &connection.client_id {
        connection
            .state
            .connections
            .write()
            .await
            .remove(&**client_id);
        connection.state.service_metrics.dec_connection_count(1);
        connection
            .state
            .storage
            .disconnect_session(&client_id, connection.session_expiry_interval);

        for (_, plugin) in &connection.state.plugins {
            plugin
                .on_client_disconnected(client_id, connection.uid.as_deref())
                .await;
        }
    }

    state.service_metrics.dec_socket_connections(1);
}
