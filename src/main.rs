use anyhow::Context;
use config::{Config, Environment, File, FileFormat};
use foundation::constants::{OPENAI_API_BASE_URL, X_API_KEY_HEADER};
use foundation::hash;
use foundation::http::get_http_client_with_headers;
use foundation::types::openai::{
    ChatMessage, OpenAIChatCompletionRequest, OpenAIChatCompletionResponse,
};
use futures::lock::Mutex;
use futures::FutureExt;
use http::HeaderMap;
use lazy_static::lazy_static;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use redis::AsyncCommands;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use std::{error::Error, sync::Arc};
use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::{Event, Shard, ShardId};
use twilight_http::Client as DiscordHttpClient;
use twilight_model::channel::message::component::{Button, ButtonStyle};
use twilight_model::channel::message::{AllowedMentions, MessageFlags};
use twilight_model::gateway::payload::outgoing::UpdatePresence;
use twilight_model::gateway::presence::{ActivityType, MinimalActivity, Status};
use twilight_model::gateway::Intents;
use twilight_model::id::Id;
use twilight_util::builder::InteractionResponseDataBuilder;
use zephyrus::framework::DefaultError;
use zephyrus::prelude::*;
use zephyrus::twilight_exports::{
    ActionRow, ApplicationMarker, Interaction, InteractionResponse, InteractionResponseType,
};

pub const MAX_MESSAGE_LEN: usize = 800;

#[derive(Deserialize, Debug)]
pub struct BotConfig {
    pub discord_token: String,
    pub trigger_chance: f64,
    pub api_key: String,
    pub app_id: String,
}

