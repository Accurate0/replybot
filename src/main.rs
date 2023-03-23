use anyhow::{bail, Context};
use aws_sdk_dynamodb::model::AttributeValue;
use config::{Config, Environment};
use foundation::aws;
use foundation::config::config_sources::SecretsManagerSource;
use foundation::constants::{OPENAI_API_BASE_URL, X_API_KEY_HEADER};
use foundation::types::openai::{
    ChatMessage, OpenAIChatCompletionRequest, OpenAIChatCompletionResponse,
};
use foundation::util::get_uuid;
use futures::lock::Mutex;
use futures::{Future, FutureExt};
use http::HeaderMap;
use lazy_static::lazy_static;
use rand::rngs::SmallRng;
use rand::SeedableRng;
use redis::AsyncCommands;
use reqwest::ClientBuilder;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::{error::Error, sync::Arc};
use tracing::instrument;
use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::{Event, EventType, Shard, ShardId};
use twilight_http::Client as DiscordHttpClient;
use twilight_model::channel::message::component::{Button, ButtonStyle};
use twilight_model::channel::message::MessageFlags;
use twilight_model::gateway::payload::outgoing::UpdatePresence;
use twilight_model::gateway::presence::{ActivityType, MinimalActivity, Status};
use twilight_model::gateway::Intents;
use twilight_model::user::User;
use twilight_util::builder::embed::{EmbedBuilder, EmbedFieldBuilder, ImageSource};
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

pub const BUTTON_THRESHOLD: usize = 1000;
pub const MAX_DISCORD_MESSAGE_LEN: usize = 2000;

