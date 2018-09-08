mod channel;
mod handler;
mod worker;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use actix::{
    self,
    actors::signal,
    fut,
};
use futures::{
    channel::mpsc,
};
use serenity::model::{
    channel::Channel as SerenityChannel,
    prelude::*,
    prelude::Message as SerenityMessage,
};

use crate::{
    AddrExt,
    Bus,
    BusId,
    Config,
    Error,
    fetch_config,
    message::{ChannelUpdated, IrcReady, MessageCreated, Terminate},
    prelude::*,
};

use self::channel::*;
use self::handler::{ClientState, DiscordEvent, new_client};
use self::worker::{DiscordWorker, SendMessage};


pub struct Discord {
    config: Arc<Config>,
    bus_id: BusId,

    channels: HashMap<ChannelId, channel::Channel>,
    members: HashMap<(GuildId, UserId), Member>,

    client_state: Option<ClientState>,
    current_user: Option<CurrentUser>,
    worker: Addr<DiscordWorker>,
}

impl Discord {
    pub fn new() -> Result<Discord, Error> {
        let config = fetch_config();
        let worker = SyncArbiter::start(8, || DiscordWorker::new());

        Ok(Discord {
            config,
            bus_id: Bus::new_id(),
            channels: HashMap::new(),
            members: HashMap::new(),
            client_state: None,
            current_user: None,
            worker,
        })
    }

    fn set_client_state(&mut self, state: ClientState) {
        if let Some(ClientState::Running { shard_manager, .. }) = self.client_state.take() {
            shard_manager.lock().shutdown_all();
        }
        self.client_state = Some(state);
    }

    fn find_channels<'a>(&'a self, channel: &'a str) -> impl Iterator<Item = &'a GuildChannel> + 'a {
        self.channels.values()
            .filter_map(move |ch| match ch.as_ref() {
                ChannelRef::Guild(name, ch) if name == channel => Some(ch),
                _ => None,
            })
    }

    fn find_channel_by_id(&self, id: ChannelId) -> Option<ChannelRef<'_>> {
        if let Some(ch) = self.channels.get(&id) {
            return Some(ch.as_ref());
        }
        None
    }

    fn register_channel(&mut self, channel: Channel) -> Option<ChannelId> {
        match channel {
            SerenityChannel::Guild(ch) => {
                let ch = ch.read();
                if let Some((_, ch)) = self.register_guild_channel(&ch) {
                    Some(ch.id)
                } else {
                    None
                }
            }
            SerenityChannel::Private(ch) => {
                let ch = ch.read();
                let ch = self.channels.entry(ch.id)
                    .or_insert_with(|| channel::Channel::Private(ch.clone()));
                Some(ch.as_ref().id())
            }
            _ => None,
        }
    }

    fn register_guild_channel<'a>(&'a mut self, channel: &GuildChannel) -> Option<(&'a str, &'a GuildChannel)> {
        if channel.kind != ChannelType::Text {
            return None;
        }
        let ch = self.channels.entry(channel.id)
            .or_insert_with(|| channel::Channel::Guild(channel.name.clone(), channel.clone()));
        ch.as_guild()
    }

    fn handle_bot_command(&mut self, msg: SerenityMessage) -> Option<String> {
        let content = msg.content.trim();
        if content.starts_with("ping") {
            return Some("pong".to_owned());
        }
        None
    }
}

impl Actor for Discord {
    type Context = actix::Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        signal::ProcessSignals::from_registry()
            .do_send(signal::Subscribe(ctx.address().recipient()));

        let (tx, rx) = mpsc::channel(128);
        match new_client(&self.config, tx) {
            Ok(state) => {
                ctx.add_message_stream_03(rx);
                self.set_client_state(state);
            }
            Err(e) => {
                error!("Connection failure: {}", e);
                ctx.notify(Terminate);
                return;
            }
        }

        let addr = ctx.address();
        async fn subscribe(addr: &Addr<Discord>) -> Result<(), MailboxError> {
            // await!(addr.subscribe::<ChannelUpdated>())?;
            await!(addr.subscribe::<IrcReady>())?;
            await!(addr.subscribe::<MessageCreated>())?;
            Ok(())
        }
        Arbiter::spawn_async(async move {
            if let Err(err) = await!(subscribe(&addr)) {
                error!("Failed to subscribe: {}", err);
                addr.do_send(Terminate);
            }
        }.boxed());
    }

    fn stopping(&mut self, ctx: &mut Self::Context) -> Running {
        let old_state = self.client_state.take();
        let ret = match old_state {
            Some(ClientState::Running { term_rx, shard_manager, .. }) => {
                shard_manager.lock().shutdown_all();
                ctx.run_later(Duration::from_secs(2), |_, ctx| {
                    ctx.spawn(fut::wrap_future(term_rx.tokio_compat())
                        .then(|res, _, _| {
                            debug!("Discord client thread terminated: {:?}", res);
                            fut::ok(())
                        })
                        .timeout(Duration::from_secs(5), ())
                        .then(|_, actor: &mut Self, ctx: &mut Self::Context| {
                            actor.client_state = Some(ClientState::Stopped);
                            ctx.stop();
                            fut::ok(())
                        })
                    );
                });
                self.client_state = Some(ClientState::Stopping);
                return Running::Continue;
            }
            Some(ClientState::Stopping { .. }) => Running::Continue,
            _ => Running::Stop,
        };
        self.client_state = old_state;
        ret
    }

    fn stopped(&mut self, _: &mut Self::Context) {
        Bus::do_publish(self.bus_id, Terminate);
    }
}

