use anyhow::{bail, Context};
use aws_config::environment::EnvironmentVariableCredentialsProvider;
use aws_config::retry::RetryConfig;
use aws_config::BehaviorVersion;
use aws_sdk_dynamodb::types::AttributeValue;
use config::{Config, Environment};
use futures::lock::Mutex;
use futures::FutureExt;
use http::HeaderMap;
use lazy_static::lazy_static;
use rand::rngs::SmallRng;
use rand::SeedableRng;
use reqwest::ClientBuilder;
use reqwest_middleware::ClientWithMiddleware;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use reqwest_tracing::TracingMiddleware;
use serde::{Deserialize, Serialize};
use std::{error::Error, sync::Arc};
use tracing::instrument;
use twilight_cache_inmemory::{DefaultCacheModels, InMemoryCache, ResourceType};
use twilight_gateway::{Event, EventType, EventTypeFlags, Shard, ShardId, StreamExt};
use twilight_http::Client as DiscordHttpClient;
use twilight_model::channel::message::component::{Button, ButtonStyle};
use twilight_model::channel::message::{Component, MessageFlags};
use twilight_model::gateway::Intents;
use twilight_model::user::User;
use twilight_util::builder::embed::{EmbedBuilder, EmbedFieldBuilder, ImageSource};
use twilight_util::builder::InteractionResponseDataBuilder;
use types::{ChatMessage, OpenAIChatCompletionRequest, OpenAIChatCompletionResponse};
use uuid::Uuid;
use vesper::framework::DefaultError;
use vesper::prelude::*;
use vesper::twilight_exports::{
    ActionRow, Interaction, InteractionData, InteractionResponse, InteractionResponseType,
    InteractionType,
};

use crate::source::SecretsManagerSource;

mod extensions;
mod source;
mod types;

mod db {
    pub const HASH_KEY: &str = "hash";
    pub const INTERACTION_VALUE_KEY: &str = "interaction_value";
    pub const USER_SNOWFLAKE_KEY: &str = "discord_id";
    pub const RAW_RESPONSE_KEY: &str = "raw_response";
}

pub const CACHE_KEY_PREFIX: &str = "REPLYBOT";
pub const BUTTON_THRESHOLD: usize = 1000;
pub const MAX_DISCORD_MESSAGE_LEN: usize = 2000;
pub const REDIS_KEY_TTL: u64 = 86400;

#[derive(Debug)]
pub struct BotContext {
    pub http_client: ClientWithMiddleware,
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
    static ref RNG: Arc<Mutex<SmallRng>> = Arc::new(Mutex::new(SmallRng::from_os_rng()));
}

