use anyhow::{bail, Context};
use aws_sdk_dynamodb::model::AttributeValue;
use cached::proc_macro::io_cached;
use cached::AsyncRedisCache;
use foundation::aws;
use foundation::constants::{OPENAI_API_BASE_URL, X_API_KEY_HEADER};
use foundation::extensions::SecretsManagerExtensions;
use foundation::types::openai::{
    ChatMessage, OpenAIChatCompletionRequest, OpenAIChatCompletionResponse,
};
use foundation::util::get_uuid;
use futures::lock::Mutex;
use futures::FutureExt;
use lazy_static::lazy_static;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use redis::AsyncCommands;
use reqwest::ClientBuilder;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use std::{error::Error, sync::Arc};
use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::{Event, EventType, Shard, ShardId};
use twilight_http::Client as DiscordHttpClient;
use twilight_model::channel::message::component::{Button, ButtonStyle};
use twilight_model::channel::message::{AllowedMentions, MessageFlags};
use twilight_model::gateway::payload::outgoing::UpdatePresence;
use twilight_model::gateway::presence::{ActivityType, MinimalActivity, Status};
use twilight_model::gateway::Intents;
use twilight_util::builder::InteractionResponseDataBuilder;
use zephyrus::framework::DefaultError;
use zephyrus::prelude::*;
use zephyrus::twilight_exports::{
    ActionRow, Interaction, InteractionData, InteractionResponse, InteractionResponseType,
    InteractionType,
};

mod db {
    pub const HASH_KEY: &str = "hash";
    pub const INTERACTION_VALUE_KEY: &str = "interaction_value";
    pub const USER_SNOWFLAKE_KEY: &str = "discord_id";
    pub const RAW_RESPONSE_KEY: &str = "raw_response";
}

pub const CONFIG_INTERACTION_TABLE: &str = "ReplybotInteraction";
pub const CONFIG_APIM_API_KEY_ID: &str = "Replybot-ApimApiKey";
#[cfg(debug_assertions)]
pub const CONFIG_DISCORD_TOKEN_ID: &str = "Replybot-DiscordAuthToken-dev";
#[cfg(not(debug_assertions))]
pub const CONFIG_DISCORD_TOKEN_ID: &str = "Replybot-DiscordAuthToken";
pub const CONFIG_TRIGGER_CHANCE: f64 = 0.00;
pub const CONFIG_REDIS_CONNECTION: &str = "redis://replybot-cache/";
pub const CONFIG_REDIS_CACHE_KEY_PREFIX: &str = "REPLYBOT_";

pub const BUTTON_THRESHOLD: usize = 1000;
pub const MAX_DISCORD_MESSAGE_LEN: usize = 2000;

pub type GuardedBotContext = Mutex<BotContext>;
pub struct BotContext {
    pub http_client: ClientWithMiddleware,
    pub redis: redis::aio::Connection,
    pub secrets: aws_sdk_secretsmanager::Client,
    pub tables: aws_sdk_dynamodb::Client,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct InteractionValue {
    openai_response: String,
}

lazy_static! {
    static ref RNG: Arc<Mutex<SmallRng>> = Arc::new(Mutex::new(SmallRng::from_entropy()));
}

#[io_cached(
    map_error = r##"|e| anyhow::anyhow!("{:?}", e)"##,
    type = "AsyncRedisCache<String, String>",
    create = r##" {
        AsyncRedisCache::new("APIM_API_KEY", 3600)
            .set_namespace(CONFIG_REDIS_CACHE_KEY_PREFIX)
            .set_connection_string(CONFIG_REDIS_CONNECTION)
            .build()
            .await
            .expect("error building redis cache")
    } "##,
    convert = r#"{ "".to_string() }"#
)]
async fn get_api_key(secrets: &aws_sdk_secretsmanager::Client) -> Result<String, anyhow::Error> {
    secrets.get_secret(CONFIG_APIM_API_KEY_ID).await
}