impl_get_bus_id!(Discord);

impl Handler<Terminate> for Discord {
    type Result = ();
    fn handle(&mut self, _: Terminate, ctx: &mut Self::Context) -> Self::Result {
        ctx.terminate();
    }
}

impl Handler<signal::Signal> for Discord {
    type Result = ();

    fn handle(&mut self, msg: signal::Signal, ctx: &mut Self::Context) {
        use self::signal::SignalType::*;
        match msg.0 {
            Int | Term | Quit => { ctx.stop(); }
            _ => { }
        }
    }
}

impl Handler<DiscordEvent> for Discord {
    type Result = ();

    fn handle(&mut self, msg: DiscordEvent, _: &mut Self::Context) {
        debug!("Discord receives DiscordEvent: {:?}", msg);
        use self::DiscordEvent::*;
        match msg {
            Ready { ready } => self.on_ready(ready),
            GuildCreate { guild } => self.on_guild_create(guild),
            GuildMemberAddition { guild_id, member } => self.on_guild_member_addition(guild_id, member),
            GuildMemberRemoval { guild_id, user } => self.on_guild_member_removal(guild_id, user),
            GuildMemberUpdate { event } => self.on_guild_member_update(event),
            Message { msg } => self.on_message(msg).unwrap(),
            _ => {
                info!("Unknown event: {:?}", msg);
            }
        }
    }
}

impl Discord {
    fn on_ready(&mut self, Ready { user, .. }: Ready) {
        self.current_user = Some(user);
    }

    fn on_guild_create(&mut self, guild: Guild) {
        let mut new_channels = vec![];
        for channel in guild.channels.values() {
            let chan = channel.read();
            if let Some((name, _)) = self.register_guild_channel(&chan) {
                new_channels.push(name.to_owned());
            }
        }
        if !new_channels.is_empty() {
            Bus::publish(self.bus_id, ChannelUpdated {
                channels: new_channels,
            });
        }

        for (id, member) in &guild.members {
            self.members.insert((guild.id, *id), member.clone());
        }
    }

    fn on_guild_member_addition(&mut self, guild_id: GuildId, member: Member) {
        let user_id = member.user.read().id;
        self.members.insert((guild_id, user_id), member);
    }

    fn on_guild_member_removal(&mut self, guild_id: GuildId, user: User) {
        self.members.remove(&(guild_id, user.id));
    }

    fn on_guild_member_update(&mut self, event: GuildMemberUpdateEvent) {
        if let Some(member) = self.members.get_mut(&(event.guild_id, event.user.id)) {
            member.nick = event.nick;
            member.roles = event.roles;
        }
    }

    fn on_message(&mut self, msg: SerenityMessage) -> Result<(), Error> {
        if self.current_user.as_ref().map(|u| u.id == msg.author.id).unwrap_or(false) {
            return Ok(());
        }
        let channel = if let Some(ch) = self.find_channel_by_id(msg.channel_id) {
            ch
        } else {
            let ch = msg.channel_id.to_channel()?;
            if let Some(id) = self.register_channel(ch) {
                self.find_channel_by_id(id).expect("unreachable")
            } else {
                return Ok(());
            }
        };
        let channel_id = match channel {
            ChannelRef::Guild(name, channel) => {
                // let nickname = channel.guild_id.member(msg.author.id)?.nick.unwrap_or(msg.author.name);
                let nickname = self.members.get(&(channel.guild_id, msg.author.id))
                    .and_then(|m| m.nick.as_ref())
                    .unwrap_or(&msg.author.name);
                let m = MessageCreated::builder()
                    .nickname(&nickname[..])
                    .channel(format!("#{}", name))
                    .content(msg.content)
                    .build().unwrap();
                Bus::do_publish(self.bus_id, m);
                return Ok(());
            }
            ChannelRef::Private(ch) => ch.id,
        };
        if let Some(content) = self.handle_bot_command(msg) {
            self.worker.do_send(SendMessage {
                channel: channel_id,
                content,
            });
        }
        Ok(())
    }
}

impl Handler<IrcReady> for Discord {
    type Result = ();

    fn handle(&mut self, _: IrcReady, _: &mut Self::Context) {
        let channels = self.channels.values()
            .filter_map(|ch| ch.as_guild().map(|i| i.0.to_owned()))
            .collect();
        Bus::do_publish(self.bus_id, ChannelUpdated { channels });
    }
}

impl Handler<MessageCreated> for Discord {
    type Result = ();

    fn handle(&mut self, msg: MessageCreated, _: &mut Self::Context) {
        if !msg.channel.starts_with('#') {
            return;
        }
        for channel in self.find_channels(&msg.channel[1..]) {
            let m = format!("<{}> {}", msg.nickname, msg.content);
            self.worker.do_send(SendMessage { channel: channel.id, content: m });
        }
    }
}
