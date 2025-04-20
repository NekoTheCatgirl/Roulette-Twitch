mod websocket;

use std::sync::Arc;

use clap::Parser;
use eyre::Context;
use rand::Rng;
use tokio::sync::Mutex;
use twitch_api::{
    client::ClientDefault,
    eventsub::{self, Event, Message, Payload},
    helix::{self, Scope},
    twitch_oauth2::{self, TwitchToken, UserToken},
    HelixClient,
};
use websocket::ChatWebsocketClient;

const ID: &str = include_str!("../secret/id");
// const SECRET: &str = include_str!("../secret/secret");

#[derive(Parser, Debug, Clone)]
#[clap(about, version)]
pub struct Cli {
    /// Client ID of twitch application
    // #[clap(long, env, hide_env = true)]
    // pub client_id: twitch_oauth2::ClientId,
    #[clap(long, env, hide_env = true)]
    pub broadcaster_login: twitch_api::types::UserName,
}

#[tokio::main]
async fn main() -> Result<(), eyre::Report> {
    color_eyre::install()?;
    tracing_subscriber::fmt::fmt()
        .with_writer(std::io::stderr)
        .init();

    let opts = Cli::parse();

    let client: HelixClient<reqwest::Client> = twitch_api::HelixClient::with_client(
        ClientDefault::default_client_with_name(Some("Roulette Bot".parse()?))?,
    );

    let mut builder = twitch_oauth2::tokens::DeviceUserTokenBuilder::new(
        ID,
        vec![
            Scope::UserReadChat,
            Scope::UserWriteChat,
            Scope::ChannelModerate,
        ],
    );
    let code = builder.start(&client).await?;
    open::that(&code.verification_uri)?;
    let token = builder.wait_for_code(&client, tokio::time::sleep).await?;

    let Some(helix::users::User {
        id: broadcaster, ..
    }) = client
        .get_user_from_login(&opts.broadcaster_login, &token)
        .await?
    else {
        eyre::bail!(
            "No broadcaster found with login: {}",
            opts.broadcaster_login
        );
    };

    let token = Arc::new(Mutex::new(token));

    let bot = Bot {
        opts,
        client,
        token,
        broadcaster,
    };
    bot.start().await?;

    Ok(())
}

pub struct Bot {
    pub opts: Cli,
    pub client: HelixClient<'static, reqwest::Client>,
    pub token: Arc<Mutex<twitch_oauth2::UserToken>>,
    pub broadcaster: twitch_api::types::UserId,
}

impl Bot {
    pub async fn start(&self) -> Result<(), eyre::Report> {
        let websocket = ChatWebsocketClient {
            session_id: None,
            token: self.token.clone(),
            client: self.client.clone(),
            connect_url: twitch_api::TWITCH_EVENTSUB_WEBSOCKET_URL.clone(),
            chats: vec![self.broadcaster.clone()],
        };

        let refresh_token = async move {
            let token = self.token.clone();
            let client = self.client.clone();

            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                let mut token = token.lock().await;
                if token.expires_in() < std::time::Duration::from_secs(60) {
                    token
                        .refresh_token(&self.client)
                        .await
                        .wrap_err("Couldn't refresh token")?;
                }
                token
                    .validate_token(&client)
                    .await
                    .wrap_err("couldn't validate token")?;
            }
            #[allow(unreachable_code)]
            Ok(())
        };
        let ws = websocket.run(|e, ts| async { self.handle_event(e, ts).await });
        futures::future::try_join(ws, refresh_token).await?;
        Ok(())
    }

    async fn handle_event(
        &self,
        event: Event,
        timestamp: twitch_api::types::Timestamp,
    ) -> Result<(), eyre::Report> {
        let token = self.token.lock().await;
        match event {
            Event::ChannelChatMessageV1(Payload {
                message: Message::Notification(payload),
                subscription,
                ..
            }) => {
                println!(
                    "[{}] {}: {}",
                    timestamp, payload.chatter_user_name, payload.message.text
                );
                if let Some(command) = payload.message.text.strip_prefix("?!") {
                    let mut split_whitespace = command.split_whitespace();
                    let command = split_whitespace.next().unwrap();
                    let rest = split_whitespace.next();

                    self.command(&payload, &subscription, command, rest, &token)
                        .await?;
                }
            }
            Event::ChannelChatNotificationV1(Payload {
                message: Message::Notification(payload),
                ..
            }) => {
                println!(
                    "[{}] {}: {}",
                    timestamp,
                    match &payload.chatter {
                        eventsub::channel::chat::notification::Chatter::Chatter {
                            chatter_user_name: user,
                            ..
                        } => user.as_str(),
                        _ => "anonymous",
                    },
                    payload.message.text
                );
            }
            _ => {}
        }
        Ok(())
    }

    async fn command(
        &self,
        payload: &eventsub::channel::ChannelChatMessageV1Payload,
        subscription: &eventsub::EventSubscriptionInformation<
            eventsub::channel::ChannelChatMessageV1,
        >,
        command: &str,
        _rest: Option<&str>,
        token: &UserToken,
    ) -> Result<(), eyre::Report> {
        tracing::info!("Command: {}", command);
        match command {
            "roulette" => {
                // Spin the roulette wheel.
                let num = rand::rng().random_range(1..=6);
                if num == 6 {
                    self.client
                        .send_chat_message_reply(
                            &subscription.condition.broadcaster_user_id,
                            &subscription.condition.user_id,
                            &payload.message_id,
                            format!(
                                "{} took a chance with the revolver, and it went bang! Bye bye {}",
                                payload.chatter_user_name.as_str(),
                                payload.chatter_user_name.as_str()
                            )
                            .as_str(),
                            token,
                        )
                        .await?;
                    self.client
                        .ban_user(
                            &payload.chatter_user_id,
                            "Bro got shot!",
                            Some(180),
                            &subscription.condition.broadcaster_user_id,
                            &subscription.condition.user_id,
                            token,
                        )
                        .await?;
                } else {
                    self.client
                        .send_chat_message_reply(
                            &subscription.condition.broadcaster_user_id,
                            &subscription.condition.user_id,
                            &payload.message_id,
                            format!("{} took a chance with the revolver, it clicks, and {} is spared to chat another day!", payload.chatter_user_name.as_str(), payload.chatter_user_name.as_str()).as_str(),
                            token,
                        )
                        .await?;
                }
            }
            _ => {}
        };
        Ok(())
    }
}