async fn make_openai_reqest(
    http: &ClientWithMiddleware,
    secrets: &aws_sdk_secretsmanager::Client,
    prompt: &str,
) -> Result<OpenAIChatCompletionResponse, anyhow::Error> {
    let api_key = get_api_key(secrets).await?;

    let response = http
        .post(format!("{OPENAI_API_BASE_URL}/chat/completions"))
        .header(X_API_KEY_HEADER, api_key)
        .header("Content-Type", "application/json")
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

    Ok(response)
}

#[error_handler]
async fn handle_interaction_error(
    _ctx: &SlashContext<Arc<GuardedBotContext>>,
    error: DefaultError,
) {
    log::error!("error handling interaction: {:?}", error);
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
    let original_response =
        make_openai_reqest(&bot_ctx.http_client, &bot_ctx.secrets, &prompt).await?;
    let response = original_response
        .choices
        .first()
        .context("no response")?
        .message
        .content
        .clone();

    let hash = get_uuid();
    let interaction_value = &InteractionValue {
        openai_response: response.clone(),
    };

    let redis = &mut bot_ctx.redis;
    redis
        .set(
            format!("{}{}", CONFIG_REDIS_CACHE_KEY_PREFIX, &hash),
            serde_json::to_string(interaction_value).context("could not serialize")?,
        )
        .await?;

    bot_ctx
        .tables
        .put_item()
        .table_name(CONFIG_INTERACTION_TABLE)
        .item(db::HASH_KEY, AttributeValue::S(hash.clone()))
        .item(
            db::INTERACTION_VALUE_KEY,
            AttributeValue::M(serde_dynamo::to_item(interaction_value)?),
        )
        .item(
            db::USER_SNOWFLAKE_KEY,
            AttributeValue::S(
                ctx.interaction
                    .author_id()
                    .context("no user id")?
                    .to_string(),
            ),
        )
        .item(
            db::RAW_RESPONSE_KEY,
            AttributeValue::M(serde_dynamo::to_item(original_response)?),
        )
        .send()
        .await?;

    if response.len() > BUTTON_THRESHOLD {
        let chunk = format!(
            "{}...",
            response.chars().take(BUTTON_THRESHOLD).collect::<String>()
        );
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
    let interaction_data = interaction.data.context("no interaction data")?;
    let m = match interaction_data {
        InteractionData::MessageComponent(m) => m,
        _ => bail!("this should not happen"),
    };

    let interaction_value: InteractionValue = {
        let mut guard = ctx.lock().await;
        let redis = &mut guard.redis;

        match redis
            .get::<_, String>(format!("{}{}", CONFIG_REDIS_CACHE_KEY_PREFIX, &m.custom_id))
            .await
        {
            Ok(interaction_value) => serde_json::from_str(&interaction_value)?,
            Err(_) => {
                log::info!("cache miss for interaction: {}", &m.custom_id);
                let tables = &guard.tables;
                let response = tables
                    .get_item()
                    .table_name(CONFIG_INTERACTION_TABLE)
                    .key(db::HASH_KEY, AttributeValue::S(m.custom_id))
                    .send()
                    .await?;

                serde_dynamo::from_item(
                    response
                        .item()
                        .context("must find item in table")?
                        .get(db::INTERACTION_VALUE_KEY)
                        .context("must have key")?
                        .as_m()
                        .ok()
                        .context("must be a map")?
                        .clone(),
                )?
            }
        }
    };

    let interaction_client = discord.interaction(interaction.application_id);
    // julian said to split messages on code blocks.
    let chunks = interaction_value
        .openai_response
        .chars()
        .collect::<Vec<char>>()
        .chunks(MAX_DISCORD_MESSAGE_LEN)
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

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    foundation::log::init_logger(
        log::LevelFilter::Info,
        vec![
            "twilight_http_ratelimiting::in_memory::bucket",
            "twilight_gateway::shard",
        ],
    );

    let shared_config = aws::config::get_shared_config().await;
    let secrets = aws_sdk_secretsmanager::Client::new(&shared_config);
    let tables = aws_sdk_dynamodb::Client::new(&shared_config);

    let client = redis::Client::open(CONFIG_REDIS_CONNECTION)?;
    let redis = client.get_async_connection().await?;
    log::info!("connected to redis");

    let discord_token = secrets.get_secret(CONFIG_DISCORD_TOKEN_ID).await?;
    log::info!(
        "loaded discord token from secret: {}",
        CONFIG_DISCORD_TOKEN_ID
    );

    let mut shard = Shard::new(
        ShardId::ONE,
        discord_token.clone(),
        Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT | Intents::GUILDS,
    );

    let discord_http = Arc::new(DiscordHttpClient::new(discord_token.to_owned()));
    let http_client =
        foundation::http::get_default_middleware(ClientBuilder::new().build()?).build();
    let bot_context = Arc::new(Mutex::new(BotContext {
        http_client,
        redis,
        secrets,
        tables,
    }) as GuardedBotContext);

    let cache = InMemoryCache::builder()
        .resource_types(ResourceType::MESSAGE | ResourceType::GUILD)
        .build();

    while let Ok(event) = shard.next_event().await {
        cache.update(&event);
        if matches!(event.kind(), EventType::GatewayHeartbeatAck) {
            continue;
        }

        match event.guild_id() {
            Some(guild_id) => {
                let guild_name = match cache.guild(guild_id) {
                    Some(g) => g.name().to_owned(),
                    None => discord_http.guild(guild_id).await?.model().await?.name,
                };

                log::info!("event {:?} from server {:?}", event.kind(), guild_name);
            }
            None => {
                log::info!("event {:?}", event.kind());
            }
        }

        if matches!(event.kind(), EventType::Ready) {
            log::info!("connected on shard");

            let activity = MinimalActivity {
                kind: ActivityType::Listening,
                name: "THE BADDEST by K/DA".to_owned(),
                url: None,
            }
            .into();

            let request = UpdatePresence::new([activity], false, None, Status::DoNotDisturb)?;
            let result = shard.command(&request).await;
            log::info!("presence update: {:?}", result);

            continue;
        }

        tokio::spawn(
            handle_event(event, Arc::clone(&discord_http), Arc::clone(&bot_context)).then(
                |result| async {
                    match result {
                        Ok(_) => {}
                        Err(e) => log::error!("{}", e),
                    }
                },
            ),
        );
    }

    Ok(())
}

async fn handle_event(
    event: Event,
    discord: Arc<DiscordHttpClient>,
    ctx: Arc<GuardedBotContext>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let app_id = discord.current_user_application().await?.model().await?.id;
    let framework = Arc::new(
        Framework::builder(discord.clone(), app_id, ctx.clone())
            .command(handle_chatgpt_interaction)
            .build(),
    );

    framework.register_global_commands().await?;

    match event {
        Event::MessageCreate(msg) if !msg.author.bot => {
            let mut rng = RNG.lock().await;
            if rng.gen_bool(CONFIG_TRIGGER_CHANCE) {
                log::info!("triggered reply for: {}", msg.author.id);
                discord.create_typing_trigger(msg.channel_id).await?;

                let ctx = ctx.lock().await;
                let original_response =
                    make_openai_reqest(&ctx.http_client, &ctx.secrets, &msg.content).await?;
                let response = original_response
                    .choices
                    .first()
                    .context("no response")?
                    .message
                    .content
                    .clone();

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
            InteractionType::ApplicationCommand => {
                let clone = Arc::clone(&framework);
                let inner = i.0;
                clone.process(inner).await;
            }
            InteractionType::MessageComponent => {
                handle_message_button_press(i.0, ctx, discord).await?
            }
            kind => log::info!("ignoring interaction type: {:?}", kind),
        },
        // Other events here...
        _ => {}
    }

    Ok(())
}