pub type GuardedBotContext = Mutex<BotContext>;
pub struct BotContext {
    pub http_client: ClientWithMiddleware,
    pub redis: redis::aio::Connection,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct InteractionValue {
    openai_response: String,
}

lazy_static! {
    static ref RNG: Arc<Mutex<SmallRng>> = Arc::new(Mutex::new(SmallRng::from_entropy()));
}

async fn make_openai_reqest(
    http: &ClientWithMiddleware,
    prompt: &str,
) -> Result<String, anyhow::Error> {
    let response = http
        .post(format!("{OPENAI_API_BASE_URL}/chat/completions"))
        .json(&OpenAIChatCompletionRequest {
            model: "gpt-3.5-turbo".to_owned(),
            max_tokens: None,
            messages: [ChatMessage {
                role: "user".to_owned(),
                content: prompt.to_owned(),
            }]
            .to_vec(),
        })
        .send()
        .await?
        .error_for_status()?
        .json::<OpenAIChatCompletionResponse>()
        .await?;

    Ok(response
        .choices
        .first()
        .context("no response")?
        .message
        .content
        .clone())
}

#[error_handler]
async fn handle_interaction_error(
    _ctx: &SlashContext<Arc<GuardedBotContext>>,
    error: DefaultError,
) {
    log::error!("error handling interaction: {:#?}", error);
}

#[command("chatgpt")]
#[description = "fucking ai"]
#[error_handler(handle_interaction_error)]
async fn handle_chatgpt_interaction(
    ctx: &SlashContext<Arc<GuardedBotContext>>,
    #[description = "say what"] prompt: String,
) -> DefaultCommandResult {
    let mut bot_ctx = ctx.data.lock().await;

    ctx.acknowledge().await?;
    let response = make_openai_reqest(&bot_ctx.http_client, &prompt).await?;
    let hash = hash::get_sha1(&response);

    if response.len() > MAX_MESSAGE_LEN {
        let redis = &mut bot_ctx.redis;
        redis
            .set(
                &hash,
                serde_json::to_string(&InteractionValue {
                    openai_response: response.clone(),
                })
                .context("could not serialize")?,
            )
            .await?;

        let chunk = format!("{}...", &response[..MAX_MESSAGE_LEN]);
        let button = Button {
            custom_id: Some(hash),
            disabled: false,
            emoji: None,
            label: Some("Click to see all".to_owned()),
            style: ButtonStyle::Primary,
            url: None,
        };

        let action_row = ActionRow {
            components: [button.into()].into(),
        };

        ctx.interaction_client
            .update_response(&ctx.interaction.token)
            .content(Some(&chunk))?
            .components(Some(&[action_row.into()]))?
            .await?;
    } else {
        ctx.interaction_client
            .update_response(&ctx.interaction.token)
            .content(Some(&response))?
            .await?;
    }

    Ok(())
}

async fn handle_message_button_press(
    interaction: Interaction,
    ctx: Arc<GuardedBotContext>,
    discord: Arc<DiscordHttpClient>,
) -> Result<(), anyhow::Error> {
    let redis = &mut ctx.lock().await.redis;
    let interaction_data = interaction.data.context("no interaction data")?;
    match interaction_data {
        zephyrus::twilight_exports::InteractionData::MessageComponent(m) => {
            let interaction_value = serde_json::from_str::<InteractionValue>(
                &redis.get::<String, String>(m.custom_id).await?,
            )?;

            let interaction_client = discord.interaction(interaction.application_id);

            let chunks = interaction_value
                .openai_response
                .chars()
                .collect::<Vec<char>>()
                .chunks(2000)
                .map(|c| c.iter().collect::<String>())
                .collect::<Vec<String>>();

            let mut chunks_iter = chunks.iter();

            if let Some(first_chunk) = chunks_iter.next() {
                let response_data = InteractionResponseDataBuilder::default()
                    .content(first_chunk)
                    .flags(MessageFlags::EPHEMERAL)
                    .build();

                interaction_client
                    .create_response(
                        interaction.id,
                        &interaction.token,
                        &InteractionResponse {
                            kind: InteractionResponseType::ChannelMessageWithSource,
                            data: Some(response_data),
                        },
                    )
                    .await?;

                for chunk in chunks_iter {
                    interaction_client
                        .create_followup(&interaction.token)
                        .content(chunk)?
                        .flags(MessageFlags::EPHEMERAL)
                        .await?;
                }
            }
        }
        _ => log::error!("this should not happen"),
    };

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    foundation::log::init_logger(log::LevelFilter::Info);

    let url = "redis://127.0.0.1/";
    let client = redis::Client::open(url)?;
    let redis = client.get_async_connection().await?;
    log::info!("connected to redis");

    let config = Config::builder()
        .add_source(File::new("config.json", FileFormat::Json))
        .add_source(Environment::with_prefix("REPLYBOT"))
        .build()?
        .try_deserialize::<BotConfig>()?;

    let mut shard = Shard::new(
        ShardId::ONE,
        config.discord_token.clone(),
        Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT,
    );

    let mut headers = HeaderMap::new();
    headers.insert(X_API_KEY_HEADER, config.api_key.parse()?);
    headers.insert("Content-Type", "application/json".parse()?);

    let discord_http = Arc::new(DiscordHttpClient::new(config.discord_token.to_owned()));
    let http_client = get_http_client_with_headers(headers, 30);
    let bot_context = Arc::new(Mutex::new(BotContext { http_client, redis }) as GuardedBotContext);

    let cache = InMemoryCache::builder()
        .resource_types(ResourceType::MESSAGE)
        .build();

    let config = Arc::new(config);
    while let Ok(event) = shard.next_event().await {
        cache.update(&event);
        match event {
            Event::Ready(_) => {
                log::info!("Connected on shard");

                let activity = MinimalActivity {
                    kind: ActivityType::Listening,
                    name: "THE BADDEST by K/DA".to_owned(),
                    url: None,
                }
                .into();

                let request = UpdatePresence::new([activity], false, None, Status::DoNotDisturb)?;
                let result = shard.command(&request).await;
                log::info!("presence update: {:?}", result);
            }

            _ => {
                tokio::spawn(
                    handle_event(
                        event,
                        Arc::clone(&discord_http),
                        Arc::clone(&bot_context),
                        Arc::clone(&config),
                    )
                    .then(|result| async {
                        match result {
                            Ok(_) => {}
                            Err(e) => log::error!("{}", e),
                        }
                    }),
                );
            }
        }
    }

    Ok(())
}

async fn handle_event(
    event: Event,
    discord: Arc<DiscordHttpClient>,
    ctx: Arc<GuardedBotContext>,
    config: Arc<BotConfig>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let app_id = Id::<ApplicationMarker>::new(config.app_id.parse()?);
    let framework = Arc::new(
        Framework::builder(discord.clone(), app_id, ctx.clone())
            .command(handle_chatgpt_interaction)
            .build(),
    );

    framework.register_global_commands().await?;

    match event {
        Event::MessageCreate(msg) if !msg.author.bot => {
            let mut rng = RNG.lock().await;
            if rng.gen_bool(config.trigger_chance) {
                log::info!("triggered reply for: {}", msg.author.id);
                discord.create_typing_trigger(msg.channel_id).await?;

                let ctx = ctx.lock().await;
                let response = make_openai_reqest(&ctx.http_client, &msg.content).await?;

                discord
                    .create_message(msg.channel_id)
                    .reply(msg.id)
                    .fail_if_not_exists(false)
                    .allowed_mentions(Some(&AllowedMentions::default()))
                    .content(&response)?
                    .await?;
            }
        }
        Event::InteractionCreate(i) => match i.kind {
            zephyrus::twilight_exports::InteractionType::ApplicationCommand => {
                let clone = Arc::clone(&framework);
                tokio::spawn(async move {
                    let inner = i.0;
                    clone.process(inner).await;
                });
            }
            zephyrus::twilight_exports::InteractionType::MessageComponent => {
                tokio::spawn(
                    handle_message_button_press(i.0, ctx, discord).then(|result| async {
                        match result {
                            Ok(_) => {}
                            Err(e) => log::error!("{}", e),
                        }
                    }),
                );
            }
            kind => log::info!("ignoring interaction type: {:?}", kind),
        },
        // Other events here...
        _ => {}
    }

    Ok(())
}
