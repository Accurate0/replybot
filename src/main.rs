use anyhow::Context;
use config::{Config, Environment, File, FileFormat};
use foundation::constants::{OPENAI_API_BASE_URL, X_API_KEY_HEADER};
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
use reqwest_middleware::ClientWithMiddleware;
use serde::Deserialize;
use std::{error::Error, sync::Arc};
use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::{Event, Shard, ShardId};
use twilight_http::Client as HttpClient;
use twilight_model::channel::message::AllowedMentions;
use twilight_model::gateway::payload::outgoing::UpdatePresence;
use twilight_model::gateway::presence::{Activity, ActivityType, MinimalActivity, Status};
use twilight_model::gateway::Intents;
use twilight_model::id::Id;
use zephyrus::prelude::*;
use zephyrus::twilight_exports::ApplicationMarker;

#[derive(Deserialize, Debug)]
pub struct BotConfig {
    pub discord_token: String,
    pub trigger_chance: f64,
    pub api_key: String,
    pub app_id: String,
}

lazy_static! {
    static ref RNG: Arc<Mutex<SmallRng>> = Arc::new(Mutex::new(SmallRng::from_entropy()));
}

async fn make_openai_reqest(
    http: &Arc<ClientWithMiddleware>,
    prompt: &str,
) -> Result<String, anyhow::Error> {
    let response = http
        .post(format!("{OPENAI_API_BASE_URL}/chat/completions"))
        .json(&OpenAIChatCompletionRequest {
            model: "gpt-3.5-turbo".to_owned(),
            max_tokens: Some(400),
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

#[command("chatgpt")]
#[description = "fucking ai"]
async fn handle_chatgpt_interaction(
    ctx: &SlashContext<Arc<ClientWithMiddleware>>,
    #[description = "say what"] prompt: String,
) -> DefaultCommandResult {
    ctx.acknowledge().await?;
    let response = make_openai_reqest(ctx.data, &prompt).await;
    match response {
        Ok(response) => {
            let response = ctx
                .interaction_client
                .update_response(&ctx.interaction.token)
                .content(Some(&response));

            match response {
                Ok(response) => {
                    if let Err(e) = response.await {
                        log::error!("error sending response: {:#?}", e);
                    };
                }
                Err(e) => log::error!("error in response: {:#?}", e),
            };
        }
        Err(e) => log::error!("error handling interaction: {:#?}", e),
    };

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    foundation::log::init_logger(log::LevelFilter::Info);
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

    let discord_http = Arc::new(HttpClient::new(config.discord_token.to_owned()));
    let http_client = Arc::new(get_http_client_with_headers(headers, 30));

    let cache = InMemoryCache::builder()
        .resource_types(ResourceType::MESSAGE)
        .build();

    let config = Arc::new(config);
    while let Ok(event) = shard.next_event().await {
        cache.update(&event);
        match event {
            Event::Ready(_) => {
                log::info!("Connected on shard");

                let activity = Activity::from(MinimalActivity {
                    kind: ActivityType::Listening,
                    name: "THE BADDEST by K/DA".to_owned(),
                    url: None,
                });
                let request = UpdatePresence::new([activity], false, None, Status::DoNotDisturb)?;
                let result = shard.command(&request).await;
                log::info!("presence update: {:?}", result);
            }

            _ => {
                tokio::spawn(
                    handle_event(
                        event,
                        Arc::clone(&discord_http),
                        Arc::clone(&http_client),
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
    discord: Arc<HttpClient>,
    http: Arc<ClientWithMiddleware>,
    config: Arc<BotConfig>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let app_id = Id::<ApplicationMarker>::new(config.app_id.parse()?);
    let framework = Arc::new(
        Framework::builder(discord.clone(), app_id, http.clone())
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

                let response = make_openai_reqest(&http, &msg.content).await?;

                discord
                    .create_message(msg.channel_id)
                    .reply(msg.id)
                    .fail_if_not_exists(false)
                    .allowed_mentions(Some(&AllowedMentions::default()))
                    .content(&response)?
                    .await?;
            }
        }
        Event::InteractionCreate(i) => {
            let clone = Arc::clone(&framework);
            tokio::spawn(async move {
                let inner = i.0;
                clone.process(inner).await;
            });
        }
        // Other events here...
        _ => {}
    }

    Ok(())
}