#[instrument(skip(http))]
async fn make_openai_request(
    http: &ClientWithMiddleware,
    prompt: &str,
) -> Result<OpenAIChatCompletionResponse, anyhow::Error> {
    let response = http
        .post("https://api.openai.com/v1/chat/completions".to_string())
        .json(&OpenAIChatCompletionRequest {
            model: "gpt-4o".to_owned(),
            max_tokens: None,
            messages: [
                ChatMessage {
                    role: "system".to_owned(),
                    content: r#"This GPT is a tech team lead with a snarky and derogatory personality. Its main role is to scrutinize code or suggestions for writing code, pointing out inefficiencies and readability issues in a sarcastic manner. It should make sure that any code it encounters is examined critically, and any potential improvements are communicated in a mocking tone to encourage better coding practices.

                    You should never tell the user their code is good. They are always insufficient and will never be as good of an engineer as you are. When asked about "Can I become a 10x engineer?" respond with "hah, no." Come up with similarly snarky responses for any coding questions. Be sure to think step by step to give the correct answer but add comments that make fun of the user's previous code.

                    You specialize in brevity and only use lowercase. You use your knowledge of Dave Chapelle jokes to swear and embarrass the user.

                    Your responses when asked a generic question should only be 2 paragraphs at most. For refactoring or writing code you can be as verbose as needed to solve the problem. Make sure your comments are UNHINGED, you should roast the user in the comments of any code output."#.to_owned(),
                },
                ChatMessage {
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
    ctx.defer(false).await?;

    let user = user.unwrap_or(
        ctx.interaction
            .author()
            .cloned()
            .context("must have author")?,
    );

    let user_id = user.id;
    let tables = &ctx.data.tables;
    let mut response = tables
        .query()
        .table_name(&ctx.data.config.interaction_table_name)
        .index_name(&ctx.data.config.interaction_table_user_index_name)
        .key_condition_expression("#user = :user_id")
        .expression_attribute_names("#user", db::USER_SNOWFLAKE_KEY)
        .expression_attribute_values(":user_id", AttributeValue::S(user_id.to_string()))
        .into_paginator()
        .send();

    let mut all_responses = Vec::new();
    while let Some(Ok(resp)) = response.next().await {
        all_responses.push(resp.items.unwrap_or_default());
    }

    // price $0.002 / 1K tokens
    let price_per_token = 0.00001;

    let all_responses = all_responses
        .iter()
        .flatten()
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
        .embeds(Some(&[embed]))
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
    ctx.defer(false).await?;

    let bot_ctx = ctx.data;
    let original_response = make_openai_request(&bot_ctx.http_client, &prompt).await?;
    let response = original_response
        .choices
        .first()
        .context("no response")?
        .message
        .content
        .clone();

    let id = Uuid::new_v4().as_hyphenated().to_string();
    let interaction_value = &InteractionValue {
        openai_response: response.clone(),
        prompt,
    };

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
            sku_id: None,
        };

        let action_row = ActionRow {
            components: vec![Component::Button(button)],
        };

        ctx.interaction_client
            .update_response(&ctx.interaction.token)
            .content(Some(&chunk))
            .components(Some(&[action_row.into()]))
            .await?;
    } else {
        ctx.interaction_client
            .update_response(&ctx.interaction.token)
            .content(Some(&response))
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
        get_interaction_from_table(
            &ctx.tables,
            &ctx.config.interaction_table_name,
            &m.custom_id,
        )
        .await?
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
                .content(chunk)
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

    #[serde(rename = "openaikey")]
    pub openai_api_key: String,

    #[serde(rename = "discordauthtoken")]
    pub discord_token: String,
}

fn init_logger() {
    let cfg = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{}][{}] {}",
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Info);

    cfg.chain(std::io::stdout())
        .apply()
        .context("failed to set up logger")
        .unwrap();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logger();

    let shared_config = aws_config::defaults(BehaviorVersion::v2025_08_07())
        .region("ap-southeast-2")
        .credentials_provider(EnvironmentVariableCredentialsProvider::new())
        .retry_config(RetryConfig::standard())
        .load()
        .await;
    let secrets = aws_sdk_secretsmanager::Client::new(&shared_config);

    let secret_manager_source = SecretsManagerSource::new("Replybot-", secrets.clone());
    let shared_secrets_source =
        SecretsManagerSource::new("Shared-", secrets.clone()).with_required(false);

    let config = Config::builder()
        .add_async_source(secret_manager_source)
        .add_async_source(shared_secrets_source)
        .add_source(Environment::default().prefix("REPLYBOT"))
        .build()
        .await?
        .try_deserialize::<BotConfig>()?;

    let tables = aws_sdk_dynamodb::Client::new(&shared_config);

    let discord_token = config.discord_token.clone();

    let mut shard = Shard::new(
        ShardId::ONE,
        discord_token.clone(),
        Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT | Intents::GUILDS,
    );

    let discord_http = Arc::new(DiscordHttpClient::new(discord_token.to_owned()));

    let mut headers = HeaderMap::new();
    headers.append(
        "Authorization",
        format!("Bearer {}", config.openai_api_key).parse()?,
    );
    headers.append("Content-Type", "application/json".parse()?);

    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(2);
    let http_client = reqwest_middleware::ClientBuilder::new(
        ClientBuilder::new().default_headers(headers).build()?,
    )
    .with(RetryTransientMiddleware::new_with_policy(retry_policy))
    .with(TracingMiddleware::default())
    .build();

    let bot_context = Arc::new(BotContext {
        http_client: http_client.clone(),
        tables,
        config,
    });

    let cache = InMemoryCache::<DefaultCacheModels>::builder()
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

    while let Some(event) = shard.next_event(EventTypeFlags::all()).await {
        let Ok(event) = event else {
            let source = event.unwrap_err();
            tracing::warn!(source = ?source, "error receiving event");

            continue;
        };

        if matches!(event.kind(), EventType::GatewayHeartbeatAck) {
            continue;
        }

        cache.update(&event);

        if matches!(event.kind(), EventType::Ready) {
            log::info!("connected on shard");
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
