use anyhow::Context;
use config::{Config, Environment, File, FileFormat};
use foundation::constants::{OPENAI_API_BASE_URL, X_API_KEY_HEADER};
use foundation::http::get_http_client_with_headers;
use foundation::types::openai::{
    ChatMessage, OpenAIChatCompletionRequest, OpenAIChatCompletionResponse,
};
use futures::lock::Mutex;
use futures::stream::StreamExt;
use futures::FutureExt;
use http::HeaderMap;
use lazy_static::lazy_static;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use reqwest_middleware::ClientWithMiddleware;
use serde::Deserialize;
use std::{error::Error, sync::Arc};
use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::{Cluster, Event};
use twilight_http::Client as HttpClient;
use twilight_model::channel::message::allowed_mentions::AllowedMentionsBuilder;
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
            messages: [ChatMessage {
                role: "user".to_owned(),
                content: prompt.to_owned(),
            }]
            .to_vec(),
        })
        .send()
        .await?
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
    let response = make_openai_reqest(ctx.data, &prompt).await?;

    ctx.interaction_client
        .update_response(&ctx.interaction.token)
        .content(Some(&response))?
        .await?;

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

    let (cluster, mut events) = Cluster::new(
        config.discord_token.to_owned(),
        Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT,
    )
    .await?;
    let cluster = Arc::new(cluster);

    let cluster_spawn = Arc::clone(&cluster);

    tokio::spawn(async move {
        cluster_spawn.up().await;
    });

    let mut headers = HeaderMap::new();
    headers.insert(X_API_KEY_HEADER, config.api_key.parse()?);
    headers.insert("Content-Type", "application/json".parse()?);

    let discord_http = Arc::new(HttpClient::new(config.discord_token.to_owned()));
    let http_client = Arc::new(get_http_client_with_headers(headers));

    let cache = InMemoryCache::builder()
        .resource_types(ResourceType::MESSAGE)
        .build();

    let config = Arc::new(config);
    while let Some((shard_id, event)) = events.next().await {
        cache.update(&event);
        match event {
            Event::ShardConnected(_) => {
                log::info!("Connected on shard {shard_id}");

                let activity = Activity::from(MinimalActivity {
                    kind: ActivityType::Custom,
                    name: "looking for...".to_owned(),
                    url: None,
                });
                let request =
                    UpdatePresence::new(Vec::from([activity]), false, None, Status::Online)?;
                let result = cluster.command(shard_id, &request).await;
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
                    .allowed_mentions(Some(&AllowedMentionsBuilder::default().build()))
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