#[derive(Debug)]
pub struct BotContext {
    pub http_client: ClientWithMiddleware,
    pub redis: Option<Mutex<redis::aio::Connection>>,
    pub tables: aws_sdk_dynamodb::Client,
    pub config: BotConfig,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct InteractionValue {
    openai_response: String,
    #[serde(default)]
    prompt: String,
}

lazy_static! {
    static ref RNG: Arc<Mutex<SmallRng>> = Arc::new(Mutex::new(SmallRng::from_entropy()));
}

#[instrument(skip(http))]
async fn make_openai_reqest(
    http: &ClientWithMiddleware,
    prompt: &str,
) -> Result<OpenAIChatCompletionResponse, anyhow::Error> {
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

    Ok(response)
}

#[error_handler]
async fn handle_interaction_error(_ctx: &SlashContext<Arc<BotContext>>, error: DefaultError) {
    log::error!("error handling interaction: {:?}", error);
}

#[instrument(skip(ctx))]
#[command("stats")]
#[description = "i wonder who costs the most (julian)"]
#[error_handler(handle_interaction_error)]
async fn handle_stats_interaction(
    ctx: &SlashContext<Arc<BotContext>>,
    #[description = "check julian"] user: Option<User>,
) -> DefaultCommandResult {
    ctx.acknowledge().await?;

    let user = user.unwrap_or(
        ctx.interaction
            .author()
            .cloned()
            .context("must have author")?,
    );

    let user_id = user.id;
    let tables = &ctx.data.tables;
    let response = tables
        .query()
        .table_name(&ctx.data.config.interaction_table_name)
        .index_name(&ctx.data.config.interaction_table_user_index_name)
        .key_condition_expression("#user = :user_id")
        .expression_attribute_names("#user", db::USER_SNOWFLAKE_KEY)
        .expression_attribute_values(":user_id", AttributeValue::S(user_id.to_string()))
        .send()
        .await?;

    // price $0.002 / 1K tokens
    let price_per_token = 2e-6_f64;

    let all_responses = response
        .items()
        .context("must have items")?
        .iter()
        .map(
            |item| -> Result<OpenAIChatCompletionResponse, anyhow::Error> {
                let raw_response = item.get(db::RAW_RESPONSE_KEY);
                match raw_response {
                    Some(raw_response) => {
                        if let Ok(raw_response) = raw_response.as_m() {
                            let resp = serde_dynamo::from_item(raw_response.clone())?;
                            Ok(resp)
                        } else {
                            bail!("non existent")
                        }
                    }
                    None => {
                        bail!("non existent")
                    }
                }
            },
        )
        .filter(|item| item.is_ok());

    let total_tokens = all_responses.fold(0i64, |accumulator, item| {
        // safe now, we filtered.
        let item = item.unwrap();
        let total_tokens = item.usage.total_tokens;

        accumulator + total_tokens
    }) as f64;

    let total_price = total_tokens * price_per_token;

    let colour = user.accent_color.unwrap_or(10830402);
    let embed = EmbedBuilder::new();
    let embed = if let Some(avatar) = user.avatar {
        let url = format!(
            "https://cdn.discordapp.com/avatars/{}/{}.png",
            user.id, avatar
        );
        embed.thumbnail(ImageSource::url(url)?)
    } else {
        embed
    };

    let embed = embed
        .color(colour)
        .field(EmbedFieldBuilder::new(
            "User",
            format!("{}#{}", user.name, user.discriminator),
        ))
        .field(EmbedFieldBuilder::new(
            "Price",
            format!("${:.4}", total_price),
        ))
        .field(EmbedFieldBuilder::new(
            "Total Tokens",
            total_tokens.to_string(),
        ))
        .build();

    ctx.interaction_client
        .update_response(&ctx.interaction.token)
        .embeds(Some(&[embed]))?
        .await?;

    Ok(())
}

#[instrument(skip(ctx))]
#[command("chatgpt")]
#[description = "fucking ai"]
#[error_handler(handle_interaction_error)]
async fn handle_chatgpt_interaction(
    ctx: &SlashContext<Arc<BotContext>>,
    #[description = "say what"] prompt: String,
) -> DefaultCommandResult {
    ctx.acknowledge().await?;

    let bot_ctx = ctx.data;
    let original_response = make_openai_reqest(&bot_ctx.http_client, &prompt).await?;
    let response = original_response
        .choices
        .first()
        .context("no response")?
        .message
        .content
        .clone();

    let id = get_uuid();
    let interaction_value = &InteractionValue {
        openai_response: response.clone(),
        prompt,
    };

    let update_redis = async {
        match &bot_ctx.redis {
            Some(redis) => {
                let id = id.clone();
                log::info!("setting key {} in redis", id);
                let redis = &mut redis.lock().await;

                redis
                    .set(
                        &id,
                        serde_json::to_string(interaction_value).context("could not serialize")?,
                    )
                    .await?;

                log::info!("[completed] setting key {} in redis", id);
            }
            None => {}
        }

        Ok::<(), anyhow::Error>(())
    };

    let update_table = async {
        log::info!("setting key {} in dynamo", id);
        bot_ctx
            .tables
            .put_item()
            .table_name(&ctx.data.config.interaction_table_name)
            .item(db::HASH_KEY, AttributeValue::S(id.clone()))
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
        log::info!("[completed] setting key {} in dynamo", id);
        Ok::<(), anyhow::Error>(())
    };

    #[allow(clippy::type_complexity)]
    let futures: [Pin<Box<dyn Future<Output = Result<(), anyhow::Error>> + Send>>; 2] =
        [Box::pin(update_redis), Box::pin(update_table)];

    let completed = futures::future::join_all(futures).await;
    for task in completed {
        task?
    }

    if response.len() > BUTTON_THRESHOLD {
        let chunk = format!(
            "{}...",
            response.chars().take(BUTTON_THRESHOLD).collect::<String>()
        );
        let button = Button {
            custom_id: Some(id),
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

async fn get_interaction_from_table(
    tables: &aws_sdk_dynamodb::Client,
    table_name: &str,
    key: &str,
) -> Result<InteractionValue, anyhow::Error> {
    log::info!("cache miss for interaction: {}", &key);
    let response = tables
        .get_item()
        .table_name(table_name)
        .key(db::HASH_KEY, AttributeValue::S(key.to_owned()))
        .send()
        .await?;

    Ok(serde_dynamo::from_item(
        response
            .item()
            .context("must find item in table")?
            .get(db::INTERACTION_VALUE_KEY)
            .context("must have key")?
            .as_m()
            .ok()
            .context("must be a map")?
            .clone(),
    )?)
}

#[instrument(skip_all)]
async fn handle_message_button_press(
    interaction: Interaction,
    ctx: Arc<BotContext>,
    discord: Arc<DiscordHttpClient>,
) -> Result<(), anyhow::Error> {
    let interaction_data = interaction.data.context("no interaction data")?;
    let m = match interaction_data {
        InteractionData::MessageComponent(m) => m,
        _ => bail!("this should not happen"),
    };

    let interaction_value: InteractionValue = {
        match &ctx.redis {
            Some(redis) => {
                let redis = &mut redis.lock().await;

                match redis.get::<_, String>(&m.custom_id).await {
                    Ok(interaction_value) => serde_json::from_str(&interaction_value)?,
                    Err(_) => {
                        get_interaction_from_table(
                            &ctx.tables,
                            &ctx.config.interaction_table_name,
                            &m.custom_id,
                        )
                        .await?
                    }
                }
            }
            None => {
                get_interaction_from_table(
                    &ctx.tables,
                    &ctx.config.interaction_table_name,
                    &m.custom_id,
                )
                .await?
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

#[derive(Deserialize, Debug)]
pub struct BotConfig {
    pub interaction_table_name: String,
    pub interaction_table_user_index_name: String,
    pub redis_connection_string: String,

    #[serde(rename = "ApimApiKey")]
    pub apim_api_key: String,
    #[cfg(debug_assertions)]
    #[serde(rename = "DiscordAuthToken-dev")]
    pub discord_token: String,

    #[cfg(not(debug_assertions))]
    #[serde(rename = "DiscordAuthToken")]
    pub discord_token: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    foundation::log::init_logger(
        log::LevelFilter::Info,
        &[
            "twilight_http_ratelimiting::in_memory::bucket",
            "twilight_gateway::shard",
        ],
    );

    let shared_config = aws::config::get_shared_config().await;
    let secrets = aws_sdk_secretsmanager::Client::new(&shared_config);

    let secret_manager_source = SecretsManagerSource::new("Replybot-".to_owned(), secrets);
    let config = Config::builder()
        .add_async_source(secret_manager_source)
        .add_source(Environment::default().prefix("REPLYBOT"))
        .build()
        .await?
        .try_deserialize::<BotConfig>()?;

    let tables = aws_sdk_dynamodb::Client::new(&shared_config);

    let client = redis::Client::open(config.redis_connection_string.clone())?;
    let redis = match client.get_async_connection().await {
        Ok(redis) => Some(Mutex::new(redis)),
        Err(_) => None,
    };
    log::info!("connected to redis: {}", redis.is_some());

    let discord_token = config.discord_token.clone();
    let api_key = config.apim_api_key.clone();

    let mut shard = Shard::new(
        ShardId::ONE,
        discord_token.clone(),
        Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT | Intents::GUILDS,
    );

    let discord_http = Arc::new(DiscordHttpClient::new(discord_token.to_owned()));

    let mut headers = HeaderMap::new();
    headers.append(X_API_KEY_HEADER, api_key.parse()?);
    headers.append("Content-Type", "application/json".parse()?);

    let http_client = foundation::http::get_default_middleware(
        ClientBuilder::new().default_headers(headers).build()?,
    )
    .build();

    let bot_context = Arc::new(BotContext {
        http_client,
        redis,
        tables,
        config,
    });

    let cache = InMemoryCache::builder()
        .resource_types(ResourceType::MESSAGE | ResourceType::GUILD)
        .build();

    let app_id = discord_http
        .current_user_application()
        .await?
        .model()
        .await?
        .id;

    let framework = Arc::new(
        Framework::builder(discord_http.clone(), app_id, bot_context.clone())
            .command(handle_chatgpt_interaction)
            .command(handle_stats_interaction)
            .build(),
    );

    if let Err(e) = framework.register_global_commands().await {
        log::error!("error registering commands: {}", e);
    };

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

            let request = UpdatePresence::new([activity], false, None, Status::Online)?;
            let result = shard.command(&request).await;
            log::info!("presence update: {:?}", result);

            continue;
        }

        if matches!(
            event.kind(),
            EventType::MessageCreate | EventType::MessageUpdate
        ) {
            continue;
        }

        tokio::spawn(
            handle_event(
                event,
                Arc::clone(&discord_http),
                Arc::clone(&bot_context),
                Arc::clone(&framework),
            )
            .then(|result| async {
                match result {
                    Ok(_) => {}
                    Err(e) => log::error!("{}", e),
                }
            }),
        );
    }

    Ok(())
}

#[instrument(skip_all)]
async fn handle_event(
    event: Event,
    discord: Arc<DiscordHttpClient>,
    ctx: Arc<BotContext>,
    framework: Arc<Framework<Arc<BotContext>>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if let Event::InteractionCreate(i) = event {
        match i.kind {
            InteractionType::ApplicationCommand => {
                let inner = i.0;
                framework.process(inner).await;
            }
            InteractionType::MessageComponent => {
                handle_message_button_press(i.0, ctx, discord).await?
            }
            kind => log::info!("ignoring interaction type: {:?}", kind),
        }
    }

    Ok(())
}
